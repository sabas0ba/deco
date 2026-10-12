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

#[cfg(test)]
mod tests {
    use super::*;

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
