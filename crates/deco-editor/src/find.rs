//! The find widget: a query, a caret in it, and the matches it found.
//!
//! State only. Where the bar is drawn and which colours it uses are the
//! frontend's business; what the query is, which matches exist and which of them
//! the editor is sitting on are the same in a terminal and in a window, so they
//! live here and are tested here.
//!
//! # The find input is a text input
//!
//! VS Code's find box is a DOM input inside the editor. That has a consequence
//! worth copying deliberately rather than by accident: while it has focus,
//! `editorTextFocus` is false but `textInputFocus` is true, so every editing
//! command bound to `textInputFocus` — `left`, `backspace`, `ctrl+v` — resolves,
//! and the input handles it. deco has no DOM to do that layering, so
//! [`Find::consume`] does it explicitly: it claims the text-editing commands
//! while the input has the keyboard, and lets everything else through.
//!
//! That is why `ctrl+v` with the find bar open pastes into the query and not into
//! the document, and why `ctrl+z` cannot silently rewrite the file behind an open
//! find bar.
//!
//! # No selection model
//!
//! The query has a caret but no selection. `ctrl+a`, `ctrl+c` and `ctrl+x`
//! therefore act on the whole query, because there is no selection for them to
//! act on instead — and they are swallowed rather than passed through, since a
//! `ctrl+x` that cut a line out of the document while the user was editing a
//! search term would be a genuine loss.
//!
//! # No regular expressions
//!
//! [`deco_core::search`] is literal. `toggleFindRegex` is recognised so that the
//! key says the feature is missing instead of reporting an unknown command.

use deco_core::position::{Position, Range};
use deco_core::search::{self, SearchOptions};
use deco_core::Buffer;
use serde_json::Value;

use crate::commands::Clipboard;

/// The find widget's state.
#[derive(Debug, Default, Clone)]
pub struct Find {
    visible: bool,
    query: String,
    /// Caret offset within `query`, counted in characters.
    caret: usize,
    /// Case-insensitive and matching anywhere until the user says otherwise,
    /// which is what VS Code's find widget defaults to — and the opposite of
    /// `ctrl+d`, where the user selected exactly the text they meant.
    options: SearchOptions,
    /// Where the search started.
    ///
    /// Typing narrows the query, and each narrowing re-selects the first match
    /// *from here* rather than from wherever the last one landed. Without an
    /// anchor, typing `f`, `o`, `o` would walk the cursor down the file.
    origin: Position,
    /// Matches for `query`, recomputed by [`Find::refresh`].
    matches: Vec<Range>,
}

impl Find {
    /// A closed find bar with no query.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the bar is on screen and holding the keyboard.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// The query as typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Caret offset within the query, in characters.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// How the query is matched.
    pub fn options(&self) -> SearchOptions {
        self.options
    }

    /// Every match found for the current query, in document order.
    pub fn matches(&self) -> &[Range] {
        &self.matches
    }

    /// Where the search was started from.
    pub fn origin(&self) -> Position {
        self.origin
    }

    /// Opens the bar, seeding the query from `seed` when there is one.
    ///
    /// An empty seed is ignored rather than clearing a query the user already
    /// has: pressing `ctrl+f` twice should not wipe what was typed the first
    /// time.
    pub fn open(&mut self, seed: Option<String>, origin: Position) {
        self.visible = true;
        self.origin = origin;
        if let Some(seed) = seed {
            if !seed.is_empty() {
                self.query = seed;
            }
        }
        self.caret = self.query.chars().count();
    }

    /// Closes the bar, keeping the query so that `F3` still has something to
    /// search for.
    ///
    /// The match list is dropped: it describes text that may be edited the
    /// moment the editor has the keyboard back, and a stale highlight is worse
    /// than none.
    pub fn close(&mut self) {
        self.visible = false;
        self.matches.clear();
    }

    /// Replaces the query, putting the caret at the end.
    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.caret = self.query.chars().count();
    }

    /// Recomputes the match list.
    ///
    /// The caller decides when: the query changed, an option was toggled, or the
    /// document was edited. Nothing here caches across calls, because a cache
    /// keyed on nothing is a stale highlight waiting to happen.
    pub fn refresh(&mut self, buffer: &Buffer) {
        self.matches = search::find_all(buffer, &self.query, self.options);
    }

    /// The first match at or after `from`, wrapping to the start.
    ///
    /// Mirrors [`search::find_next`], over the cached list rather than a fresh
    /// scan of the document.
    pub fn first_at_or_after(&self, from: Position) -> Option<Range> {
        self.matches
            .iter()
            .find(|range| range.start >= from)
            .or_else(|| self.matches.first())
            .copied()
    }

    /// The last match ending at or before `from`, wrapping to the end.
    ///
    /// Mirrors [`search::find_previous`].
    pub fn last_at_or_before(&self, from: Position) -> Option<Range> {
        self.matches
            .iter()
            .rev()
            .find(|range| range.end <= from)
            .or_else(|| self.matches.last())
            .copied()
    }

    /// Which match `range` is, counting from one, for the `3 of 7` readout.
    ///
    /// `None` when the selection is not on a match — the user moved the cursor
    /// away, and claiming they are still on the third result would be a lie.
    pub fn ordinal(&self, range: Range) -> Option<usize> {
        self.matches
            .iter()
            .position(|candidate| *candidate == range)
            .map(|index| index + 1)
    }

    /// Turns case sensitivity on or off. Callers refresh afterwards.
    pub fn toggle_case_sensitive(&mut self) {
        self.options.case_sensitive = !self.options.case_sensitive;
    }

    /// Turns whole-word matching on or off. Callers refresh afterwards.
    pub fn toggle_whole_word(&mut self) {
        self.options.whole_word = !self.options.whole_word;
    }

    /// Applies a command to the query, if it is one the input owns.
    ///
    /// Returns whether the command was consumed. See the module docs for why
    /// this exists rather than being expressed in `when` clauses alone.
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
                // A newline in a query it cannot represent would be invisible in
                // a one-line input; `enter` is bound to the next-match command
                // anyway, so this only guards against a pasted or scripted one.
                for c in text.chars().filter(|c| *c != '\n' && *c != '\r') {
                    self.insert(c);
                }
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
            // A one-line input has no notion of the top or bottom of a document,
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
                for c in clipboard
                    .read()
                    .chars()
                    .filter(|c| *c != '\n' && *c != '\r')
                {
                    self.insert(c);
                }
                true
            }
            "editor.action.clipboardCopyAction" => {
                clipboard.write(&self.query);
                true
            }
            "editor.action.clipboardCutAction" => {
                clipboard.write(&self.query);
                self.query.clear();
                self.caret = 0;
                true
            }
            // Swallowed rather than handled. There is nothing to select, undo or
            // redo in the query — and letting these through would apply them to
            // the document, which is not where the user is looking.
            "editor.action.selectAll" | "undo" | "redo" => true,
            _ => false,
        }
    }

    /// Inserts one character at the caret.
    fn insert(&mut self, c: char) {
        let byte = self.byte_offset(self.caret);
        self.query.insert(byte, c);
        self.caret += 1;
    }

    /// Removes the character at character offset `index`.
    fn remove_at(&mut self, index: usize) {
        let byte = self.byte_offset(index);
        self.query.remove(byte);
    }

    /// Length of the query in characters.
    fn len(&self) -> usize {
        self.query.chars().count()
    }

    /// Byte offset of character offset `index`.
    ///
    /// The caret is counted in characters so that arrow keys move one visible
    /// thing at a time, but `String` is indexed in bytes, and a query containing
    /// anything outside ASCII makes the two differ.
    fn byte_offset(&self, index: usize) -> usize {
        self.query
            .char_indices()
            .nth(index)
            .map(|(byte, _)| byte)
            .unwrap_or(self.query.len())
    }

    /// Start of the word to the left of the caret.
    fn word_left(&self) -> usize {
        let chars: Vec<char> = self.query.chars().collect();
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
        let chars: Vec<char> = self.query.chars().collect();
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

/// The same word rule the rest of the editor uses.
///
/// Duplicated from `deco_core::search` rather than exposed from it: that one
/// describes the *document's* words, and a search query is not a document.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::MemoryClipboard;

    fn find(query: &str) -> Find {
        let mut find = Find::new();
        find.open(Some(query.to_owned()), Position::ZERO);
        find
    }

    /// Runs a command against the input, with a clipboard it can ignore.
    fn consume(find: &mut Find, command: &str) -> bool {
        find.consume(command, None, &mut MemoryClipboard::default())
    }

    fn typed(find: &mut Find, text: &str) {
        find.consume(
            "type",
            Some(&serde_json::json!({ "text": text })),
            &mut MemoryClipboard::default(),
        );
    }

    #[test]
    fn a_new_find_bar_is_closed_and_empty() {
        let find = Find::new();
        assert!(!find.visible());
        assert_eq!(find.query(), "");
        assert!(find.matches().is_empty());
    }

    #[test]
    fn opening_seeds_the_query_and_puts_the_caret_at_the_end() {
        let find = find("foo");
        assert!(find.visible());
        assert_eq!(find.query(), "foo");
        assert_eq!(find.caret(), 3);
    }

    #[test]
    fn opening_with_no_seed_keeps_the_previous_query() {
        let mut find = find("foo");
        find.close();
        find.open(None, Position::ZERO);
        assert_eq!(find.query(), "foo");
    }

    #[test]
    fn opening_with_an_empty_seed_keeps_the_previous_query() {
        // Pressing ctrl+f with nothing selected must not wipe what was typed
        // the last time.
        let mut find = find("foo");
        find.close();
        find.open(Some(String::new()), Position::ZERO);
        assert_eq!(find.query(), "foo");
    }

    #[test]
    fn closing_keeps_the_query_but_drops_the_matches() {
        let mut find = find("foo");
        find.refresh(&Buffer::from_text("foo foo"));
        assert_eq!(find.matches().len(), 2);
        find.close();
        assert!(!find.visible());
        assert_eq!(find.query(), "foo");
        assert!(find.matches().is_empty(), "a stale highlight is worse");
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut find = find("fo");
        typed(&mut find, "o");
        assert_eq!(find.query(), "foo");
        assert_eq!(find.caret(), 3);
    }

    #[test]
    fn typing_in_the_middle_inserts_there() {
        let mut find = find("fo");
        consume(&mut find, "cursorLeft");
        typed(&mut find, "x");
        assert_eq!(find.query(), "fxo");
        assert_eq!(find.caret(), 2);
    }

    #[test]
    fn a_newline_never_reaches_a_one_line_input() {
        let mut find = find("a");
        typed(&mut find, "b\nc\r\nd");
        assert_eq!(find.query(), "abcd");
    }

    #[test]
    fn backspace_removes_the_character_to_the_left() {
        let mut find = find("foo");
        assert!(consume(&mut find, "deleteLeft"));
        assert_eq!(find.query(), "fo");
        assert_eq!(find.caret(), 2);
    }

    #[test]
    fn backspace_at_the_start_does_nothing_but_is_still_consumed() {
        let mut find = find("foo");
        consume(&mut find, "cursorHome");
        // Consumed even though it changes nothing: passing it through would
        // delete a character out of the document instead.
        assert!(consume(&mut find, "deleteLeft"));
        assert_eq!(find.query(), "foo");
    }

    #[test]
    fn delete_removes_the_character_to_the_right() {
        let mut find = find("foo");
        consume(&mut find, "cursorHome");
        consume(&mut find, "deleteRight");
        assert_eq!(find.query(), "oo");
        assert_eq!(find.caret(), 0);
    }

    #[test]
    fn the_caret_stops_at_both_ends() {
        let mut find = find("ab");
        for _ in 0..5 {
            consume(&mut find, "cursorLeft");
        }
        assert_eq!(find.caret(), 0);
        for _ in 0..5 {
            consume(&mut find, "cursorRight");
        }
        assert_eq!(find.caret(), 2);
    }

    #[test]
    fn home_and_end_reach_the_ends_of_the_query() {
        let mut find = find("hello");
        consume(&mut find, "cursorHome");
        assert_eq!(find.caret(), 0);
        consume(&mut find, "cursorEnd");
        assert_eq!(find.caret(), 5);
        // A one-line input has no document top or bottom to go to.
        consume(&mut find, "cursorTop");
        assert_eq!(find.caret(), 0);
        consume(&mut find, "cursorBottom");
        assert_eq!(find.caret(), 5);
    }

    #[test]
    fn word_motion_crosses_one_word_at_a_time() {
        let mut find = find("foo bar");
        consume(&mut find, "cursorWordLeft");
        assert_eq!(find.caret(), 4);
        consume(&mut find, "cursorWordLeft");
        assert_eq!(find.caret(), 0);
        consume(&mut find, "cursorWordEndRight");
        assert_eq!(find.caret(), 3);
    }

    #[test]
    fn deleting_a_word_leaves_the_rest() {
        let mut find = find("foo bar");
        consume(&mut find, "deleteWordLeft");
        assert_eq!(find.query(), "foo ");
        assert_eq!(find.caret(), 4);
    }

    #[test]
    fn deleting_a_word_to_the_right_leaves_the_caret_put() {
        let mut find = find("foo bar");
        consume(&mut find, "cursorHome");
        consume(&mut find, "deleteWordRight");
        assert_eq!(find.query(), " bar");
        assert_eq!(find.caret(), 0);
    }

    #[test]
    fn a_query_outside_ascii_is_edited_by_character_not_by_byte() {
        let mut find = find("naïve");
        assert_eq!(find.caret(), 5);
        consume(&mut find, "deleteLeft");
        assert_eq!(find.query(), "naïv");
        consume(&mut find, "cursorHome");
        consume(&mut find, "cursorRight");
        consume(&mut find, "cursorRight");
        typed(&mut find, "X");
        assert_eq!(find.query(), "naXïv");
    }

    #[test]
    fn an_astral_character_is_one_caret_step() {
        let mut find = find("a😀b");
        consume(&mut find, "cursorHome");
        consume(&mut find, "cursorRight");
        consume(&mut find, "deleteRight");
        assert_eq!(find.query(), "ab", "the whole emoji should go");
    }

    #[test]
    fn paste_goes_into_the_query_not_the_document() {
        let mut find = find("");
        let mut clipboard = MemoryClipboard::default();
        clipboard.write("pasted");
        assert!(find.consume("editor.action.clipboardPasteAction", None, &mut clipboard));
        assert_eq!(find.query(), "pasted");
    }

    #[test]
    fn pasting_multiple_lines_flattens_them() {
        let mut find = find("");
        let mut clipboard = MemoryClipboard::default();
        clipboard.write("one\ntwo");
        find.consume("editor.action.clipboardPasteAction", None, &mut clipboard);
        assert_eq!(find.query(), "onetwo");
    }

    #[test]
    fn copy_takes_the_whole_query() {
        let mut find = find("foo");
        let mut clipboard = MemoryClipboard::default();
        find.consume("editor.action.clipboardCopyAction", None, &mut clipboard);
        assert_eq!(clipboard.read(), "foo");
        assert_eq!(find.query(), "foo");
    }

    #[test]
    fn cut_takes_the_whole_query_and_empties_it() {
        let mut find = find("foo");
        let mut clipboard = MemoryClipboard::default();
        find.consume("editor.action.clipboardCutAction", None, &mut clipboard);
        assert_eq!(clipboard.read(), "foo");
        assert_eq!(find.query(), "");
        assert_eq!(find.caret(), 0);
    }

    #[test]
    fn undo_and_select_all_are_swallowed_rather_than_reaching_the_document() {
        let mut find = find("foo");
        for command in ["undo", "redo", "editor.action.selectAll"] {
            assert!(consume(&mut find, command), "{command} should be consumed");
        }
        assert_eq!(find.query(), "foo");
    }

    #[test]
    fn commands_the_input_does_not_own_are_left_alone() {
        let mut find = find("foo");
        for command in [
            "editor.action.nextMatchFindAction",
            "closeFindWidget",
            "workbench.action.files.save",
            "cursorUp",
            "cursorDown",
            "editor.action.commentLine",
        ] {
            assert!(
                !consume(&mut find, command),
                "{command} should reach the editor"
            );
        }
    }

    #[test]
    fn matches_come_from_the_document_in_order() {
        let mut find = find("foo");
        find.refresh(&Buffer::from_text("foo\nbar\nfoo\n"));
        let starts: Vec<(u32, u32)> = find
            .matches()
            .iter()
            .map(|r| (r.start.line, r.start.character))
            .collect();
        assert_eq!(starts, vec![(0, 0), (2, 0)]);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let mut find = find("");
        find.refresh(&Buffer::from_text("anything"));
        assert!(find.matches().is_empty());
    }

    #[test]
    fn case_sensitivity_toggles_and_changes_what_matches() {
        let mut find = find("foo");
        let buffer = Buffer::from_text("foo FOO");
        find.refresh(&buffer);
        // Case-insensitive until asked otherwise, which is what VS Code's find
        // widget does — and the opposite of what `ctrl+d` does, because there the
        // user selected exactly this text.
        assert_eq!(find.matches().len(), 2);
        find.toggle_case_sensitive();
        find.refresh(&buffer);
        assert_eq!(find.matches().len(), 1);
    }

    #[test]
    fn whole_word_toggles_and_changes_what_matches() {
        let mut find = find("foo");
        let buffer = Buffer::from_text("foo foobar");
        find.refresh(&buffer);
        assert_eq!(find.matches().len(), 2);
        find.toggle_whole_word();
        find.refresh(&buffer);
        assert_eq!(find.matches().len(), 1);
    }

    #[test]
    fn stepping_forward_wraps_at_the_end() {
        let mut find = find("foo");
        find.refresh(&Buffer::from_text("foo\nfoo\n"));
        let second = find.first_at_or_after(Position::new(1, 0)).unwrap();
        assert_eq!(second.start, Position::new(1, 0));
        // Past the last match, so back to the first.
        let wrapped = find.first_at_or_after(Position::new(9, 0)).unwrap();
        assert_eq!(wrapped.start, Position::new(0, 0));
    }

    #[test]
    fn stepping_backwards_wraps_at_the_start() {
        let mut find = find("foo");
        find.refresh(&Buffer::from_text("foo\nfoo\n"));
        let first = find.last_at_or_before(Position::new(1, 0)).unwrap();
        assert_eq!(first.start, Position::new(0, 0));
        let wrapped = find.last_at_or_before(Position::ZERO).unwrap();
        assert_eq!(wrapped.start, Position::new(1, 0), "back to the last match");
    }

    #[test]
    fn stepping_with_no_matches_finds_nothing() {
        let mut find = find("zzz");
        find.refresh(&Buffer::from_text("foo"));
        assert!(find.first_at_or_after(Position::ZERO).is_none());
        assert!(find.last_at_or_before(Position::ZERO).is_none());
    }

    #[test]
    fn the_ordinal_counts_from_one() {
        let mut find = find("foo");
        find.refresh(&Buffer::from_text("foo\nfoo\nfoo\n"));
        let second = find.matches()[1];
        assert_eq!(find.ordinal(second), Some(2));
    }

    #[test]
    fn a_range_that_is_not_a_match_has_no_ordinal() {
        let mut find = find("foo");
        find.refresh(&Buffer::from_text("foo bar"));
        let elsewhere = Range::new(Position::new(0, 4), Position::new(0, 7));
        assert_eq!(
            find.ordinal(elsewhere),
            None,
            "the cursor moved off the match"
        );
    }
}
