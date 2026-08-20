//! Key chords in VS Code's `keybindings.json` spelling.

use std::fmt;

/// The four modifier keys a binding can require.
///
/// `meta` is Command on macOS, the Windows key on Windows and Super on Linux;
/// `keybindings.json` spells it `cmd`, `win`, `super` or `meta` interchangeably
/// and all four parse to the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Modifiers {
    /// Control.
    pub ctrl: bool,
    /// Shift.
    pub shift: bool,
    /// Alt / Option.
    pub alt: bool,
    /// Command / Windows / Super.
    pub meta: bool,
}

impl Modifiers {
    /// No modifiers held.
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        meta: false,
    };

    /// Only Control.
    pub const CTRL: Modifiers = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };
    /// Only Shift.
    pub const SHIFT: Modifiers = Modifiers {
        shift: true,
        ..Modifiers::NONE
    };
    /// Only Alt.
    pub const ALT: Modifiers = Modifiers {
        alt: true,
        ..Modifiers::NONE
    };
    /// Only Meta.
    pub const META: Modifiers = Modifiers {
        meta: true,
        ..Modifiers::NONE
    };

    /// Whether no modifier is held.
    pub fn is_empty(&self) -> bool {
        *self == Modifiers::NONE
    }

    /// Combines two sets.
    pub fn union(self, other: Modifiers) -> Modifiers {
        Modifiers {
            ctrl: self.ctrl || other.ctrl,
            shift: self.shift || other.shift,
            alt: self.alt || other.alt,
            meta: self.meta || other.meta,
        }
    }
}

/// A key with a name rather than a printable character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NamedKey {
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Enter / Return.
    Enter,
    /// Escape.
    Escape,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// End.
    End,
    /// Home.
    Home,
    /// Left arrow.
    Left,
    /// Up arrow.
    Up,
    /// Right arrow.
    Right,
    /// Down arrow.
    Down,
    /// Insert.
    Insert,
    /// Delete (forward delete).
    Delete,
    /// Pause / Break.
    PauseBreak,
    /// Caps Lock.
    CapsLock,
    /// Scroll Lock.
    ScrollLock,
    /// Num Lock.
    NumLock,
    /// Context menu key.
    ContextMenu,
    /// Function key `F(n)`, 1-19 as VS Code allows.
    F(u8),
    /// Numeric keypad digit 0-9.
    Numpad(u8),
    /// Keypad `*`.
    NumpadMultiply,
    /// Keypad `+`.
    NumpadAdd,
    /// Keypad separator.
    NumpadSeparator,
    /// Keypad `-`.
    NumpadSubtract,
    /// Keypad `.`.
    NumpadDecimal,
    /// Keypad `/`.
    NumpadDivide,
}

impl NamedKey {
    /// Parses a lowercase VS Code key name.
    fn parse(name: &str) -> Option<Self> {
        let key = match name {
            "backspace" => NamedKey::Backspace,
            "tab" => NamedKey::Tab,
            "enter" => NamedKey::Enter,
            "escape" | "esc" => NamedKey::Escape,
            "pageup" => NamedKey::PageUp,
            "pagedown" => NamedKey::PageDown,
            "end" => NamedKey::End,
            "home" => NamedKey::Home,
            "left" => NamedKey::Left,
            "up" => NamedKey::Up,
            "right" => NamedKey::Right,
            "down" => NamedKey::Down,
            "insert" => NamedKey::Insert,
            "delete" => NamedKey::Delete,
            "pausebreak" => NamedKey::PauseBreak,
            "capslock" => NamedKey::CapsLock,
            "scrolllock" => NamedKey::ScrollLock,
            "numlock" => NamedKey::NumLock,
            "contextmenu" => NamedKey::ContextMenu,
            "numpad_multiply" => NamedKey::NumpadMultiply,
            "numpad_add" => NamedKey::NumpadAdd,
            "numpad_separator" => NamedKey::NumpadSeparator,
            "numpad_subtract" => NamedKey::NumpadSubtract,
            "numpad_decimal" => NamedKey::NumpadDecimal,
            "numpad_divide" => NamedKey::NumpadDivide,
            _ => {
                if let Some(digits) = name.strip_prefix('f') {
                    let n: u8 = digits.parse().ok()?;
                    if (1..=19).contains(&n) {
                        return Some(NamedKey::F(n));
                    }
                    return None;
                }
                if let Some(digit) = name.strip_prefix("numpad") {
                    let n: u8 = digit.parse().ok()?;
                    if n <= 9 {
                        return Some(NamedKey::Numpad(n));
                    }
                }
                return None;
            }
        };
        Some(key)
    }
}

impl fmt::Display for NamedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            NamedKey::Backspace => "backspace",
            NamedKey::Tab => "tab",
            NamedKey::Enter => "enter",
            NamedKey::Escape => "escape",
            NamedKey::PageUp => "pageup",
            NamedKey::PageDown => "pagedown",
            NamedKey::End => "end",
            NamedKey::Home => "home",
            NamedKey::Left => "left",
            NamedKey::Up => "up",
            NamedKey::Right => "right",
            NamedKey::Down => "down",
            NamedKey::Insert => "insert",
            NamedKey::Delete => "delete",
            NamedKey::PauseBreak => "pausebreak",
            NamedKey::CapsLock => "capslock",
            NamedKey::ScrollLock => "scrolllock",
            NamedKey::NumLock => "numlock",
            NamedKey::ContextMenu => "contextmenu",
            NamedKey::NumpadMultiply => "numpad_multiply",
            NamedKey::NumpadAdd => "numpad_add",
            NamedKey::NumpadSeparator => "numpad_separator",
            NamedKey::NumpadSubtract => "numpad_subtract",
            NamedKey::NumpadDecimal => "numpad_decimal",
            NamedKey::NumpadDivide => "numpad_divide",
            NamedKey::F(n) => return write!(f, "f{n}"),
            NamedKey::Numpad(n) => return write!(f, "numpad{n}"),
        };
        f.write_str(name)
    }
}

/// The non-modifier part of a chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    /// A printable character, always stored lowercase — `Shift` lives in
    /// [`Modifiers`], so `A` and `shift+a` are the same chord.
    Char(char),
    /// A key identified by name.
    Named(NamedKey),
}

/// A single keypress: modifiers plus one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    /// Modifiers that must be held.
    pub modifiers: Modifiers,
    /// The key itself.
    pub key: Key,
}

impl Chord {
    /// Builds a chord.
    pub fn new(modifiers: Modifiers, key: Key) -> Self {
        Self {
            modifiers,
            key: normalize_key(key),
        }
    }

    /// A chord with no modifiers.
    pub fn plain(key: Key) -> Self {
        Self::new(Modifiers::NONE, key)
    }

    /// A chord for a printable character with no modifiers.
    pub fn char(c: char) -> Self {
        Self::plain(Key::Char(c))
    }

    /// Parses a single chord such as `ctrl+shift+p`.
    pub fn parse(input: &str) -> Result<Self, KeyParseError> {
        let mut rest = input.trim();
        if rest.is_empty() {
            return Err(KeyParseError::Empty);
        }
        let mut modifiers = Modifiers::default();

        // Strip modifier prefixes one at a time rather than splitting on '+',
        // so that a binding whose key *is* `+` or `-` still parses.
        while let Some(plus) = rest.find('+') {
            // A leading `+` is the key itself, not an empty modifier.
            if plus == 0 {
                break;
            }
            let (head, tail) = rest.split_at(plus);
            let lower = head.to_ascii_lowercase();
            let m = match lower.as_str() {
                "ctrl" | "control" => Modifiers::CTRL,
                "shift" => Modifiers::SHIFT,
                "alt" | "option" => Modifiers::ALT,
                "cmd" | "command" | "meta" | "win" | "super" => Modifiers::META,
                _ => break,
            };
            if (m.ctrl && modifiers.ctrl)
                || (m.shift && modifiers.shift)
                || (m.alt && modifiers.alt)
                || (m.meta && modifiers.meta)
            {
                return Err(KeyParseError::DuplicateModifier {
                    input: input.to_owned(),
                    modifier: lower,
                });
            }
            modifiers = modifiers.union(m);
            rest = &tail[1..];
        }

        if rest.is_empty() {
            return Err(KeyParseError::MissingKey {
                input: input.to_owned(),
            });
        }

        let key = parse_key(rest).ok_or_else(|| KeyParseError::UnknownKey {
            input: input.to_owned(),
            key: rest.to_owned(),
        })?;
        Ok(Chord {
            modifiers,
            key: normalize_key(key),
        })
    }
}

/// Lowercases character keys so `Shift` is only ever recorded as a modifier.
fn normalize_key(key: Key) -> Key {
    match key {
        Key::Char(c) => Key::Char(c.to_lowercase().next().unwrap_or(c)),
        other => other,
    }
}

fn parse_key(token: &str) -> Option<Key> {
    let lower = token.to_ascii_lowercase();
    // `space` is the one VS Code key name that stands for a character deco can
    // also be handed directly. A terminal sends NUL for Ctrl+Space and crossterm
    // reports it as `Char(' ')` with Control; a window system reports the space
    // bar as a named key. One of those has to be the representation, and the
    // character is the one that also has to type a space when nothing is bound
    // to it — so a binding written `space` means the same key.
    if lower == "space" {
        return Some(Key::Char(' '));
    }
    if let Some(named) = NamedKey::parse(&lower) {
        return Some(Key::Named(named));
    }
    let mut chars = token.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        return Some(Key::Char(first));
    }
    None
}

impl fmt::Display for Chord {
    /// Writes the canonical `keybindings.json` spelling, modifiers in VS Code's
    /// order so that round-tripping a file does not reshuffle it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.modifiers.shift {
            f.write_str("shift+")?;
        }
        if self.modifiers.alt {
            f.write_str("alt+")?;
        }
        if self.modifiers.meta {
            f.write_str("cmd+")?;
        }
        match self.key {
            // Written by name, so a chord round-trips through `keybindings.json`
            // rather than ending in a trailing blank nothing can read back.
            Key::Char(' ') => f.write_str("space"),
            Key::Char(c) => write!(f, "{c}"),
            Key::Named(n) => write!(f, "{n}"),
        }
    }
}

/// One or two chords — VS Code allows a two-chord sequence such as
/// `ctrl+k ctrl+c` and no more.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeySequence(Vec<Chord>);

impl KeySequence {
    /// Builds a sequence, rejecting anything outside one or two chords.
    pub fn new(chords: Vec<Chord>) -> Result<Self, KeyParseError> {
        match chords.len() {
            0 => Err(KeyParseError::Empty),
            1 | 2 => Ok(Self(chords)),
            n => Err(KeyParseError::TooManyChords { count: n }),
        }
    }

    /// A single-chord sequence.
    pub fn single(chord: Chord) -> Self {
        Self(vec![chord])
    }

    /// Parses a whitespace-separated sequence such as `ctrl+k ctrl+c`.
    pub fn parse(input: &str) -> Result<Self, KeyParseError> {
        let chords: Result<Vec<Chord>, _> = input.split_whitespace().map(Chord::parse).collect();
        Self::new(chords?)
    }

    /// The chords in order.
    pub fn chords(&self) -> &[Chord] {
        &self.0
    }

    /// The first chord, which is what dispatch keys off.
    pub fn first(&self) -> Chord {
        self.0[0]
    }

    /// Whether this sequence needs a second keypress.
    pub fn is_chord(&self) -> bool {
        self.0.len() > 1
    }
}

impl fmt::Display for KeySequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (idx, chord) in self.0.iter().enumerate() {
            if idx > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{chord}")?;
        }
        Ok(())
    }
}

/// Failure to parse a key or key sequence.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyParseError {
    /// The input held no chords at all.
    #[error("empty key binding")]
    Empty,
    /// Modifiers were given but no key followed.
    #[error("key binding `{input}` has modifiers but no key")]
    MissingKey {
        /// The offending input.
        input: String,
    },
    /// The key name was not recognised.
    #[error("key binding `{input}` uses unknown key `{key}`")]
    UnknownKey {
        /// The offending input.
        input: String,
        /// The unrecognised token.
        key: String,
    },
    /// The same modifier appeared twice.
    #[error("key binding `{input}` repeats modifier `{modifier}`")]
    DuplicateModifier {
        /// The offending input.
        input: String,
        /// The repeated modifier.
        modifier: String,
    },
    /// More than two chords were given.
    #[error("key binding has {count} chords; at most 2 are supported")]
    TooManyChords {
        /// How many chords were found.
        count: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_character() {
        let c = Chord::parse("a").unwrap();
        assert_eq!(c, Chord::char('a'));
        assert!(c.modifiers.is_empty());
    }

    #[test]
    fn parses_modifiers() {
        let c = Chord::parse("ctrl+shift+p").unwrap();
        assert!(c.modifiers.ctrl && c.modifiers.shift);
        assert!(!c.modifiers.alt && !c.modifiers.meta);
        assert_eq!(c.key, Key::Char('p'));
    }

    #[test]
    fn space_is_one_key_however_it_is_written() {
        // The bug this guards: `space` parsed to a named key while both frontends
        // could deliver the space bar as a character, so `ctrl+space` was a
        // binding nothing could ever press.
        assert_eq!(Chord::parse("space").unwrap().key, Key::Char(' '));
        assert_eq!(
            Chord::parse("ctrl+space").unwrap(),
            Chord::new(Modifiers::CTRL, Key::Char(' '))
        );
    }

    #[test]
    fn a_space_chord_is_written_back_by_name() {
        // A trailing blank is not something `keybindings.json` can be read back
        // from, so the canonical spelling stays `space`.
        assert_eq!(
            Chord::parse("ctrl+space").unwrap().to_string(),
            "ctrl+space"
        );
        assert_eq!(Chord::char(' ').to_string(), "space");
        assert_eq!(
            KeySequence::parse("ctrl+k space").unwrap().to_string(),
            "ctrl+k space"
        );
    }

    #[test]
    fn modifier_order_does_not_matter() {
        assert_eq!(
            Chord::parse("shift+ctrl+p").unwrap(),
            Chord::parse("ctrl+shift+p").unwrap()
        );
    }

    #[test]
    fn all_meta_spellings_are_equivalent() {
        for spelling in ["cmd+a", "command+a", "meta+a", "win+a", "super+a"] {
            assert!(
                Chord::parse(spelling).unwrap().modifiers.meta,
                "{spelling} lost meta"
            );
        }
    }

    #[test]
    fn parsing_is_case_insensitive() {
        assert_eq!(
            Chord::parse("Ctrl+Shift+P").unwrap(),
            Chord::parse("ctrl+shift+p").unwrap()
        );
    }

    #[test]
    fn shift_is_a_modifier_not_a_capital_letter() {
        // `A` and `a` are the same key; only the modifier distinguishes them.
        assert_eq!(Chord::parse("A").unwrap(), Chord::parse("a").unwrap());
        assert_ne!(Chord::parse("shift+a").unwrap(), Chord::parse("a").unwrap());
    }

    #[test]
    fn parses_punctuation_keys() {
        for (input, expected) in [
            ("ctrl+,", ','),
            ("ctrl+.", '.'),
            ("ctrl+/", '/'),
            ("ctrl+;", ';'),
            ("ctrl+`", '`'),
        ] {
            assert_eq!(
                Chord::parse(input).unwrap().key,
                Key::Char(expected),
                "{input}"
            );
        }
    }

    #[test]
    fn parses_plus_and_minus_as_keys() {
        // Splitting naively on '+' would mangle both of these.
        let plus = Chord::parse("ctrl+shift+=").unwrap();
        assert_eq!(plus.key, Key::Char('='));
        let minus = Chord::parse("ctrl+-").unwrap();
        assert_eq!(minus.key, Key::Char('-'));
        assert!(minus.modifiers.ctrl);
        let literal_plus = Chord::parse("ctrl++").unwrap();
        assert_eq!(literal_plus.key, Key::Char('+'));
    }

    #[test]
    fn parses_named_keys() {
        for (input, expected) in [
            ("enter", NamedKey::Enter),
            ("escape", NamedKey::Escape),
            ("esc", NamedKey::Escape),
            ("pageup", NamedKey::PageUp),
            ("f1", NamedKey::F(1)),
            ("f19", NamedKey::F(19)),
            ("numpad0", NamedKey::Numpad(0)),
            ("numpad_add", NamedKey::NumpadAdd),
        ] {
            assert_eq!(
                Chord::parse(input).unwrap().key,
                Key::Named(expected),
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_out_of_range_function_keys() {
        assert!(Chord::parse("f0").is_err());
        assert!(Chord::parse("f20").is_err());
    }

    #[test]
    fn rejects_modifiers_without_a_key() {
        assert_eq!(
            Chord::parse("ctrl+"),
            Err(KeyParseError::MissingKey {
                input: "ctrl+".into()
            })
        );
    }

    #[test]
    fn rejects_unknown_key_names() {
        assert!(matches!(
            Chord::parse("ctrl+frobnicate"),
            Err(KeyParseError::UnknownKey { .. })
        ));
    }

    #[test]
    fn rejects_repeated_modifiers() {
        assert!(matches!(
            Chord::parse("ctrl+ctrl+a"),
            Err(KeyParseError::DuplicateModifier { .. })
        ));
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(Chord::parse("   "), Err(KeyParseError::Empty));
    }

    #[test]
    fn parses_two_chord_sequences() {
        let seq = KeySequence::parse("ctrl+k ctrl+c").unwrap();
        assert!(seq.is_chord());
        assert_eq!(seq.chords().len(), 2);
        assert_eq!(seq.first(), Chord::parse("ctrl+k").unwrap());
    }

    #[test]
    fn rejects_three_chord_sequences() {
        assert_eq!(
            KeySequence::parse("ctrl+a ctrl+b ctrl+c"),
            Err(KeyParseError::TooManyChords { count: 3 })
        );
    }

    #[test]
    fn formats_in_canonical_order() {
        assert_eq!(
            Chord::parse("shift+ctrl+alt+cmd+p").unwrap().to_string(),
            "ctrl+shift+alt+cmd+p"
        );
        assert_eq!(
            KeySequence::parse("ctrl+k  ctrl+c").unwrap().to_string(),
            "ctrl+k ctrl+c"
        );
    }

    #[test]
    fn formatting_round_trips_through_parsing() {
        for input in [
            "ctrl+shift+p",
            "cmd+k cmd+c",
            "f5",
            "alt+left",
            "ctrl+numpad_add",
            "ctrl+-",
        ] {
            let parsed = KeySequence::parse(input).unwrap();
            let reparsed = KeySequence::parse(&parsed.to_string()).unwrap();
            assert_eq!(parsed, reparsed, "{input} did not round trip");
        }
    }
}
