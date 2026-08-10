//! One line of editable text, with a caret and no selection.
//!
//! The find bar's query, its replacement, the go-to-line box and the command
//! palette's filter are the same thing four times over. They share this type
//! rather than each keeping a `String` and an index, because a second copy of the
//! character/byte arithmetic would be a second place for it to be wrong on any
//! text outside ASCII.
//!
//! # Why it consumes commands
//!
//! A one-line input in VS Code is a real text input inside the editor, so while
//! it has focus `editorTextFocus` is false but `textInputFocus` is true — and
//! every editing command bound to `textInputFocus` (`left`, `backspace`,
//! `ctrl+v`) still resolves, with the input handling it. deco has no DOM to do
//! that layering, so [`Input::consume`] does it explicitly: it claims the
//! text-editing commands and lets everything else through to the editor.
//!
//! That is why `ctrl+v` with a prompt open pastes into the prompt and not into
//! the document, and why `ctrl+z` cannot silently rewrite the file behind one.
//!
//! # No selection model
//!
//! There is a caret but no selection, so `ctrl+a`, `ctrl+c` and `ctrl+x` act on
//! the whole line. They are still swallowed rather than passed through: a
//! `ctrl+x` that cut a line out of the document while the user was editing a
//! search term would be a genuine loss.

use serde_json::Value;

use crate::commands::Clipboard;

/// A one-line editable field.
#[derive(Debug, Default, Clone)]
pub struct Input {
    text: String,
    /// Caret offset, counted in characters.
    caret: usize,
}

impl Input {
    /// An empty field.
    pub fn new() -> Self {
        Self::default()
    }

    /// The text as typed.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Caret offset, in characters.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Replaces the text, putting the caret at the end.
    pub fn set(&mut self, text: String) {
        self.text = text;
        self.caret = self.text.chars().count();
    }

    /// Empties the field.
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// Applies a command, if it is one this field owns.
    ///
    /// Returns whether the command was consumed. See the module docs for why this
    /// exists rather than being expressed in `when` clauses alone.
    pub fn consume(
        &mut self,
        command: &str,
        args: Option<&Value>,
        clipboard: &mut dyn Clipboard,
    ) -> bool {
        match command {
            "type" => {
                let text = args
                    .and_then(|a| a.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.insert_str(text);
                true
            }
            "deleteLeft" => {
                if self.caret > 0 {
                    self.caret -= 1;
                    self.remove_at(self.caret);
                }
                true
            }
            "deleteRight" => {
                if self.caret < self.len() {
                    self.remove_at(self.caret);
                }
                true
            }
            "deleteWordLeft" => {
                let target = self.word_left();
                for _ in target..self.caret {
                    self.remove_at(target);
                }
                self.caret = target;
                true
            }
            "deleteWordRight" => {
                let target = self.word_right();
                for _ in self.caret..target {
                    self.remove_at(self.caret);
                }
                true
            }
            "cursorLeft" => {
                self.caret = self.caret.saturating_sub(1);
                true
            }
            "cursorRight" => {
                self.caret = (self.caret + 1).min(self.len());
                true
            }
            // A one-line field has no notion of the top or bottom of a document,
            // so `ctrl+home` and `home` mean the same thing here.
            "cursorHome" | "cursorTop" => {
                self.caret = 0;
                true
            }
            "cursorEnd" | "cursorBottom" => {
                self.caret = self.len();
                true
            }
            "cursorWordLeft" => {
                self.caret = self.word_left();
                true
            }
            "cursorWordEndRight" => {
                self.caret = self.word_right();
                true
            }
            "editor.action.clipboardPasteAction" => {
                let pasted = clipboard.read();
                self.insert_str(&pasted);
                true
            }
            "editor.action.clipboardCopyAction" => {
                clipboard.write(&self.text);
                true
            }
            "editor.action.clipboardCutAction" => {
                clipboard.write(&self.text);
                self.clear();
                true
            }
            // Swallowed rather than handled. There is nothing to select, undo or
            // redo in one line of text — and letting these through would apply
            // them to the document, which is not where the user is looking.
            "editor.action.selectAll" | "undo" | "redo" => true,
            _ => false,
        }
    }

    /// Inserts `text` at the caret, dropping line breaks.
    ///
    /// A newline it cannot represent would be invisible in a one-line field;
    /// `enter` is bound to a command in every prompt that uses this, so this only
    /// guards against a pasted or scripted one.
    fn insert_str(&mut self, text: &str) {
        for c in text.chars().filter(|c| *c != '\n' && *c != '\r') {
            let byte = self.byte_offset(self.caret);
            self.text.insert(byte, c);
            self.caret += 1;
        }
    }

    /// Removes the character at character offset `index`.
    fn remove_at(&mut self, index: usize) {
        let byte = self.byte_offset(index);
        self.text.remove(byte);
    }

    /// Length in characters.
    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// Byte offset of character offset `index`.
    ///
    /// The caret is counted in characters so that arrow keys move one visible
    /// thing at a time, but `String` is indexed in bytes, and any text outside
    /// ASCII makes the two differ.
    fn byte_offset(&self, index: usize) -> usize {
        self.text
            .char_indices()
            .nth(index)
            .map(|(byte, _)| byte)
            .unwrap_or(self.text.len())
    }

    /// Start of the word to the left of the caret.
    fn word_left(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = self.caret;
        while index > 0 && !is_word_char(chars[index - 1]) {
            index -= 1;
        }
        while index > 0 && is_word_char(chars[index - 1]) {
            index -= 1;
        }
        index
    }

    /// End of the word to the right of the caret.
    fn word_right(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = self.caret;
        while index < chars.len() && !is_word_char(chars[index]) {
            index += 1;
        }
        while index < chars.len() && is_word_char(chars[index]) {
            index += 1;
        }
        index
    }
}

/// The word rule for a one-line field.
///
/// Deliberately not `deco_core::search`'s: that one describes the *document's*
/// words, and a search query is not a document.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::MemoryClipboard;

    fn input(text: &str) -> Input {
        let mut input = Input::new();
        input.set(text.to_owned());
        input
    }

    fn run(input: &mut Input, command: &str) -> bool {
        input.consume(command, None, &mut MemoryClipboard::default())
    }

    #[test]
    fn setting_text_puts_the_caret_at_the_end() {
        assert_eq!(input("foo").caret(), 3);
    }

    #[test]
    fn a_command_it_does_not_own_is_left_alone() {
        let mut input = input("foo");
        for command in [
            "cursorUp",
            "editor.action.commentLine",
            "workbench.action.quit",
        ] {
            assert!(!run(&mut input, command), "{command}");
        }
    }

    #[test]
    fn the_caret_is_counted_in_characters_not_bytes() {
        // The whole reason this is one type rather than a `String` and an index
        // at each call site.
        let mut input = input("naïve");
        assert_eq!(input.caret(), 5);
        run(&mut input, "deleteLeft");
        assert_eq!(input.text(), "naïv");
        run(&mut input, "cursorHome");
        run(&mut input, "cursorRight");
        run(&mut input, "cursorRight");
        input.consume(
            "type",
            Some(&serde_json::json!({ "text": "X" })),
            &mut MemoryClipboard::default(),
        );
        assert_eq!(input.text(), "naXïv");
    }

    #[test]
    fn cut_and_copy_act_on_the_whole_line() {
        let mut clipboard = MemoryClipboard::default();
        let mut input = input("foo");
        input.consume("editor.action.clipboardCopyAction", None, &mut clipboard);
        assert_eq!(clipboard.read(), "foo");
        assert_eq!(input.text(), "foo");
        input.consume("editor.action.clipboardCutAction", None, &mut clipboard);
        assert_eq!(input.text(), "");
        assert_eq!(input.caret(), 0);
    }

    #[test]
    fn undo_is_swallowed_so_it_cannot_reach_the_document() {
        let mut input = input("foo");
        for command in ["undo", "redo", "editor.action.selectAll"] {
            assert!(run(&mut input, command), "{command} should be consumed");
        }
        assert_eq!(input.text(), "foo");
    }

    #[test]
    fn a_pasted_newline_does_not_reach_a_one_line_field() {
        let mut clipboard = MemoryClipboard::default();
        clipboard.write("one\ntwo");
        let mut input = Input::new();
        input.consume("editor.action.clipboardPasteAction", None, &mut clipboard);
        assert_eq!(input.text(), "onetwo");
    }
}
