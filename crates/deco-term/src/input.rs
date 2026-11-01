//! The bytes a key sends to the program in a terminal.
//!
//! The encodings are xterm's, which is what `TERM=xterm-256color` promises:
//! control characters for `ctrl` with a letter, `ESC` before a key held with
//! `alt`, and `CSI` sequences with a modifier parameter for the cursor and
//! function keys (`ESC [ 1 ; 5 C` for `ctrl+right`). In application cursor
//! mode the unmodified arrows send `ESC O` instead of `ESC [`.

use deco_keymap::keys::{Chord, Key, NamedKey};

use crate::screen::Modes;

/// The bytes for `chord`, or `None` for a key that sends nothing.
pub fn encode(chord: &Chord, modes: Modes) -> Option<Vec<u8>> {
    let m = chord.modifiers;
    // xterm's modifier parameter: 1 plus a bit per modifier.
    let parameter =
        1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.ctrl) + 8 * u8::from(m.meta);
    let modified = parameter > 1;

    let named = match chord.key {
        Key::Char(c) => return encode_char(c, chord),
        Key::Named(named) => named,
    };
    // Keys whose plain form is one byte take `ESC` for `alt`.
    let simple = |byte: u8| {
        let mut bytes = Vec::new();
        if m.alt {
            bytes.push(0x1b);
        }
        bytes.push(byte);
        Some(bytes)
    };
    let cursor = |letter: u8| {
        let mut bytes = Vec::new();
        if modified {
            bytes.extend_from_slice(format!("\x1b[1;{parameter}").as_bytes());
        } else if modes.application_cursor {
            bytes.extend_from_slice(b"\x1bO");
        } else {
            bytes.extend_from_slice(b"\x1b[");
        }
        bytes.push(letter);
        Some(bytes)
    };
    let tilde = |number: u8| {
        let text = if modified {
            format!("\x1b[{number};{parameter}~")
        } else {
            format!("\x1b[{number}~")
        };
        Some(text.into_bytes())
    };
    // F1 to F4 use `SS3` unmodified and `CSI 1 ; m` with modifiers.
    let ss3 = |letter: u8| {
        let text = if modified {
            format!("\x1b[1;{parameter}{}", char::from(letter))
        } else {
            format!("\x1bO{}", char::from(letter))
        };
        Some(text.into_bytes())
    };
    match named {
        NamedKey::Enter => simple(b'\r'),
        NamedKey::Tab if m.shift => Some(b"\x1b[Z".to_vec()),
        NamedKey::Tab => simple(b'\t'),
        NamedKey::Backspace if m.ctrl => simple(0x08),
        NamedKey::Backspace => simple(0x7f),
        NamedKey::Escape => simple(0x1b),
        NamedKey::Up => cursor(b'A'),
        NamedKey::Down => cursor(b'B'),
        NamedKey::Right => cursor(b'C'),
        NamedKey::Left => cursor(b'D'),
        NamedKey::Home => cursor(b'H'),
        NamedKey::End => cursor(b'F'),
        NamedKey::Insert => tilde(2),
        NamedKey::Delete => tilde(3),
        NamedKey::PageUp => tilde(5),
        NamedKey::PageDown => tilde(6),
        NamedKey::F(n @ 1..=4) => ss3(b'P' + n - 1),
        NamedKey::F(n) => {
            let number = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                13 => 25,
                14 => 26,
                15 => 28,
                16 => 29,
                17 => 31,
                18 => 32,
                19 => 33,
                _ => return None,
            };
            tilde(number)
        }
        NamedKey::Numpad(digit) => simple(b'0' + digit.min(9)),
        NamedKey::NumpadMultiply => simple(b'*'),
        NamedKey::NumpadAdd => simple(b'+'),
        NamedKey::NumpadSubtract => simple(b'-'),
        NamedKey::NumpadDecimal => simple(b'.'),
        NamedKey::NumpadDivide => simple(b'/'),
        NamedKey::NumpadSeparator => simple(b','),
        _ => None,
    }
}

/// A printable key, with `ctrl` making a control character and `alt` adding
/// `ESC`.
fn encode_char(c: char, chord: &Chord) -> Option<Vec<u8>> {
    let m = chord.modifiers;
    // The chord holds the unshifted letter with `shift` set; the program
    // expects the character the key produced.
    let c = if m.shift && c.is_alphabetic() {
        c.to_uppercase().next().unwrap_or(c)
    } else {
        c
    };
    let mut bytes = Vec::new();
    if m.alt {
        bytes.push(0x1b);
    }
    if m.ctrl {
        let control = match c.to_ascii_lowercase() {
            letter @ 'a'..='z' => letter as u8 - b'a' + 1,
            ' ' | '@' | '2' => 0x00,
            '[' | '3' => 0x1b,
            '\\' | '4' => 0x1c,
            ']' | '5' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '-' | '/' | '7' => 0x1f,
            '8' | '?' => 0x7f,
            // No control character: the key is sent as typed.
            _ => {
                let mut buffer = [0; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                return Some(bytes);
            }
        };
        bytes.push(control);
        return Some(bytes);
    }
    let mut buffer = [0; 4];
    bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
    Some(bytes)
}

/// Pasted text, bracketed when the program asked for it.
///
/// Inside brackets, an embedded end marker is removed so the text cannot end
/// the paste early and have the rest run as typed commands.
pub fn paste(text: &str, modes: Modes) -> Vec<u8> {
    if !modes.bracketed_paste {
        return text.replace("\r\n", "\r").replace('\n', "\r").into_bytes();
    }
    // Repeated, because removing one marker can join two halves of another.
    let mut text = text.to_owned();
    while text.contains("\x1b[201~") {
        text = text.replace("\x1b[201~", "");
    }
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

/// A mouse button, for [`encode_mouse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// The primary button.
    Left,
    /// The wheel's button.
    Middle,
    /// The secondary button.
    Right,
}

impl MouseButton {
    fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
        }
    }
}

/// What the mouse did, for [`encode_mouse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    /// A button went down.
    Press(MouseButton),
    /// A button came up.
    Release(MouseButton),
    /// The pointer moved with a button held.
    Drag(MouseButton),
    /// The pointer moved with no button held.
    Move,
    /// The wheel turned away from the user.
    WheelUp,
    /// The wheel turned towards the user.
    WheelDown,
}

/// A mouse event in a terminal's cells, for [`encode_mouse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseReport {
    /// What happened.
    pub action: MouseAction,
    /// The column, from 0 at the left.
    pub col: usize,
    /// The row, from 0 at the top.
    pub row: usize,
    /// Shift was held.
    pub shift: bool,
    /// Alt was held, which xterm reports as meta.
    pub alt: bool,
    /// Ctrl was held.
    pub ctrl: bool,
}

/// The bytes that report `report` to a program that asked for mouse events,
/// or `None` when it did not ask for this one.
///
/// xterm's encodings: the SGR form (`CSI < b ; x ; y M`, or `m` for a
/// release) when mode 1006 is on, and otherwise the original form
/// (`CSI M` and three bytes), which cannot carry a column or row past 222 and
/// sends nothing there. The button code is 0 to 2 for a button, 3 for a
/// release in the original form, 64 and 65 for the wheel, plus 32 for a move,
/// and 4, 8 and 16 for shift, alt and ctrl.
pub fn encode_mouse(report: &MouseReport, modes: Modes) -> Option<Vec<u8>> {
    use crate::screen::MouseTracking;
    let wanted = match (modes.mouse, report.action) {
        (MouseTracking::Off, _) => false,
        (_, MouseAction::Press(_) | MouseAction::Release(_)) => true,
        (_, MouseAction::WheelUp | MouseAction::WheelDown) => true,
        (MouseTracking::Press, _) => false,
        (MouseTracking::Drag, MouseAction::Drag(_)) => true,
        (MouseTracking::Drag, _) => false,
        (MouseTracking::Any, _) => true,
    };
    if !wanted {
        return None;
    }
    let base = match report.action {
        MouseAction::Press(button) | MouseAction::Release(button) => button.code(),
        MouseAction::Drag(button) => button.code() + 32,
        MouseAction::Move => 3 + 32,
        MouseAction::WheelUp => 64,
        MouseAction::WheelDown => 65,
    };
    let modifiers =
        4 * u8::from(report.shift) + 8 * u8::from(report.alt) + 16 * u8::from(report.ctrl);
    let (x, y) = (report.col + 1, report.row + 1);
    if modes.mouse_sgr {
        let release = matches!(report.action, MouseAction::Release(_));
        return Some(
            format!(
                "\x1b[<{};{x};{y}{}",
                base + modifiers,
                if release { 'm' } else { 'M' }
            )
            .into_bytes(),
        );
    }
    // The original form has no release per button: a release is code 3.
    let code = match report.action {
        MouseAction::Release(_) => 3,
        _ => base,
    } + modifiers;
    let byte = |value: usize| u8::try_from(value + 32).ok().filter(|byte| *byte < 255);
    Some(vec![0x1b, b'[', b'M', code + 32, byte(x)?, byte(y)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(action: MouseAction, col: usize, row: usize) -> MouseReport {
        MouseReport {
            action,
            col,
            row,
            shift: false,
            alt: false,
            ctrl: false,
        }
    }

    #[test]
    fn nothing_is_sent_until_a_program_asks_for_the_mouse() {
        let press = report(MouseAction::Press(MouseButton::Left), 0, 0);
        assert_eq!(encode_mouse(&press, Modes::default()), None);
    }

    #[test]
    fn a_press_and_release_in_the_sgr_form_and_the_original_one() {
        let mut modes = Modes {
            mouse: crate::screen::MouseTracking::Press,
            ..Modes::default()
        };
        let press = report(MouseAction::Press(MouseButton::Left), 4, 1);
        let release = report(MouseAction::Release(MouseButton::Left), 4, 1);
        assert_eq!(encode_mouse(&press, modes).unwrap(), b"\x1b[M\x20\x25\x22");
        assert_eq!(
            encode_mouse(&release, modes).unwrap(),
            b"\x1b[M\x23\x25\x22"
        );
        modes.mouse_sgr = true;
        assert_eq!(encode_mouse(&press, modes).unwrap(), b"\x1b[<0;5;2M");
        assert_eq!(encode_mouse(&release, modes).unwrap(), b"\x1b[<0;5;2m");
    }

    #[test]
    fn moves_are_sent_only_in_the_modes_that_ask_for_them() {
        let mut modes = Modes {
            mouse_sgr: true,
            ..Modes::default()
        };
        let drag = report(MouseAction::Drag(MouseButton::Left), 0, 0);
        let moved = report(MouseAction::Move, 0, 0);
        modes.mouse = crate::screen::MouseTracking::Press;
        assert_eq!(encode_mouse(&drag, modes), None);
        modes.mouse = crate::screen::MouseTracking::Drag;
        assert_eq!(encode_mouse(&drag, modes).unwrap(), b"\x1b[<32;1;1M");
        assert_eq!(encode_mouse(&moved, modes), None);
        modes.mouse = crate::screen::MouseTracking::Any;
        assert_eq!(encode_mouse(&moved, modes).unwrap(), b"\x1b[<35;1;1M");
    }

    #[test]
    fn the_wheel_and_modifiers_have_their_own_codes() {
        let modes = Modes {
            mouse: crate::screen::MouseTracking::Press,
            mouse_sgr: true,
            ..Modes::default()
        };
        let mut wheel = report(MouseAction::WheelDown, 2, 3);
        wheel.ctrl = true;
        assert_eq!(encode_mouse(&wheel, modes).unwrap(), b"\x1b[<81;3;4M");
    }

    #[test]
    fn the_original_form_sends_nothing_past_its_last_column() {
        let modes = Modes {
            mouse: crate::screen::MouseTracking::Press,
            ..Modes::default()
        };
        let far = report(MouseAction::Press(MouseButton::Left), 300, 0);
        assert_eq!(encode_mouse(&far, modes), None);
    }

    #[test]
    fn a_program_turns_tracking_on_and_off_with_its_modes() {
        let mut terminal = crate::Terminal::new(10, 2, 0);
        terminal.feed(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(
            terminal.screen().modes().mouse,
            crate::screen::MouseTracking::Drag
        );
        assert!(terminal.screen().modes().mouse_sgr);
        terminal.feed(b"\x1b[?1000l");
        assert_eq!(
            terminal.screen().modes().mouse,
            crate::screen::MouseTracking::Off
        );
    }

    fn bytes(chord: &str) -> Vec<u8> {
        encode(&Chord::parse(chord).unwrap(), Modes::default()).unwrap_or_default()
    }

    #[test]
    fn characters_and_control_characters() {
        assert_eq!(bytes("a"), b"a");
        assert_eq!(bytes("shift+a"), b"A");
        assert_eq!(bytes("ctrl+c"), [0x03]);
        assert_eq!(bytes("ctrl+shift+c"), [0x03]);
        assert_eq!(bytes("alt+b"), b"\x1bb");
        assert_eq!(bytes("ctrl+space"), [0x00]);
        assert_eq!(bytes("ctrl+["), [0x1b]);
        assert_eq!(
            encode(
                &Chord::new(Default::default(), Key::Char('é')),
                Modes::default()
            ),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn named_keys() {
        assert_eq!(bytes("enter"), b"\r");
        assert_eq!(bytes("backspace"), [0x7f]);
        assert_eq!(bytes("shift+tab"), b"\x1b[Z");
        assert_eq!(bytes("up"), b"\x1b[A");
        assert_eq!(bytes("ctrl+right"), b"\x1b[1;5C");
        assert_eq!(bytes("shift+alt+left"), b"\x1b[1;4D");
        assert_eq!(bytes("delete"), b"\x1b[3~");
        assert_eq!(bytes("ctrl+pagedown"), b"\x1b[6;5~");
        assert_eq!(bytes("f1"), b"\x1bOP");
        assert_eq!(bytes("shift+f1"), b"\x1b[1;2P");
        assert_eq!(bytes("f5"), b"\x1b[15~");
        assert_eq!(bytes("f12"), b"\x1b[24~");
    }

    #[test]
    fn application_cursor_mode_changes_only_the_plain_arrows() {
        let modes = Modes {
            application_cursor: true,
            ..Modes::default()
        };
        let encode = |chord| encode(&Chord::parse(chord).unwrap(), modes).unwrap();
        assert_eq!(encode("up"), b"\x1bOA");
        assert_eq!(encode("home"), b"\x1bOH");
        assert_eq!(encode("ctrl+up"), b"\x1b[1;5A");
    }

    #[test]
    fn a_paste_cannot_end_its_own_brackets() {
        let modes = Modes {
            bracketed_paste: true,
            ..Modes::default()
        };
        assert_eq!(
            paste("a\x1b[201~rm -rf ~\n", modes),
            b"\x1b[200~arm -rf ~\n\x1b[201~"
        );
        assert_eq!(paste("\x1b[20\x1b[201~1~x", modes), b"\x1b[200~x\x1b[201~");
        assert_eq!(paste("a\nb", Modes::default()), b"a\rb");
    }
}
