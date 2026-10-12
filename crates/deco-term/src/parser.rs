//! A parser for the byte stream a program writes to a terminal.
//!
//! The states and transitions follow the DEC-compatible parser described by
//! Paul Flo Williams at <https://vt100.net/emu/dec_ansi_parser>, which is the
//! model xterm and most terminal emulators implement. The parser only splits
//! the stream into actions: printable characters, control functions, escape
//! sequences, control sequences (CSI) and operating system commands (OSC).
//! What each action does to a screen is decided by the [`Perform`]
//! implementation it is given.
//!
//! Text is decoded as UTF-8. Because of that, the 8-bit C1 controls
//! (`0x80`-`0x9f`) are not recognised: those bytes are UTF-8 continuation
//! bytes, as in xterm's UTF-8 mode. Device control strings (DCS) and the
//! SOS, PM and APC strings are consumed and ignored.

/// The most parameters a control sequence keeps. Later ones are dropped and
/// the sequence is marked as ignored, as in the reference parser.
pub const MAX_PARAMS: usize = 32;

/// The most intermediate bytes a sequence keeps.
const MAX_INTERMEDIATES: usize = 2;

/// The longest operating system command kept, in bytes. A longer one is cut
/// at this length rather than growing without bound.
const MAX_OSC: usize = 4096;

/// What a parsed action does, supplied by the screen.
pub trait Perform {
    /// A printable character.
    fn print(&mut self, c: char);
    /// A C0 control character, such as line feed or backspace.
    fn execute(&mut self, byte: u8);
    /// A control sequence: `ESC [`, parameters, intermediates, final byte.
    ///
    /// `ignore` is true when the sequence had more parameters or
    /// intermediates than the parser keeps.
    fn csi(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: u8);
    /// An escape sequence: `ESC`, intermediates, final byte.
    fn esc(&mut self, intermediates: &[u8], ignore: bool, byte: u8);
    /// An operating system command: `ESC ]`, fields separated by `;`,
    /// terminated by BEL or `ESC \`.
    fn osc(&mut self, fields: &[&[u8]]);
}

/// The numeric parameters of a control sequence.
///
/// Parameters are separated by `;`. A parameter can carry sub-parameters
/// separated by `:`, as in `38:2::255:0:0`, which [`Params::groups`] returns
/// together. A missing value is `0`, which each control function interprets
/// as its default.
#[derive(Debug, Clone)]
pub struct Params {
    values: [u16; MAX_PARAMS],
    /// Whether the value at the same index starts a new parameter (`;` or
    /// the first value) rather than continuing one (`:`).
    starts: [bool; MAX_PARAMS],
    len: usize,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            values: [0; MAX_PARAMS],
            starts: [false; MAX_PARAMS],
            len: 0,
        }
    }
}

impl Params {
    /// Each parameter with its sub-parameters.
    pub fn groups(&self) -> impl Iterator<Item = &[u16]> + '_ {
        let mut start = 0;
        std::iter::from_fn(move || {
            if start >= self.len {
                return None;
            }
            let mut end = start + 1;
            while end < self.len && !self.starts[end] {
                end += 1;
            }
            let group = &self.values[start..end];
            start = end;
            Some(group)
        })
    }

    /// The first value of the parameter at `index`, or `0` when absent.
    pub fn get(&self, index: usize) -> u16 {
        self.groups()
            .nth(index)
            .and_then(|group| group.first().copied())
            .unwrap_or(0)
    }

    /// Like [`Params::get`], with `0` read as `1`, which is the default of
    /// every count and position parameter.
    pub fn count(&self, index: usize) -> u16 {
        self.get(index).max(1)
    }

    /// How many parameters there are, not counting sub-parameters.
    pub fn len(&self) -> usize {
        self.groups().count()
    }

    /// Whether the sequence had no parameters.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    /// Starts a new value. Returns false when there is no room left.
    fn push(&mut self, starts: bool) -> bool {
        if self.len == MAX_PARAMS {
            return false;
        }
        self.values[self.len] = 0;
        self.starts[self.len] = starts;
        self.len += 1;
        true
    }

    /// Appends a decimal digit to the last value, saturating at `u16::MAX`.
    fn digit(&mut self, digit: u8) {
        if let Some(value) = self.values[..self.len].last_mut() {
            *value = value
                .saturating_mul(10)
                .saturating_add(u16::from(digit - b'0'));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
    /// DCS, SOS, PM and APC: everything up to the string terminator is
    /// dropped.
    IgnoredString,
}

/// The parser's state between calls to [`Parser::advance`].
///
/// A sequence split across two reads is completed by the second, so a caller
/// can pass bytes as they arrive from a pipe.
#[derive(Debug, Clone)]
pub struct Parser {
    state: State,
    params: Params,
    /// Whether a value has been started for the current parameter, so that
    /// `;;` yields an empty parameter and a trailing `;` one more.
    param_started: bool,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediate_len: usize,
    ignoring: bool,
    osc: Vec<u8>,
    utf8: Utf8,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            state: State::Ground,
            params: Params::default(),
            param_started: false,
            intermediates: [0; MAX_INTERMEDIATES],
            intermediate_len: 0,
            ignoring: false,
            osc: Vec::new(),
            utf8: Utf8::default(),
        }
    }
}

impl Parser {
    /// A parser in its initial state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses `bytes`, calling `perform` for each complete action.
    pub fn advance(&mut self, perform: &mut impl Perform, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(perform, byte);
        }
    }

    fn byte(&mut self, perform: &mut impl Perform, byte: u8) {
        // A multi-byte character in progress. Only text in the ground state
        // and OSC strings are decoded; elsewhere a non-ASCII byte is ignored.
        if self.state == State::Ground && (byte >= 0x80 || self.utf8.pending()) {
            match self.utf8.push(byte) {
                Decoded::Char(c) => perform.print(c),
                Decoded::Pending => {}
                // The byte ended an invalid sequence; the byte itself is
                // read again as the start of whatever follows.
                Decoded::Invalid { reread } => {
                    perform.print(char::REPLACEMENT_CHARACTER);
                    if reread {
                        self.byte(perform, byte);
                    }
                }
            }
            return;
        }

        // Transitions that apply in every state.
        match byte {
            0x18 | 0x1a => {
                perform.execute(byte);
                self.state = State::Ground;
                return;
            }
            0x1b => {
                if self.state == State::OscString {
                    self.dispatch_osc(perform);
                }
                self.enter_escape();
                return;
            }
            _ => {}
        }

        match self.state {
            State::Ground => match byte {
                0x00..=0x1f => perform.execute(byte),
                // DEL is ignored.
                0x7f => {}
                _ => perform.print(char::from(byte)),
            },
            State::Escape => match byte {
                0x00..=0x1f => perform.execute(byte),
                0x20..=0x2f => {
                    self.collect(byte);
                    self.state = State::EscapeIntermediate;
                }
                b'[' => self.enter_csi(),
                b']' => {
                    self.osc.clear();
                    self.state = State::OscString;
                }
                b'P' | b'X' | b'^' | b'_' => self.state = State::IgnoredString,
                0x7f => {}
                0x30..=0x7e => {
                    perform.esc(&[], false, byte);
                    self.state = State::Ground;
                }
                _ => {}
            },
            State::EscapeIntermediate => match byte {
                0x00..=0x1f => perform.execute(byte),
                0x20..=0x2f => self.collect(byte),
                0x30..=0x7e => {
                    perform.esc(self.intermediates(), self.ignoring, byte);
                    self.state = State::Ground;
                }
                _ => {}
            },
            State::CsiEntry | State::CsiParam => match byte {
                0x00..=0x1f => perform.execute(byte),
                b'0'..=b'9' => {
                    self.start_param();
                    self.params.digit(byte);
                    self.state = State::CsiParam;
                }
                b';' | b':' => {
                    self.start_param();
                    // The separator ends this value and starts the next.
                    if !self.params.push(byte == b';') {
                        self.ignoring = true;
                    }
                    self.state = State::CsiParam;
                }
                // A private marker is only allowed before any parameter.
                0x3c..=0x3f if self.state == State::CsiEntry => {
                    self.collect(byte);
                    self.state = State::CsiParam;
                }
                0x3c..=0x3f => self.state = State::CsiIgnore,
                0x20..=0x2f => {
                    self.collect(byte);
                    self.state = State::CsiIntermediate;
                }
                0x40..=0x7e => self.dispatch_csi(perform, byte),
                _ => {}
            },
            State::CsiIntermediate => match byte {
                0x00..=0x1f => perform.execute(byte),
                0x20..=0x2f => self.collect(byte),
                0x30..=0x3f => self.state = State::CsiIgnore,
                0x40..=0x7e => self.dispatch_csi(perform, byte),
                _ => {}
            },
            State::CsiIgnore => match byte {
                0x00..=0x1f => perform.execute(byte),
                0x40..=0x7e => self.state = State::Ground,
                _ => {}
            },
            State::OscString => match byte {
                0x07 => {
                    self.dispatch_osc(perform);
                    self.state = State::Ground;
                }
                0x00..=0x1f => {}
                _ => {
                    if self.osc.len() < MAX_OSC {
                        self.osc.push(byte);
                    }
                }
            },
            State::IgnoredString => {}
        }
    }

    fn enter_escape(&mut self) {
        self.intermediate_len = 0;
        self.ignoring = false;
        self.state = State::Escape;
    }

    fn enter_csi(&mut self) {
        self.params.clear();
        self.param_started = false;
        self.intermediate_len = 0;
        self.ignoring = false;
        self.state = State::CsiEntry;
    }

    /// Makes sure the parameter being written has a value to write to.
    fn start_param(&mut self) {
        if !self.param_started {
            self.param_started = true;
            if !self.params.push(true) {
                self.ignoring = true;
            }
        }
    }

    fn collect(&mut self, byte: u8) {
        if self.intermediate_len == MAX_INTERMEDIATES {
            self.ignoring = true;
        } else {
            self.intermediates[self.intermediate_len] = byte;
            self.intermediate_len += 1;
        }
    }

    fn intermediates(&self) -> &[u8] {
        &self.intermediates[..self.intermediate_len]
    }

    fn dispatch_csi(&mut self, perform: &mut impl Perform, byte: u8) {
        perform.csi(&self.params, self.intermediates(), self.ignoring, byte);
        self.state = State::Ground;
    }

    fn dispatch_osc(&mut self, perform: &mut impl Perform) {
        let fields: Vec<&[u8]> = self.osc.split(|&byte| byte == b';').collect();
        perform.osc(&fields);
        self.osc.clear();
    }
}

/// An incremental UTF-8 decoder.
#[derive(Debug, Clone, Copy, Default)]
struct Utf8 {
    value: u32,
    /// Continuation bytes still expected.
    remaining: u8,
    /// The smallest value the sequence may encode, to reject overlong forms.
    minimum: u32,
}

enum Decoded {
    Char(char),
    Pending,
    /// The sequence was invalid. `reread` is true when the byte that showed
    /// it was not a continuation byte and must be parsed on its own.
    Invalid {
        reread: bool,
    },
}

impl Utf8 {
    fn pending(&self) -> bool {
        self.remaining > 0
    }

    fn push(&mut self, byte: u8) -> Decoded {
        if self.remaining > 0 {
            if byte & 0xc0 != 0x80 {
                *self = Self::default();
                return Decoded::Invalid { reread: true };
            }
            self.value = (self.value << 6) | u32::from(byte & 0x3f);
            self.remaining -= 1;
            if self.remaining > 0 {
                return Decoded::Pending;
            }
            let (value, minimum) = (self.value, self.minimum);
            *self = Self::default();
            return match char::from_u32(value) {
                Some(c) if value >= minimum => Decoded::Char(c),
                _ => Decoded::Invalid { reread: false },
            };
        }
        let (remaining, value, minimum) = match byte {
            0x00..=0x7f => return Decoded::Char(char::from(byte)),
            0xc2..=0xdf => (1, byte & 0x1f, 0x80),
            0xe0..=0xef => (2, byte & 0x0f, 0x800),
            0xf0..=0xf4 => (3, byte & 0x07, 0x10000),
            _ => return Decoded::Invalid { reread: false },
        };
        *self = Self {
            value: u32::from(value),
            remaining,
            minimum,
        };
        Decoded::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every action as text, for comparing against expectations.
    #[derive(Default)]
    struct Log(Vec<String>);

    impl Perform for Log {
        fn print(&mut self, c: char) {
            match self.0.last_mut() {
                Some(last) if last.starts_with("print ") => last.push(c),
                _ => self.0.push(format!("print {c}")),
            }
        }

        fn execute(&mut self, byte: u8) {
            self.0.push(format!("execute {byte:#04x}"));
        }

        fn csi(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: u8) {
            let groups: Vec<String> = params
                .groups()
                .map(|group| {
                    group
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(":")
                })
                .collect();
            self.0.push(format!(
                "csi {}{} {}{}",
                String::from_utf8_lossy(intermediates),
                groups.join(";"),
                char::from(action),
                if ignore { " ignored" } else { "" }
            ));
        }

        fn esc(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
            self.0.push(format!(
                "esc {}{}",
                String::from_utf8_lossy(intermediates),
                char::from(byte)
            ));
        }

        fn osc(&mut self, fields: &[&[u8]]) {
            let fields: Vec<String> = fields
                .iter()
                .map(|field| String::from_utf8_lossy(field).into_owned())
                .collect();
            self.0.push(format!("osc {}", fields.join("|")));
        }
    }

    fn parse(bytes: &[u8]) -> Vec<String> {
        let mut log = Log::default();
        Parser::new().advance(&mut log, bytes);
        log.0
    }

    #[test]
    fn text_and_controls_are_split() {
        assert_eq!(
            parse(b"ab\r\nc"),
            ["print ab", "execute 0x0d", "execute 0x0a", "print c"]
        );
    }

    #[test]
    fn control_sequences_carry_parameters_markers_and_intermediates() {
        assert_eq!(parse(b"\x1b[1;31m"), ["csi 1;31 m"]);
        assert_eq!(parse(b"\x1b[?1049h"), ["csi ?1049 h"]);
        assert_eq!(parse(b"\x1b[2 q"), ["csi  2 q"]);
        assert_eq!(parse(b"\x1b[m"), ["csi  m"]);
        // Empty parameters are kept, so `;5H` is row default, column 5.
        assert_eq!(parse(b"\x1b[;5H"), ["csi 0;5 H"]);
        assert_eq!(parse(b"\x1b[1;H"), ["csi 1;0 H"]);
    }

    #[test]
    fn colons_make_sub_parameters() {
        assert_eq!(parse(b"\x1b[38:2::255:0:0m"), ["csi 38:2:0:255:0:0 m"]);
        let mut params = Params::default();
        params.push(true);
        params.digit(b'4');
        params.push(false);
        params.digit(b'3');
        params.push(true);
        params.digit(b'7');
        let groups: Vec<&[u16]> = params.groups().collect();
        assert_eq!(groups, [&[4, 3][..], &[7][..]]);
        assert_eq!((params.get(1), params.count(5)), (7, 1));
    }

    #[test]
    fn a_sequence_split_across_reads_is_completed() {
        let mut log = Log::default();
        let mut parser = Parser::new();
        for part in [
            &b"\x1b"[..],
            b"[3",
            b"1",
            b"m\xc3\xa9",
            b"\xe2\x82",
            b"\xac",
        ] {
            parser.advance(&mut log, part);
        }
        assert_eq!(log.0, ["csi 31 m", "print é€"]);
    }

    #[test]
    fn osc_ends_with_bell_or_string_terminator() {
        assert_eq!(parse(b"\x1b]0;title\x07x"), ["osc 0|title", "print x"]);
        assert_eq!(
            parse("\x1b]2;tïtle\x1b\\x".as_bytes()),
            ["osc 2|tïtle", "esc \\", "print x"]
        );
    }

    #[test]
    fn device_control_strings_are_dropped() {
        assert_eq!(parse(b"\x1bPq#0;2;0;0;0\x1b\\a"), ["esc \\", "print a"]);
    }

    #[test]
    fn invalid_utf8_becomes_a_replacement_character() {
        assert_eq!(parse(b"a\xffb"), ["print a\u{fffd}b"]);
        // A lead byte followed by ASCII: the ASCII byte is kept.
        assert_eq!(parse(b"\xe2(x"), ["print \u{fffd}(x"]);
        // An overlong encoding of `/`.
        assert_eq!(parse(b"\xc0\xaf"), ["print \u{fffd}\u{fffd}"]);
        // An escape interrupts a character and still starts a sequence.
        assert_eq!(parse(b"\xe2\x1b[m"), ["print \u{fffd}", "csi  m"]);
    }

    #[test]
    fn too_many_parameters_mark_the_sequence_ignored() {
        let long = format!("\x1b[{}m", vec!["1"; MAX_PARAMS + 1].join(";"));
        let log = parse(long.as_bytes());
        assert!(log[0].ends_with(" ignored"), "{log:?}");
        // A huge value saturates rather than overflowing.
        assert_eq!(parse(b"\x1b[99999999A"), ["csi 65535 A"]);
    }

    #[test]
    fn cancel_aborts_a_sequence() {
        assert_eq!(parse(b"\x1b[3\x18m"), ["execute 0x18", "print m"]);
    }
}
