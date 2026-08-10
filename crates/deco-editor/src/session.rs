//! One editor session: settings, keymap, theme, and the document being edited.

use std::path::PathBuf;

use deco_config::{EditorSettings, Scope, Settings};
use deco_keymap::{
    binding::Platform,
    keys::{Chord, Key},
    resolver, ContextKeys, Keymap, Resolution,
};
use deco_theme::ColorTheme;
use serde_json::json;

use crate::commands::{self, Clipboard, Context, MemoryClipboard, Outcome};
use crate::document::{Document, View};
use crate::find::Find;

/// The length of `text` in UTF-16 code units, which is how positions count.
fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// Why a batch of server-computed edits could not be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    /// Two of the edits covered the same text.
    ///
    /// The protocol forbids this, so a server sending it is broken. Refused
    /// rather than guessed at: picking which to honour would corrupt the file
    /// silently, and a file the user can still fix by hand is worth more than
    /// one that was quietly mangled.
    #[error("the server sent overlapping edits, which have no well-defined result")]
    Overlapping,
}

/// Which way [`Session::goto_marker`] walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Towards the end of the file.
    Next,
    /// Towards the start.
    Prev,
}

/// Everything one editor window needs.
pub struct Session {
    /// Resolved configuration, layered.
    pub settings: Settings,
    /// The active keymap.
    pub keymap: Keymap,
    /// The active colour theme.
    pub theme: ColorTheme,
    /// The open document.
    pub document: Document,
    /// The view onto it.
    pub view: View,
    /// Context keys that `when` clauses read.
    pub context: ContextKeys,
    /// Where cut and copy put their text.
    pub clipboard: Box<dyn Clipboard>,
    /// A transient message for the status bar.
    pub status: Option<String>,
    /// The find bar, open or not.
    ///
    /// Always present so that `F3` still has a query to search for after the bar
    /// is closed, which is how VS Code behaves.
    pub find: Find,
    /// Diagnostics for the open document, newest publication wins.
    ///
    /// Owned by the session rather than by the LSP client so that the frontends
    /// have one place to read from no matter where a diagnostic came from — a
    /// language server today, an extension or a linter later.
    pub diagnostics: Vec<deco_lsp::Diagnostic>,
    /// Problems found while loading the user's configuration, kept so the
    /// frontend can show them rather than failing to start.
    pub problems: Vec<String>,
}

impl Session {
    /// Builds a session from the user's configuration.
    ///
    /// Neither a broken `keybindings.json` nor a missing theme stops the editor
    /// from opening: both are reported through [`Session::problems`] and the
    /// defaults are used instead. An editor that refuses to start because of a
    /// typo in a config file cannot be used to fix that typo.
    pub fn new(settings: Settings, user_keybindings: Option<&str>, platform: Platform) -> Self {
        let (keymap, keymap_problems) = resolver::build(platform, user_keybindings);
        let mut problems: Vec<String> = keymap_problems
            .iter()
            .map(|p| format!("keybindings.json entry {}: {}", p.index, p.message))
            .collect();

        let theme_name = settings
            .get_str("workbench.colorTheme", None)
            .unwrap_or("Default Dark Modern")
            .to_owned();
        let theme = match deco_theme::defaults::builtin(&theme_name) {
            Some(theme) => theme,
            None => {
                problems.push(format!("unknown theme `{theme_name}`; using the default"));
                deco_theme::defaults::fallback_theme()
            }
        };

        let editor_settings = EditorSettings::resolve(&settings, None);
        let document = Document::untitled(editor_settings);

        let mut session = Self {
            settings,
            keymap,
            theme,
            document,
            view: View::default(),
            // Seeded from the same platform the keymap was built for, so a
            // `!isMac` binding cannot be chosen and then gated out.
            context: ContextKeys::for_platform(platform),
            clipboard: Box::new(MemoryClipboard::default()),
            status: None,
            find: Find::new(),
            diagnostics: Vec::new(),
            problems,
        };
        session.refresh_context();
        session
    }

    /// A session with only the built-in defaults.
    pub fn with_defaults() -> Self {
        Self::new(Settings::with_defaults(), None, Platform::host())
    }

    /// Replaces the open document.
    pub fn open(&mut self, path: PathBuf, text: &str) {
        let language = crate::document::language_for_path(&path);
        let settings = EditorSettings::resolve(&self.settings, language);
        self.document = Document::from_file(path, text, settings);
        self.view = View {
            height: self.view.height,
            width: self.view.width,
            ..Default::default()
        };
        // The previous document's diagnostics point at line numbers in a file
        // that is no longer on screen. Carrying them over would decorate the
        // new one with the old one's errors.
        self.diagnostics.clear();
        // Same reasoning for the match list. The query survives, since searching
        // the next file for the same thing is a reasonable thing to want.
        self.find.close();
        self.refresh_context();
    }

    /// Replaces the diagnostics for the open document.
    ///
    /// Replace rather than append, matching the protocol: a server publishes
    /// the complete set for a document each time, and an empty set is how it
    /// says the problems are fixed.
    pub fn set_diagnostics(&mut self, diagnostics: Vec<deco_lsp::Diagnostic>) {
        self.diagnostics = diagnostics;
        self.refresh_context();
    }

    /// The diagnostics under a position, worst first.
    pub fn diagnostics_at(
        &self,
        position: deco_core::position::Position,
    ) -> Vec<&deco_lsp::Diagnostic> {
        let mut hits: Vec<&deco_lsp::Diagnostic> = self
            .diagnostics
            .iter()
            .filter(|d| d.contains(position))
            .collect();
        hits.sort_by_key(|d| d.severity);
        hits
    }

    /// How many diagnostics of each severity the open document has.
    pub fn diagnostic_counts(&self) -> deco_lsp::diagnostics::Counts {
        let mut counts = deco_lsp::diagnostics::Counts::default();
        for diagnostic in &self.diagnostics {
            match diagnostic.severity {
                deco_lsp::Severity::Error => counts.errors += 1,
                deco_lsp::Severity::Warning => counts.warnings += 1,
                deco_lsp::Severity::Information => counts.information += 1,
                deco_lsp::Severity::Hint => counts.hints += 1,
            }
        }
        counts
    }

    /// Installs a workspace settings layer, then re-resolves everything that
    /// depends on it.
    pub fn set_workspace_settings(&mut self, source: &str) {
        if let Err(error) = self.settings.load_layer(Scope::Workspace, source) {
            self.problems.push(format!("workspace settings: {error}"));
            return;
        }
        self.document.settings = EditorSettings::resolve(&self.settings, self.document.language());
    }

    /// Recomputes the context keys `when` clauses read.
    ///
    /// Called after anything that changes focus, selection or the document, so
    /// that a binding gated on `editorHasSelection` becomes active in the same
    /// frame the selection appears.
    pub fn refresh_context(&mut self) {
        let selections = &self.view.selections;
        // VS Code's own distinction, and the find bar depends on it:
        // `editorTextFocus` is the text area, `textInputFocus` is any text input
        // including the find box, and `editorFocus` covers the editor as a whole.
        // So with the find bar open, `tab` and `ctrl+space` stop resolving while
        // `left` and `backspace` keep doing so — and `Find::consume` claims them.
        let find_focus = self.find.visible();
        self.context.set("editorTextFocus", !find_focus);
        self.context.set("editorFocus", true);
        self.context.set("textInputFocus", true);
        self.context.set("findWidgetVisible", find_focus);
        self.context.set("findInputFocussed", find_focus);
        self.context.set("editorReadonly", false);
        self.context.set(
            "editorHasSelection",
            selections.iter().any(|s| !s.is_empty()),
        );
        self.context
            .set("editorHasMultipleSelections", selections.is_multi());
        self.context.set("dirty", self.document.dirty);
        // VS Code's own key, so a `when` clause copied from an existing
        // keybindings.json means the same thing here. `gotoNextError` is bound
        // to it by default.
        self.context
            .set("editorHasDiagnostics", !self.diagnostics.is_empty());
        match self.document.language() {
            Some(language) => self.context.set("editorLangId", language),
            None => self.context.remove("editorLangId"),
        }
    }

    /// Feeds one keypress through the keymap and runs whatever it resolves to.
    ///
    /// `now_ms` is a monotonic timestamp used for undo grouping; the frontend
    /// owns the clock so that this whole path stays testable.
    pub fn handle_chord(&mut self, chord: Chord, now_ms: u64) -> Outcome {
        let resolution = self
            .keymap
            .resolve(&mut self.view.chord, chord, &self.context);

        let outcome = match resolution {
            Resolution::Pending { .. } => Outcome::Handled,
            Resolution::Match { command, args } => self.dispatch(&command, args.as_ref(), now_ms),
            Resolution::NoMatch => {
                // An unbound printable key types itself. Modifiers other than
                // Shift mean the user was reaching for a command, so those are
                // left alone rather than inserting stray characters.
                match chord.key {
                    Key::Char(c)
                        if !chord.modifiers.ctrl
                            && !chord.modifiers.meta
                            && !chord.modifiers.alt =>
                    {
                        let text = if chord.modifiers.shift {
                            c.to_uppercase().to_string()
                        } else {
                            c.to_string()
                        };
                        self.dispatch("type", Some(&json!({ "text": text })), now_ms)
                    }
                    _ => Outcome::NotFound,
                }
            }
        };

        self.refresh_context();
        outcome
    }

    /// Runs a command, giving the find input first refusal on it.
    ///
    /// The find input is a text input, so while it holds the keyboard the
    /// text-editing commands belong to it — see the [`crate::find`] module for
    /// why that cannot be expressed as a `when` clause. Everything else, and
    /// everything at all when the bar is closed, goes to [`Session::run`].
    fn dispatch(
        &mut self,
        command: &str,
        args: Option<&serde_json::Value>,
        now_ms: u64,
    ) -> Outcome {
        if self.find.visible() && self.find.consume(command, args, self.clipboard.as_mut()) {
            return self.find_query_changed();
        }
        self.run(command, args, now_ms)
    }

    /// Runs a command by identifier.
    pub fn run(&mut self, command: &str, args: Option<&serde_json::Value>, now_ms: u64) -> Outcome {
        // Diagnostic navigation is handled here rather than in `commands`
        // because it needs the diagnostic list, which belongs to the session —
        // a command sees only the document, the view and the clipboard.
        let outcome = match command {
            "editor.action.marker.next" | "editor.action.marker.nextInFiles" => {
                self.goto_marker(Direction::Next)
            }
            "editor.action.marker.prev" | "editor.action.marker.prevInFiles" => {
                self.goto_marker(Direction::Prev)
            }
            // The find bar, for the same reason: it needs the whole document and
            // its own state, neither of which a command in `commands` can see.
            "actions.find" => self.open_find(),
            "closeFindWidget" => {
                self.find.close();
                Outcome::Handled
            }
            "editor.action.nextMatchFindAction" => self.step_find(Direction::Next),
            "editor.action.previousMatchFindAction" => self.step_find(Direction::Prev),
            "toggleFindCaseSensitive" => {
                self.find.toggle_case_sensitive();
                self.find_query_changed()
            }
            "toggleFindWholeWord" => {
                self.find.toggle_whole_word();
                self.find_query_changed()
            }
            // Recognised so the key says what is missing. `deco_core::search` is
            // literal, and a regex mode needs its own escaping and its own error
            // reporting for an invalid pattern.
            "toggleFindRegex" => {
                Outcome::Message("regular-expression search is not implemented yet".to_owned())
            }
            "editor.action.startFindReplaceAction" => {
                Outcome::Message("replace is not implemented yet".to_owned())
            }
            // Commands that need something the core has no concept of. Named
            // here rather than left to fall through as `NotFound`, so a typo in
            // a keybinding is still reported as unknown.
            "editor.action.showHover"
            | "editor.action.revealDefinition"
            | "editor.action.goToReferences"
            | "editor.action.triggerSuggest"
            | "editor.action.formatDocument"
            | "editor.action.formatSelection"
            | "acceptSelectedSuggestion"
            | "selectNextSuggestion"
            | "selectPrevSuggestion"
            | "hideSuggestWidget"
            | "closeHoverWidget" => Outcome::Frontend(command.to_owned()),
            _ => {
                let mut ctx = Context {
                    document: &mut self.document,
                    view: &mut self.view,
                    clipboard: self.clipboard.as_mut(),
                    now_ms,
                };
                commands::execute(&mut ctx, command, args)
            }
        };
        // Shared deliberately: a command handled above must report to the
        // status bar the same way every other one does, or F8 lands on an error
        // and says nothing about it.
        if let Outcome::Message(message) = &outcome {
            self.status = Some(message.clone());
        }
        self.refresh_context();
        outcome
    }

    /// Opens the find bar, seeding it from the selection.
    ///
    /// `editor.find.seedSearchStringFromSelection` is on by default in VS Code,
    /// and the reason is that selecting a word and pressing `ctrl+f` is how the
    /// find bar is usually reached.
    fn open_find(&mut self) -> Outcome {
        let primary = *self.view.selections.primary();
        let seed =
            (!primary.is_empty()).then(|| self.document.buffer.text_in_range(primary.range()));
        // The start of the selection, not the cursor: seeding from a selection
        // must leave that same occurrence as the current match rather than
        // skipping to the next one.
        self.find.open(seed, primary.start());
        self.find_query_changed()
    }

    /// Re-finds the matches and moves to the first one from the search origin.
    ///
    /// Called after anything that changes what matches: a keystroke in the query,
    /// a toggled option. Searching from the origin rather than from the cursor is
    /// what stops typing `f`, `o`, `o` from walking down the file one match at a
    /// time.
    fn find_query_changed(&mut self) -> Outcome {
        self.find.refresh(&self.document.buffer);
        if let Some(range) = self.find.first_at_or_after(self.find.origin()) {
            self.select_match(range);
        }
        Outcome::Handled
    }

    /// `F3` and `shift+F3`: the next or previous match, wrapping.
    fn step_find(&mut self, direction: Direction) -> Outcome {
        // `F3` with nothing typed yet searches for the selection, or for the word
        // under the cursor — which is what makes it useful without `ctrl+f`
        // first.
        if self.find.query().is_empty() {
            let Some((seed, range)) = self.seed_from_document() else {
                return Outcome::Message("nothing to search for".to_owned());
            };
            self.find.set_query(seed);
            // Select the seed, so that the step below moves off it. Without this
            // the search starts at a bare caret sitting inside the very word it
            // just seeded from, finds that word, and appears to do nothing.
            self.select_match(range);
        }
        self.find.refresh(&self.document.buffer);
        if self.find.matches().is_empty() {
            return Outcome::Message(format!("no results for `{}`", self.find.query()));
        }

        let primary = *self.view.selections.primary();
        // From the far end of the selection in the direction of travel, so that
        // pressing the key while sitting on a match moves off it instead of
        // finding it again.
        let found = match direction {
            Direction::Next => self.find.first_at_or_after(primary.end()),
            Direction::Prev => self.find.last_at_or_before(primary.start()),
        };
        let Some(range) = found else {
            return Outcome::Handled;
        };
        self.select_match(range);
        // The bar shows the count when it is open; when it is closed this is the
        // only place the user learns whether the search wrapped or found nothing.
        match self.find.ordinal(range) {
            Some(ordinal) if !self.find.visible() => Outcome::Message(format!(
                "{ordinal} of {} for `{}`",
                self.find.matches().len(),
                self.find.query()
            )),
            _ => Outcome::Handled,
        }
    }

    /// The text `F3` should search for when the query is still empty, and where
    /// in the document it came from.
    ///
    /// The range matters as much as the text: it is the match the cursor is
    /// already on, and `F3` has to step off it rather than onto it.
    fn seed_from_document(&self) -> Option<(String, deco_core::position::Range)> {
        let primary = *self.view.selections.primary();
        if !primary.is_empty() {
            let range = primary.range();
            let text = self.document.buffer.text_in_range(range);
            return (!text.is_empty()).then_some((text, range));
        }
        let word = deco_core::search::word_at(&self.document.buffer, primary.active)?;
        Some((self.document.buffer.text_in_range(word), word))
    }

    /// Selects `range` and scrolls it into view.
    fn select_match(&mut self, range: deco_core::position::Range) {
        use deco_core::selection::{Selection, SelectionSet};
        self.view.selections = SelectionSet::single(Selection::new(range.start, range.end));
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);
    }

    /// Moves the cursor to the next or previous diagnostic.
    ///
    /// Wraps around, as VS Code's does: reaching the last error and pressing F8
    /// again returns to the first rather than doing nothing, which is what
    /// makes it usable for walking a file repeatedly.
    fn goto_marker(&mut self, direction: Direction) -> Outcome {
        if self.diagnostics.is_empty() {
            return Outcome::Message("no problems in this file".into());
        }

        // Sorted rather than taken in publication order: servers emit in
        // whatever order analysis finished, and "next" has to mean next in the
        // file or the cursor jumps around unpredictably.
        let mut starts: Vec<deco_core::position::Position> =
            self.diagnostics.iter().map(|d| d.range.start).collect();
        starts.sort();
        starts.dedup();

        let cursor = self.view.selections.primary().active;
        let target = match direction {
            Direction::Next => starts
                .iter()
                .find(|start| **start > cursor)
                .copied()
                .unwrap_or(starts[0]),
            Direction::Prev => starts
                .iter()
                .rev()
                .find(|start| **start < cursor)
                .copied()
                .unwrap_or(starts[starts.len() - 1]),
        };

        // Clamped because a diagnostic can outlive the text it describes: the
        // user may have deleted the offending lines before the server caught
        // up, and an unclamped position would panic or scroll past the end.
        let target = self.document.buffer.clamp_position(target);
        self.view.selections = deco_core::selection::SelectionSet::single(
            deco_core::selection::Selection::caret(target),
        );
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);

        let message = match self.diagnostics_at(target).first() {
            Some(diagnostic) => diagnostic.label(),
            None => format!("line {}", target.line + 1),
        };
        Outcome::Message(message)
    }

    /// Replaces a range with `text`, leaving the cursor after it.
    ///
    /// The seam a frontend needs to apply an edit it computed itself — accepting
    /// a completion, applying a formatting result — rather than through a
    /// command. It goes through the same transaction and history machinery as
    /// every other edit, so the result is one undo step and the document's dirty
    /// flag is correct.
    ///
    /// `Discrete` rather than typed: accepting a completion is one decision, and
    /// coalescing it with the characters typed just before would make a single
    /// undo throw away the word as well as the completion.
    pub fn replace_range(
        &mut self,
        range: deco_core::position::Range,
        text: &str,
        now_ms: u64,
    ) -> deco_core::position::Position {
        use deco_core::{Change, EditKind, Selection, SelectionSet, Transaction};

        let before = self.view.selections.clone();
        // Clamped because the range may have been computed against text the user
        // has since changed — a completion answered while they kept typing.
        let range = deco_core::position::Range::new(
            self.document.buffer.clamp_position(range.start),
            self.document.buffer.clamp_position(range.end),
        );

        let transaction = Transaction::single(Change::replace(range, text.to_owned()));
        let inverse = self.document.buffer.apply(&transaction);

        // Where the inserted text ends, which is where a caret belongs after an
        // insertion — computed from the text rather than by re-searching the
        // buffer, so it is right even when the text contains newlines.
        let end = match text.rfind('\n') {
            Some(last_break) => {
                let lines_added = text.matches('\n').count() as u32;
                let tail = &text[last_break + 1..];
                deco_core::position::Position::new(range.start.line + lines_added, utf16_len(tail))
            }
            None => deco_core::position::Position::new(
                range.start.line,
                range.start.character + utf16_len(text),
            ),
        };
        let end = self.document.buffer.clamp_position(end);

        let after = SelectionSet::single(Selection::caret(end));
        self.view.selections = after.clone();
        self.document
            .history
            .record(inverse, EditKind::Discrete, before, after, now_ms);
        self.document.dirty = true;
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);
        self.refresh_context();
        end
    }

    /// Applies a batch of server-computed replacements as one undo step.
    ///
    /// Every range refers to the document as the server saw it, and the protocol
    /// says nothing about the order they arrive in — so applying them front to
    /// back would corrupt the file, because the first edit shifts every position
    /// after it. [`deco_core::Transaction`] sorts them and applies back to front,
    /// which is why they are handed over as one batch rather than looped over.
    ///
    /// Returns how many edits were applied, or an error naming the reason when
    /// none could be.
    ///
    /// The cursor is kept where it was, clamped into the new text. A formatting
    /// run that moved the caret to the end of the file would be correct by the
    /// letter of the edits and useless in practice.
    pub fn apply_edits(
        &mut self,
        edits: &[deco_lsp::TextEdit],
        now_ms: u64,
    ) -> Result<usize, EditError> {
        use deco_core::{Change, EditKind, Transaction};

        // A server routinely answers an already-formatted document with a no-op
        // edit. Applying one would mark the file dirty and add an undo step for
        // nothing.
        let changes: Vec<Change> = edits
            .iter()
            .filter(|edit| !edit.is_noop())
            .map(|edit| {
                Change::replace(
                    deco_core::position::Range::new(
                        self.document.buffer.clamp_position(edit.range.start),
                        self.document.buffer.clamp_position(edit.range.end),
                    ),
                    edit.new_text.clone(),
                )
            })
            .collect();

        if changes.is_empty() {
            return Ok(0);
        }

        let applied = changes.len();
        // Overlapping edits have no well-defined result. The specification
        // forbids them, so a server sending them is broken — and guessing which
        // to honour would corrupt the file silently, which is worse than
        // refusing and saying so.
        let transaction = Transaction::new(changes).map_err(|_| EditError::Overlapping)?;

        let before = self.view.selections.clone();
        let inverse = self.document.buffer.apply(&transaction);

        let cursor = self.document.buffer.clamp_position(before.primary().active);
        let after = deco_core::SelectionSet::caret(cursor);
        self.view.selections = after.clone();
        self.document
            .history
            .record(inverse, EditKind::Discrete, before, after, now_ms);
        self.document.dirty = true;
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);
        self.refresh_context();
        Ok(applied)
    }

    /// The formatting options a language server should be told about.
    ///
    /// The user's own, resolved for the open document's language — so a server
    /// formats to the project's indentation rather than to its own defaults.
    pub fn formatting_options(&self) -> deco_lsp::FormattingOptions {
        let settings = &self.document.settings;
        deco_lsp::FormattingOptions {
            tab_size: settings.tab_size.clamp(1, u32::MAX as usize) as u32,
            insert_spaces: settings.insert_spaces,
            trim_trailing_whitespace: settings.trim_trailing_whitespace,
            insert_final_newline: settings.insert_final_newline,
        }
    }

    /// Text to write to disk for the open document.
    pub fn save_contents(&self) -> String {
        let mut text = self.document.buffer.to_disk_string();
        if self.document.settings.insert_final_newline && !text.ends_with('\n') {
            let eol = self.document.buffer.line_ending().as_str();
            text.push_str(eol);
        }
        text
    }

    /// Marks the document as saved.
    pub fn mark_saved(&mut self) {
        self.document.dirty = false;
        self.document.history.break_group();
        self.refresh_context();
    }

    /// Tells the session how large the text area is.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.view.width = width;
        self.view.height = height;
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_core::Position;

    fn session() -> Session {
        Session::new(Settings::with_defaults(), None, Platform::Linux)
    }

    fn press(session: &mut Session, key: &str) -> Outcome {
        session.handle_chord(Chord::parse(key).unwrap(), 0)
    }

    #[test]
    fn a_default_session_starts_clean() {
        let session = session();
        assert!(session.problems.is_empty(), "{:?}", session.problems);
        assert_eq!(session.document.title(), "Untitled");
        assert_eq!(session.theme.name, "Default Dark Modern");
    }

    #[test]
    fn an_unbound_printable_key_types_itself() {
        let mut s = session();
        press(&mut s, "h");
        press(&mut s, "i");
        assert_eq!(s.document.buffer.text(), "hi");
    }

    #[test]
    fn shift_types_an_uppercase_character() {
        let mut s = session();
        press(&mut s, "shift+a");
        assert_eq!(s.document.buffer.text(), "A");
    }

    #[test]
    fn an_unbound_control_chord_types_nothing() {
        let mut s = session();
        assert_eq!(press(&mut s, "ctrl+alt+shift+j"), Outcome::NotFound);
        assert_eq!(s.document.buffer.text(), "");
    }

    #[test]
    fn bound_keys_reach_their_command() {
        let mut s = session();
        press(&mut s, "a");
        press(&mut s, "b");
        assert_eq!(s.document.buffer.text(), "ab");

        press(&mut s, "ctrl+z");
        assert_eq!(s.document.buffer.text(), "", "ctrl+z should undo");
    }

    #[test]
    fn save_and_quit_reach_the_frontend() {
        let mut s = session();
        assert_eq!(press(&mut s, "ctrl+s"), Outcome::Save);
        assert_eq!(press(&mut s, "ctrl+q"), Outcome::Quit);
    }

    #[test]
    fn a_chord_takes_two_presses() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "let x = 1;");
        assert_eq!(press(&mut s, "ctrl+k"), Outcome::Handled);
        assert!(s.view.chord.pending().is_some());
        press(&mut s, "ctrl+c");
        assert!(s.view.chord.pending().is_none());
        assert_eq!(s.document.buffer.text(), "// let x = 1;");
    }

    #[test]
    fn context_keys_track_the_selection() {
        let mut s = session();
        press(&mut s, "a");
        assert_eq!(s.context.get("editorHasSelection"), Some(&json!(false)));
        press(&mut s, "ctrl+a");
        assert_eq!(s.context.get("editorHasSelection"), Some(&json!(true)));
    }

    #[test]
    fn context_keys_track_the_language() {
        let mut s = session();
        assert_eq!(s.context.get("editorLangId"), None);
        s.open(PathBuf::from("/w/main.rs"), "");
        assert_eq!(s.context.get("editorLangId"), Some(&json!("rust")));
    }

    #[test]
    fn user_keybindings_override_the_defaults() {
        let user = r#"[{ "key": "ctrl+s", "command": "workbench.action.quit" }]"#;
        let mut s = Session::new(Settings::with_defaults(), Some(user), Platform::Linux);
        assert!(s.problems.is_empty(), "{:?}", s.problems);
        assert_eq!(press(&mut s, "ctrl+s"), Outcome::Quit);
    }

    #[test]
    fn a_broken_keybindings_file_is_reported_but_not_fatal() {
        let user = r#"[{ "key": "ctrl+nonsense", "command": "x" }]"#;
        let mut s = Session::new(Settings::with_defaults(), Some(user), Platform::Linux);
        assert_eq!(s.problems.len(), 1);
        // The defaults still work.
        assert_eq!(press(&mut s, "ctrl+s"), Outcome::Save);
    }

    #[test]
    fn an_unknown_theme_falls_back_and_is_reported() {
        let mut settings = Settings::with_defaults();
        settings.set(
            Scope::User,
            "workbench.colorTheme",
            json!("Nonexistent Theme"),
        );
        let s = Session::new(settings, None, Platform::Linux);
        assert_eq!(s.theme.name, "Default Dark Modern");
        assert_eq!(s.problems.len(), 1);
    }

    #[test]
    fn language_specific_settings_apply_to_the_open_document() {
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                Scope::User,
                r#"{"editor.tabSize": 2, "[go]": {"editor.tabSize": 8}}"#,
            )
            .unwrap();
        let mut s = Session::new(settings, None, Platform::Linux);

        s.open(PathBuf::from("/w/main.go"), "");
        assert_eq!(s.document.settings.tab_size, 8);

        s.open(PathBuf::from("/w/main.rs"), "");
        assert_eq!(s.document.settings.tab_size, 2);
    }

    #[test]
    fn saving_round_trips_crlf() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.txt"), "one\r\ntwo\r\n");
        assert_eq!(s.save_contents(), "one\r\ntwo\r\n");
    }

    #[test]
    fn insert_final_newline_adds_one_when_asked() {
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "files.insertFinalNewline", json!(true));
        settings.set(Scope::User, "files.eol", json!("\n"));
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.txt"), "no newline");
        assert_eq!(s.save_contents(), "no newline\n");
    }

    #[test]
    fn marking_saved_clears_the_dirty_flag() {
        let mut s = session();
        press(&mut s, "x");
        assert!(s.document.dirty);
        s.mark_saved();
        assert!(!s.document.dirty);
    }

    #[test]
    fn resizing_keeps_the_cursor_visible() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.txt"), &"x\n".repeat(100));
        s.view.selections = deco_core::SelectionSet::caret(Position::new(90, 0));
        s.resize(80, 20);
        assert!(s.view.visible_lines(&s.document.buffer).contains(&90));
    }

    /// A diagnostic spanning one line, from `character` 0 to 4.
    fn diagnostic(line: u32, severity: deco_lsp::Severity, message: &str) -> deco_lsp::Diagnostic {
        deco_lsp::Diagnostic {
            range: deco_core::position::Range::new(Position::new(line, 0), Position::new(line, 4)),
            severity,
            code: None,
            source: None,
            message: message.into(),
        }
    }

    fn with_diagnostics(lines: &[u32]) -> Session {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), &"line\n".repeat(20));
        s.set_diagnostics(
            lines
                .iter()
                .map(|line| diagnostic(*line, deco_lsp::Severity::Error, "boom"))
                .collect(),
        );
        s
    }

    fn cursor_line(s: &Session) -> u32 {
        s.view.selections.primary().active.line
    }

    #[test]
    fn diagnostics_set_the_context_key_vscode_uses() {
        // So a `when` clause copied from an existing keybindings.json means the
        // same thing here.
        let mut s = with_diagnostics(&[]);
        assert_eq!(s.context.get("editorHasDiagnostics"), Some(&json!(false)));
        s.set_diagnostics(vec![diagnostic(1, deco_lsp::Severity::Error, "x")]);
        assert_eq!(s.context.get("editorHasDiagnostics"), Some(&json!(true)));
    }

    #[test]
    fn publishing_replaces_rather_than_appends() {
        // The protocol is replace-per-document; appending would double every
        // error each time the file is analysed.
        let mut s = with_diagnostics(&[1, 2, 3]);
        s.set_diagnostics(vec![diagnostic(9, deco_lsp::Severity::Error, "only")]);
        assert_eq!(s.diagnostics.len(), 1);
    }

    #[test]
    fn opening_another_file_drops_the_previous_ones_diagnostics() {
        // They point at line numbers in a file that is no longer on screen.
        let mut s = with_diagnostics(&[1, 2]);
        s.open(PathBuf::from("/w/b.rs"), "fn other() {}");
        assert!(s.diagnostics.is_empty());
        assert_eq!(s.context.get("editorHasDiagnostics"), Some(&json!(false)));
    }

    #[test]
    fn f8_walks_forward_through_the_problems() {
        let mut s = with_diagnostics(&[2, 5, 9]);
        for expected in [2, 5, 9] {
            s.run("editor.action.marker.next", None, 0);
            assert_eq!(cursor_line(&s), expected);
        }
    }

    #[test]
    fn f8_wraps_around_at_the_end() {
        // Reaching the last error and pressing again returns to the first;
        // doing nothing would make it useless for a second pass.
        let mut s = with_diagnostics(&[2, 5]);
        s.view.selections = deco_core::SelectionSet::caret(Position::new(19, 0));
        s.run("editor.action.marker.next", None, 0);
        assert_eq!(cursor_line(&s), 2);
    }

    #[test]
    fn shift_f8_walks_backwards_and_wraps() {
        let mut s = with_diagnostics(&[2, 5, 9]);
        s.view.selections = deco_core::SelectionSet::caret(Position::new(9, 0));
        s.run("editor.action.marker.prev", None, 0);
        assert_eq!(cursor_line(&s), 5);
        s.run("editor.action.marker.prev", None, 0);
        assert_eq!(cursor_line(&s), 2);
        s.run("editor.action.marker.prev", None, 0);
        assert_eq!(cursor_line(&s), 9, "wraps to the last");
    }

    #[test]
    fn navigation_visits_problems_in_file_order_not_publication_order() {
        // Servers emit in whatever order analysis finished.
        let mut s = with_diagnostics(&[]);
        s.set_diagnostics(vec![
            diagnostic(9, deco_lsp::Severity::Error, "third"),
            diagnostic(2, deco_lsp::Severity::Error, "first"),
            diagnostic(5, deco_lsp::Severity::Error, "second"),
        ]);
        s.run("editor.action.marker.next", None, 0);
        assert_eq!(cursor_line(&s), 2);
    }

    #[test]
    fn navigation_reports_the_diagnostic_it_landed_on() {
        let mut s = with_diagnostics(&[]);
        s.set_diagnostics(vec![deco_lsp::Diagnostic {
            source: Some("rustc".into()),
            code: Some("E0308".into()),
            ..diagnostic(3, deco_lsp::Severity::Error, "mismatched types")
        }]);
        s.run("editor.action.marker.next", None, 0);
        assert_eq!(s.status.as_deref(), Some("rustc[E0308]: mismatched types"));
    }

    #[test]
    fn navigation_says_so_when_there_is_nothing_to_visit() {
        let mut s = with_diagnostics(&[]);
        let before = cursor_line(&s);
        s.run("editor.action.marker.next", None, 0);
        assert_eq!(cursor_line(&s), before, "the cursor must not move");
        assert_eq!(s.status.as_deref(), Some("no problems in this file"));
    }

    #[test]
    fn a_diagnostic_past_the_end_of_the_file_is_clamped() {
        // A server can be a moment behind: the user deletes the offending lines
        // before it recomputes, and its ranges outlive the text.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "one\ntwo\n");
        s.set_diagnostics(vec![diagnostic(900, deco_lsp::Severity::Error, "stale")]);
        s.run("editor.action.marker.next", None, 0);
        assert!(
            cursor_line(&s) <= 2,
            "the cursor landed outside the document: {}",
            cursor_line(&s)
        );
    }

    #[test]
    fn the_diagnostics_under_the_cursor_come_back_worst_first() {
        let mut s = with_diagnostics(&[]);
        s.set_diagnostics(vec![
            diagnostic(3, deco_lsp::Severity::Hint, "hint"),
            diagnostic(3, deco_lsp::Severity::Error, "error"),
            diagnostic(8, deco_lsp::Severity::Error, "elsewhere"),
        ]);
        let hits = s.diagnostics_at(Position::new(3, 1));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].message, "error");
    }

    #[test]
    fn counts_tally_by_severity() {
        let mut s = with_diagnostics(&[]);
        s.set_diagnostics(vec![
            diagnostic(1, deco_lsp::Severity::Error, "a"),
            diagnostic(2, deco_lsp::Severity::Error, "b"),
            diagnostic(3, deco_lsp::Severity::Warning, "c"),
            diagnostic(4, deco_lsp::Severity::Hint, "d"),
        ]);
        let counts = s.diagnostic_counts();
        assert_eq!(counts.errors, 2);
        assert_eq!(counts.warnings, 1);
        assert_eq!(counts.hints, 1);
        assert_eq!(counts.total(), 4);
    }

    #[test]
    fn f8_is_bound_by_default() {
        let mut s = with_diagnostics(&[4]);
        press(&mut s, "f8");
        assert_eq!(cursor_line(&s), 4);
    }

    #[test]
    fn replacing_a_range_leaves_the_cursor_after_the_text() {
        // Where a caret belongs after accepting a completion.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "let x = Has;\n");
        let end = s.replace_range(
            deco_core::position::Range::new(Position::new(0, 8), Position::new(0, 11)),
            "HashMap",
            0,
        );
        assert_eq!(
            s.document.buffer.line_content(0).unwrap(),
            "let x = HashMap;"
        );
        assert_eq!(end, Position::new(0, 15));
        assert_eq!(s.view.selections.primary().active, end);
        assert!(s.document.dirty);
    }

    #[test]
    fn replacing_a_range_is_one_undo_step() {
        // Accepting a completion is one decision; coalescing it with the word
        // typed before would make a single undo throw both away.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "Has\n");
        s.replace_range(
            deco_core::position::Range::new(Position::new(0, 0), Position::new(0, 3)),
            "HashMap",
            0,
        );
        s.run("undo", None, 0);
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "Has");
    }

    #[test]
    fn replacing_with_multiline_text_puts_the_cursor_on_the_last_line() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "x\n");
        let end = s.replace_range(
            deco_core::position::Range::new(Position::new(0, 0), Position::new(0, 1)),
            "if a {\n    b\n}",
            0,
        );
        assert_eq!(end, Position::new(2, 1));
        assert_eq!(s.view.selections.primary().active, end);
    }

    #[test]
    fn the_cursor_lands_correctly_after_text_outside_the_bmp() {
        // Positions count UTF-16 units, so an emoji advances the column by two.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "\n");
        let end = s.replace_range(
            deco_core::position::Range::new(Position::ZERO, Position::ZERO),
            "ab🎉",
            0,
        );
        assert_eq!(end.character, 4, "two units for the emoji");
    }

    #[test]
    fn a_range_past_the_end_of_the_document_is_clamped() {
        // The range may have been computed against text the user has since
        // changed — a completion answered while they kept typing.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "ab\n");
        s.replace_range(
            deco_core::position::Range::new(Position::new(0, 1), Position::new(99, 99)),
            "Z",
            0,
        );
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "aZ");
    }

    #[test]
    fn an_empty_range_inserts_without_deleting() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "ac\n");
        s.replace_range(
            deco_core::position::Range::empty(Position::new(0, 1)),
            "b",
            0,
        );
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "abc");
    }

    fn edit(line: u32, from: u32, to: u32, text: &str) -> deco_lsp::TextEdit {
        deco_lsp::TextEdit {
            range: deco_core::position::Range::new(
                Position::new(line, from),
                Position::new(line, to),
            ),
            new_text: text.to_owned(),
        }
    }

    #[test]
    fn edits_are_applied_back_to_front_whatever_order_they_arrive_in() {
        // The trap this exists for: every range refers to the document the
        // server saw, and applying them front to back shifts every position
        // after the first edit. Given deliberately out of order.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "aaaa bbbb cccc\n");
        let applied = s
            .apply_edits(
                &[
                    edit(0, 10, 14, "THREE"),
                    edit(0, 0, 4, "ONE"),
                    edit(0, 5, 9, "TWO"),
                ],
                0,
            )
            .unwrap();

        assert_eq!(applied, 3);
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "ONE TWO THREE");
    }

    #[test]
    fn a_whole_batch_is_one_undo_step() {
        // A formatting run is one decision; undoing it a line at a time would be
        // unusable on a file the server reflowed.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "aaaa bbbb\n");
        s.apply_edits(&[edit(0, 0, 4, "x"), edit(0, 5, 9, "y")], 0)
            .unwrap();
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "x y");

        s.run("undo", None, 0);
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "aaaa bbbb");
    }

    #[test]
    fn edits_spanning_lines_are_applied_correctly() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "one\ntwo\nthree\nfour\n");
        s.apply_edits(
            &[
                deco_lsp::TextEdit {
                    range: deco_core::position::Range::new(
                        Position::new(2, 0),
                        Position::new(3, 4),
                    ),
                    new_text: "THREE-FOUR".into(),
                },
                edit(0, 0, 3, "ONE"),
            ],
            0,
        )
        .unwrap();
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "ONE");
        assert_eq!(s.document.buffer.line_content(2).unwrap(), "THREE-FOUR");
    }

    #[test]
    fn an_already_formatted_document_is_left_alone() {
        // Servers answer with a no-op edit for this. Applying one would mark the
        // file dirty and add an undo step for nothing.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "fine\n");
        let applied = s.apply_edits(&[edit(0, 2, 2, "")], 0).unwrap();
        assert_eq!(applied, 0);
        assert!(!s.document.dirty, "nothing changed, so nothing is dirty");
    }

    #[test]
    fn an_empty_edit_list_changes_nothing() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "fine\n");
        assert_eq!(s.apply_edits(&[], 0).unwrap(), 0);
        assert!(!s.document.dirty);
    }

    #[test]
    fn overlapping_edits_are_refused_and_the_file_is_untouched() {
        // The protocol forbids them, so a server sending them is broken. Picking
        // one to honour would corrupt the file silently, and a file the user can
        // still fix by hand is worth more than one quietly mangled.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "aaaa bbbb\n");
        assert_eq!(
            s.apply_edits(&[edit(0, 0, 6, "x"), edit(0, 4, 9, "y")], 0),
            Err(EditError::Overlapping)
        );
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "aaaa bbbb");
        assert!(!s.document.dirty);
    }

    #[test]
    fn the_cursor_stays_where_it_was_rather_than_following_the_edits() {
        // A formatting run that moved the caret to the end of the file would be
        // correct by the letter of the edits and useless in practice.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "one\ntwo\nthree\n");
        s.view.selections = deco_core::SelectionSet::caret(Position::new(1, 2));
        s.apply_edits(&[edit(0, 0, 3, "ONE")], 0).unwrap();
        assert_eq!(s.view.selections.primary().active, Position::new(1, 2));
    }

    #[test]
    fn a_cursor_past_the_end_of_the_reformatted_text_is_clamped() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a long line here\n");
        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 15));
        s.apply_edits(&[edit(0, 0, 16, "short")], 0).unwrap();
        let cursor = s.view.selections.primary().active;
        assert!(cursor.character <= 5, "cursor at {cursor:?}");
    }

    #[test]
    fn an_edit_range_past_the_end_of_the_document_is_clamped() {
        // The server may have answered about text the user has since deleted.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "ab\n");
        s.apply_edits(&[edit(0, 1, 99, "Z")], 0).unwrap();
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "aZ");
    }

    #[test]
    fn formatting_options_come_from_the_users_own_settings() {
        // A server told nothing indents to its defaults, and against a project
        // that disagrees the result is a diff touching every line.
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                Scope::User,
                r#"{"editor.tabSize": 2, "editor.insertSpaces": false,
                    "files.insertFinalNewline": true}"#,
            )
            .unwrap();
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.rs"), "x\n");

        let options = s.formatting_options();
        assert_eq!(options.tab_size, 2);
        assert!(!options.insert_spaces);
        assert!(options.insert_final_newline);
    }

    #[test]
    fn format_commands_are_routed_to_the_frontend() {
        // The core has no server to ask, and naming them keeps a mistyped
        // binding reporting as unknown.
        let mut s = session();
        assert_eq!(
            s.run("editor.action.formatDocument", None, 0),
            Outcome::Frontend("editor.action.formatDocument".into())
        );
        assert_eq!(
            s.run("editor.action.formatSelection", None, 0),
            Outcome::Frontend("editor.action.formatSelection".into())
        );
        assert_eq!(s.run("editor.action.nonsense", None, 0), Outcome::NotFound);
    }

    // ---- The find bar ---------------------------------------------------

    /// A session holding `text`, with the caret at the start.
    fn searchable(text: &str) -> Session {
        let mut s = session();
        s.open(PathBuf::from("/w/a.txt"), text);
        s.resize(80, 10);
        s
    }

    /// The primary selection as `(line, start)..(line, end)`.
    fn selected(s: &Session) -> ((u32, u32), (u32, u32)) {
        let primary = s.view.selections.primary();
        (
            (primary.start().line, primary.start().character),
            (primary.end().line, primary.end().character),
        )
    }

    #[test]
    fn ctrl_f_opens_the_find_bar() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+f");
        assert!(s.find.visible());
    }

    #[test]
    fn typing_with_the_bar_open_goes_into_the_query_not_the_document() {
        let mut s = searchable("foo bar\n");
        press(&mut s, "ctrl+f");
        for key in ["b", "a", "r"] {
            press(&mut s, key);
        }
        assert_eq!(s.find.query(), "bar");
        assert_eq!(
            s.document.buffer.text(),
            "foo bar\n",
            "the document must be untouched"
        );
    }

    #[test]
    fn typing_selects_the_first_match_from_the_search_origin() {
        let mut s = searchable("xx\nfoo\nfoo\n");
        press(&mut s, "ctrl+f");
        for key in ["f", "o", "o"] {
            press(&mut s, key);
        }
        // The first match, not the third: narrowing the query must not walk the
        // cursor down the file one keystroke at a time.
        assert_eq!(selected(&s), ((1, 0), (1, 3)));
    }

    #[test]
    fn backspace_edits_the_query_and_leaves_the_document_alone() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+f");
        press(&mut s, "f");
        press(&mut s, "x");
        press(&mut s, "backspace");
        assert_eq!(s.find.query(), "f");
        assert_eq!(s.document.buffer.text(), "foo\n");
    }

    #[test]
    fn ctrl_v_pastes_into_the_query_not_the_document() {
        let mut s = searchable("foo\n");
        s.clipboard.write("foo");
        press(&mut s, "ctrl+f");
        press(&mut s, "ctrl+v");
        assert_eq!(s.find.query(), "foo");
        assert_eq!(s.document.buffer.text(), "foo\n");
    }

    #[test]
    fn undo_cannot_rewrite_the_document_from_behind_an_open_find_bar() {
        let mut s = searchable("");
        for key in ["h", "i"] {
            press(&mut s, key);
        }
        assert_eq!(s.document.buffer.text(), "hi");
        press(&mut s, "ctrl+f");
        press(&mut s, "ctrl+z");
        assert_eq!(
            s.document.buffer.text(),
            "hi",
            "the user was looking at the find bar"
        );
    }

    #[test]
    fn tab_does_not_indent_the_document_while_the_bar_is_open() {
        // `tab` is gated on `editorTextFocus`, which the find bar turns off.
        let mut s = searchable("x\n");
        press(&mut s, "ctrl+f");
        press(&mut s, "tab");
        assert_eq!(s.document.buffer.text(), "x\n");
    }

    #[test]
    fn ctrl_f_seeds_the_query_from_the_selection() {
        let mut s = searchable("hello world\n");
        s.view.selections = deco_core::selection::SelectionSet::single(
            deco_core::selection::Selection::new(Position::new(0, 6), Position::new(0, 11)),
        );
        press(&mut s, "ctrl+f");
        assert_eq!(s.find.query(), "world");
        // Seeding from a selection leaves that same occurrence current rather
        // than skipping to the next one.
        assert_eq!(selected(&s), ((0, 6), (0, 11)));
    }

    #[test]
    fn pressing_ctrl_f_again_does_not_wipe_the_query() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+f");
        press(&mut s, "f");
        press(&mut s, "ctrl+f");
        assert_eq!(s.find.query(), "f");
    }

    #[test]
    fn enter_goes_to_the_next_match_rather_than_inserting_a_newline() {
        let mut s = searchable("foo\nfoo\n");
        press(&mut s, "ctrl+f");
        for key in ["f", "o", "o"] {
            press(&mut s, key);
        }
        assert_eq!(selected(&s), ((0, 0), (0, 3)));
        press(&mut s, "enter");
        assert_eq!(selected(&s), ((1, 0), (1, 3)));
        assert_eq!(s.document.buffer.text(), "foo\nfoo\n");
    }

    #[test]
    fn shift_enter_goes_to_the_previous_match() {
        let mut s = searchable("foo\nfoo\n");
        press(&mut s, "ctrl+f");
        for key in ["f", "o", "o"] {
            press(&mut s, key);
        }
        press(&mut s, "shift+enter");
        assert_eq!(selected(&s), ((1, 0), (1, 3)), "wrapped to the last match");
    }

    #[test]
    fn f3_walks_forward_and_wraps() {
        let mut s = searchable("foo\nfoo\n");
        press(&mut s, "ctrl+f");
        for key in ["f", "o", "o"] {
            press(&mut s, key);
        }
        press(&mut s, "f3");
        assert_eq!(selected(&s), ((1, 0), (1, 3)));
        press(&mut s, "f3");
        assert_eq!(selected(&s), ((0, 0), (0, 3)), "wrapped to the first match");
    }

    #[test]
    fn f3_with_no_query_searches_for_the_word_under_the_cursor() {
        let mut s = searchable("foo bar\nfoo\n");
        press(&mut s, "f3");
        assert_eq!(s.find.query(), "foo");
        assert_eq!(selected(&s), ((1, 0), (1, 3)));
        assert!(!s.find.visible(), "F3 does not open the bar");
    }

    #[test]
    fn f3_with_nothing_to_search_for_says_so() {
        let mut s = searchable("   \n");
        s.view.selections = deco_core::selection::SelectionSet::caret(Position::new(0, 1));
        assert_eq!(
            press(&mut s, "f3"),
            Outcome::Message("nothing to search for".to_owned())
        );
    }

    #[test]
    fn f3_reports_where_it_landed_while_the_bar_is_closed() {
        let mut s = searchable("foo\nfoo\n");
        // The bar is closed, so the status bar is the only place a count can go.
        let outcome = press(&mut s, "f3");
        assert_eq!(outcome, Outcome::Message("2 of 2 for `foo`".to_owned()));
    }

    #[test]
    fn a_query_matching_nothing_says_so_and_leaves_the_cursor_alone() {
        let mut s = searchable("foo\n");
        s.find.set_query("zzz".to_owned());
        let before = selected(&s);
        assert_eq!(
            press(&mut s, "f3"),
            Outcome::Message("no results for `zzz`".to_owned())
        );
        assert_eq!(selected(&s), before);
    }

    #[test]
    fn escape_closes_the_bar_and_keeps_the_query_for_f3() {
        let mut s = searchable("foo\nfoo\n");
        press(&mut s, "ctrl+f");
        for key in ["f", "o", "o"] {
            press(&mut s, key);
        }
        press(&mut s, "escape");
        assert!(!s.find.visible());
        assert_eq!(s.find.query(), "foo");
        assert!(s.find.matches().is_empty(), "no stale highlight");
        // And F3 still knows what to look for.
        press(&mut s, "f3");
        assert_eq!(selected(&s), ((1, 0), (1, 3)));
    }

    #[test]
    fn typing_reaches_the_document_again_once_the_bar_is_closed() {
        let mut s = searchable("");
        press(&mut s, "ctrl+f");
        press(&mut s, "x");
        press(&mut s, "escape");
        press(&mut s, "y");
        assert_eq!(s.document.buffer.text(), "y");
        assert_eq!(s.find.query(), "x");
    }

    #[test]
    fn alt_c_toggles_case_sensitivity_and_re_searches() {
        let mut s = searchable("FOO\nfoo\n");
        press(&mut s, "ctrl+f");
        for key in ["f", "o", "o"] {
            press(&mut s, key);
        }
        assert_eq!(s.find.matches().len(), 2, "case-insensitive to begin with");
        press(&mut s, "alt+c");
        assert!(s.find.options().case_sensitive);
        assert_eq!(s.find.matches().len(), 1);
        assert_eq!(selected(&s), ((1, 0), (1, 3)));
    }

    #[test]
    fn alt_w_toggles_whole_word_and_re_searches() {
        let mut s = searchable("foobar\nfoo\n");
        press(&mut s, "ctrl+f");
        for key in ["f", "o", "o"] {
            press(&mut s, key);
        }
        assert_eq!(s.find.matches().len(), 2);
        press(&mut s, "alt+w");
        assert!(s.find.options().whole_word);
        assert_eq!(s.find.matches().len(), 1);
    }

    #[test]
    fn the_features_that_are_missing_say_so_rather_than_reporting_unknown() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+f");
        assert_eq!(
            press(&mut s, "alt+r"),
            Outcome::Message("regular-expression search is not implemented yet".to_owned())
        );
        press(&mut s, "escape");
        assert_eq!(
            press(&mut s, "ctrl+h"),
            Outcome::Message("replace is not implemented yet".to_owned())
        );
    }

    #[test]
    fn the_context_keys_follow_the_find_bar() {
        let mut s = searchable("foo\n");
        assert_eq!(s.context.get("findWidgetVisible"), Some(&json!(false)));
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(true)));

        press(&mut s, "ctrl+f");
        // VS Code's spelling, both of them, so a `when` clause copied out of a
        // keybindings.json means the same thing here.
        assert_eq!(s.context.get("findWidgetVisible"), Some(&json!(true)));
        assert_eq!(s.context.get("findInputFocussed"), Some(&json!(true)));
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(false)));
        assert_eq!(
            s.context.get("textInputFocus"),
            Some(&json!(true)),
            "the find box is still a text input"
        );

        press(&mut s, "escape");
        assert_eq!(s.context.get("findWidgetVisible"), Some(&json!(false)));
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(true)));
    }

    #[test]
    fn opening_another_file_drops_the_matches_but_keeps_the_query() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+f");
        for key in ["f", "o", "o"] {
            press(&mut s, key);
        }
        assert!(!s.find.matches().is_empty());
        s.open(PathBuf::from("/w/b.txt"), "nothing here\n");
        assert!(s.find.matches().is_empty());
        assert_eq!(s.find.query(), "foo");
    }
}
