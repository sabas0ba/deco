//! The terminal's clipboard, written with OSC 52.
//!
//! A terminal program cannot reach the system clipboard directly: over SSH, in
//! a container or under WSL it runs on a different machine from the clipboard.
//! The terminal emulator can, and OSC 52 (`ESC ] 52 ; c ; <base64> ST`, from
//! xterm's control sequences) asks it to set the clipboard. The terminal is the
//! only path that works in all of those cases, so deco uses it rather than a
//! platform clipboard library.
//!
//! Only writing is supported. Reading the clipboard through OSC 52 is disabled
//! by default in most terminals, because it lets any program that writes to the
//! terminal read what the user copied. Paste therefore uses deco's own copy of
//! the last text copied in deco; text copied elsewhere is pasted with the
//! terminal's own paste key, which arrives as typed text.
//!
//! The terminal must allow programs to set the clipboard. Most do by default;
//! tmux needs `set -g set-clipboard on`.

use std::cell::RefCell;
use std::rc::Rc;

use deco_editor::commands::Clipboard;

/// The most text sent to the terminal in one write, in bytes.
///
/// A larger copy stays in deco's own clipboard. Terminals limit the sequence's
/// length differently, and writing megabytes of base64 to a slow connection
/// would stall the editor.
pub const MAX_BYTES: usize = 1024 * 1024;

/// deco's clipboard, which also hands each write to the terminal.
///
/// The text is kept so that paste works whether or not the terminal accepted
/// the write. The sequence is not written here because the clipboard has no
/// access to the terminal; [`Pending`] holds it until the event loop writes it.
#[derive(Debug)]
pub struct TerminalClipboard {
    text: String,
    pending: Pending,
}

impl TerminalClipboard {
    /// A clipboard whose writes are queued on `pending`.
    pub fn new(pending: Pending) -> Self {
        Self {
            text: String::new(),
            pending,
        }
    }
}

impl Clipboard for TerminalClipboard {
    fn read(&self) -> String {
        self.text.clone()
    }

    fn write(&mut self, text: &str) {
        self.text = text.to_owned();
        *self.pending.0.borrow_mut() = (text.len() <= MAX_BYTES).then(|| text.to_owned());
    }
}

/// The text most recently copied and not yet sent to the terminal.
///
/// Shared between the clipboard, which the session owns, and the driver, which
/// sends it. Only the latest copy is kept, because the terminal's clipboard
/// holds one value and an earlier copy would be overwritten at once.
#[derive(Debug, Clone, Default)]
pub struct Pending(Rc<RefCell<Option<String>>>);

impl Pending {
    /// Takes the text waiting to be sent, if any.
    pub fn take(&self) -> Option<String> {
        self.0.borrow_mut().take()
    }
}

/// The OSC 52 sequence that sets the clipboard to `text`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", base64(text.as_bytes()))
}

/// `bytes` in the base64 alphabet of RFC 4648, section 4, with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let value = group.iter().enumerate().fold(0u32, |value, (i, &byte)| {
            value | u32::from(byte) << (16 - 8 * i)
        });
        // A group of n bytes gives n + 1 characters; the rest are padding.
        for i in 0..4 {
            if i <= group.len() {
                let index = (value >> (18 - 6 * i)) & 0x3f;
                encoded.push(char::from(ALPHABET[index as usize]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_test_vectors() {
        // RFC 4648, section 10.
        for (input, output) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), output, "{input:?}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn a_copy_is_kept_for_paste_and_queued_for_the_terminal() {
        let pending = Pending::default();
        let mut clipboard = TerminalClipboard::new(pending.clone());
        clipboard.write("first");
        clipboard.write("héllo\n");

        assert_eq!(clipboard.read(), "héllo\n");
        assert_eq!(
            pending.take().as_deref(),
            Some("héllo\n"),
            "the latest only"
        );
        assert_eq!(pending.take(), None);
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x1b\\");
    }

    #[test]
    fn a_copy_too_large_for_the_terminal_stays_in_deco() {
        let pending = Pending::default();
        let mut clipboard = TerminalClipboard::new(pending.clone());
        let large = "x".repeat(MAX_BYTES + 1);
        clipboard.write(&large);

        assert_eq!(clipboard.read(), large);
        assert_eq!(pending.take(), None);
    }
}
