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

/// The length of `text` in UTF-16 code units, which is how positions count.
fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
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
        self.context.set("editorTextFocus", true);
        self.context.set("editorFocus", true);
        self.context.set("textInputFocus", true);
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
            Resolution::Match { command, args } => self.run(&command, args.as_ref(), now_ms),
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
                        self.run("type", Some(&json!({ "text": text })), now_ms)
                    }
                    _ => Outcome::NotFound,
                }
            }
        };

        self.refresh_context();
        outcome
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
            // Commands that need something the core has no concept of. Named
            // here rather than left to fall through as `NotFound`, so a typo in
            // a keybinding is still reported as unknown.
            "editor.action.showHover"
            | "editor.action.revealDefinition"
            | "editor.action.goToReferences"
            | "editor.action.triggerSuggest"
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
}
