//! One line of editable text, with a caret and no selection.
//!
//! The find bar's query and replacement, the go-to-line box and the command
//! palette's filter all use this type instead of each keeping a `String` and an
//! index. This keeps the character/byte offset conversion, which is easy to get
//! wrong for non-ASCII text, in one place.
//!
//! # Why it consumes commands
//!
//! A one-line input in VS Code is a real text input inside the editor. While it
//! has focus, `editorTextFocus` is false but `textInputFocus` is true, so every
//! editing command bound to `textInputFocus` (`left`, `backspace`, `ctrl+v`)
//! still resolves and the input handles it. deco has no DOM, so
//! [`Input::consume`] implements this explicitly: it takes the text-editing
//! commands and passes everything else through to the editor.
//!
//! As a result, `ctrl+v` with a prompt open pastes into the prompt and not into
//! the document, and `ctrl+z` cannot change the file behind an open prompt.
//!
//! # One selection, and it is the whole line
//!
//! There is a caret and no ranged selection, so `ctrl+c` and `ctrl+x` act on the
//! whole line. They are still consumed rather than passed through, so that
//! `ctrl+x` does not cut a line from the document while the user is editing a
//! search term.
//!
//! The only selection a field can have is the whole line. A seeded field opens
//! in this state, and `ctrl+a` sets it. Without it, the first keystroke would
//! append to the seed: pressing `ctrl+shift+f` on `fn` and typing `println`
//! searched for `fnprintln`. While the line is selected, typing replaces it,
//! deleting removes it, and moving the caret clears the selection. This covers
//! what these fields need without supporting arbitrary selection ranges.

use serde_json::Value;

use crate::commands::Clipboard;

/// A one-line editable field.
#[derive(Debug, Default, Clone)]
pub struct Input {
    text: String,
    /// Caret offset, counted in characters.
    caret: usize,
    /// Whether the whole line is selected. See the module docs.
    ///
    /// This is the only selection this field has. It is not a range because no
    /// caller needs one: a seed is either replaced entirely or edited from the
    /// caret.
    selected: bool,
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

    /// Replaces the text, putting the caret at the end and selecting nothing.
    pub fn set(&mut self, text: String) {
        self.text = text;
        self.caret = self.text.chars().count();
        self.selected = false;
    }

    /// Replaces the text and selects all of it, so the next key replaces it.
    ///
    /// Used for a seed the user is expected to edit or discard, such as the
    /// save-as path, the find query or the project-search term. For a seed that
    /// is a *prefix* to continue, such as the directory `ctrl+o` opens with, use
    /// [`Input::set`] so the first keystroke is added at the end.
    pub fn seed(&mut self, text: String) {
        self.set(text);
        self.selected = !self.text.is_empty();
    }

    /// Whether the whole line is selected.
    ///
    /// Used by the renderer to distinguish a field whose next keystroke replaces
    /// all text from one whose next keystroke inserts.
    pub fn selected(&self) -> bool {
        self.selected
    }

    /// Empties the field.
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
        self.selected = false;
    }

    /// Applies a command, if it is one this field owns.
    ///
    /// Returns whether the command was consumed. See the module docs for why this
    /// is needed in addition to `when` clauses.
    pub fn consume(
        &mut self,
        command: &str,
        args: Option<&Value>,
        clipboard: &mut dyn Clipboard,
    ) -> bool {
        // A fully selected line behaves like any other selection: typing
        // replaces it, deleting removes all of it, and moving the caret clears
        // it.
        if self.selected {
            match command {
                "type" | "editor.action.clipboardPasteAction" => self.clear(),
                "deleteLeft" | "deleteRight" | "deleteWordLeft" | "deleteWordRight" => {
                    self.clear();
                    return true;
                }
                "cursorLeft" | "cursorRight" | "cursorHome" | "cursorTop" | "cursorEnd"
                | "cursorBottom" | "cursorWordLeft" | "cursorWordEndRight" => {
                    self.selected = false;
                }
                // Copy and cut already act on the whole line, and select-all set
                // this state. Other commands are not handled by this field, so
                // they do not clear the selection.
                _ => {}
            }
        }

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
            // A one-line field has no document top or bottom, so `ctrl+home`
            // and `home` do the same thing here.
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
            // Selects the whole line, the only selection available. It was
            // previously consumed as a no-op, so `ctrl+a` did nothing.
            "editor.action.selectAll" => {
                self.selected = !self.text.is_empty();
                true
            }
            // Consumed without effect. The field has no undo history, and
            // passing these through would apply them to the document.
            "undo" | "redo" => true,
            _ => false,
        }
    }

    /// Inserts `text` at the caret, dropping line breaks.
    ///
    /// A newline would be invisible in a one-line field. `enter` is bound to a
    /// command in every prompt that uses this type, so this only affects pasted
    /// or scripted text.
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
    /// The caret is counted in characters so that arrow keys move one character
    /// at a time. `String` is indexed in bytes, and the two differ for non-ASCII
    /// text.
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
/// This intentionally differs from `deco_core::search`, which defines words in
/// the document, not in a search query.
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
        // This is the main reason for a shared type instead of a `String` and an
        // index at each call site.
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
        for command in ["undo", "redo"] {
            assert!(run(&mut input, command), "{command} should be consumed");
        }
        assert_eq!(input.text(), "foo");
    }

    /// A field seeded with `text`, which is how a prompt or the find bar opens.
    fn seeded(text: &str) -> Input {
        let mut input = Input::new();
        input.seed(text.to_owned());
        input
    }

    fn typed(input: &mut Input, text: &str) {
        input.consume(
            "type",
            Some(&serde_json::json!({ "text": text })),
            &mut MemoryClipboard::default(),
        );
    }

    #[test]
    fn a_seeded_field_opens_with_all_of_it_selected() {
        let input = seeded("/home/u/a.txt");
        assert!(input.selected());
        assert_eq!(input.caret(), 13, "and the caret is still at the end");
    }

    #[test]
    fn an_empty_seed_selects_nothing() {
        // Otherwise an empty field would report `selected`, and the renderer
        // would draw an empty selection.
        assert!(!seeded("").selected());
    }

    #[test]
    fn typing_over_a_selected_field_replaces_all_of_it() {
        let mut input = seeded("/home/u/a.txt");
        typed(&mut input, "copy.txt");
        assert_eq!(input.text(), "copy.txt");
        assert_eq!(input.caret(), 8);
        assert!(!input.selected());
    }

    #[test]
    fn pasting_over_a_selected_field_replaces_all_of_it() {
        let mut clipboard = MemoryClipboard::default();
        clipboard.write("pasted");
        let mut input = seeded("original");
        input.consume("editor.action.clipboardPasteAction", None, &mut clipboard);
        assert_eq!(input.text(), "pasted");
    }

    #[test]
    fn deleting_a_selected_field_empties_it_whichever_key_is_pressed() {
        for command in [
            "deleteLeft",
            "deleteRight",
            "deleteWordLeft",
            "deleteWordRight",
        ] {
            let mut input = seeded("one two three");
            assert!(run(&mut input, command), "{command}");
            assert_eq!(input.text(), "", "{command}");
            assert_eq!(input.caret(), 0, "{command}");
            assert!(!input.selected(), "{command}");
        }
    }

    #[test]
    fn moving_the_caret_collapses_the_selection_and_keeps_the_text() {
        let mut input = seeded("a.txt");
        assert!(run(&mut input, "cursorHome"));
        assert!(!input.selected());
        assert_eq!(input.text(), "a.txt");
        assert_eq!(input.caret(), 0);
        typed(&mut input, "z");
        assert_eq!(
            input.text(),
            "za.txt",
            "editing from the caret, not replacing"
        );
    }

    #[test]
    fn select_all_selects_the_whole_line_so_the_next_key_replaces_it() {
        // Regression: select-all was consumed as a no-op, so `ctrl+a` did
        // nothing.
        let mut typing = input("foo");
        assert!(run(&mut typing, "editor.action.selectAll"));
        assert!(typing.selected());
        typed(&mut typing, "bar");
        assert_eq!(typing.text(), "bar");

        // And `ctrl+a` then backspace empties it, as it does everywhere else.
        let mut deleting = input("foo");
        run(&mut deleting, "editor.action.selectAll");
        run(&mut deleting, "deleteLeft");
        assert_eq!(deleting.text(), "");
    }

    #[test]
    fn select_all_on_an_empty_field_selects_nothing() {
        let mut input = Input::new();
        assert!(run(&mut input, "editor.action.selectAll"));
        assert!(!input.selected());
    }

    #[test]
    fn a_command_the_field_does_not_own_leaves_the_selection_alone() {
        // A command the field does not handle must not clear its selection.
        let mut input = seeded("foo");
        assert!(!run(&mut input, "cursorUp"));
        assert!(input.selected());
    }

    #[test]
    fn cutting_a_selected_field_still_takes_the_whole_line() {
        let mut clipboard = MemoryClipboard::default();
        let mut input = seeded("foo");
        input.consume("editor.action.clipboardCutAction", None, &mut clipboard);
        assert_eq!(clipboard.read(), "foo");
        assert_eq!(input.text(), "");
        assert!(!input.selected());
    }

    #[test]
    fn setting_text_outright_selects_nothing() {
        let mut input = seeded("seeded");
        input.set("replaced".to_owned());
        assert!(!input.selected());
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
