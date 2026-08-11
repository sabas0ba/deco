//! One editor session: settings, keymap, theme, and the document being edited.

use std::path::{Path, PathBuf};

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
use crate::prompt::{Prompt, PromptKind};

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

/// One open document that is not on screen.
///
/// Everything that must survive a tab switch and come back intact: the text and
/// its history, the cursor and scroll position, and the diagnostics a server has
/// published for it. The find bar deliberately does not — it closes on a switch,
/// exactly as it does when a file replaces the document, because its match list
/// describes text that is no longer on screen.
#[derive(Debug)]
struct Tab {
    document: Document,
    view: View,
    diagnostics: Vec<deco_lsp::Diagnostic>,
    semantic: Vec<deco_lsp::requests::SemanticSpan>,
}

/// What a tab bar needs to draw one tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabLabel {
    /// The file name, or `Untitled`.
    pub title: String,
    /// Whether the buffer differs from disk.
    pub dirty: bool,
    /// Whether this is the tab on screen.
    pub active: bool,
}

/// Everything one editor window needs.
pub struct Session {
    /// Resolved configuration, layered.
    pub settings: Settings,
    /// The active keymap.
    pub keymap: Keymap,
    /// The active colour theme.
    pub theme: ColorTheme,
    /// The active document.
    pub document: Document,
    /// The view onto it.
    pub view: View,
    /// Tabs to the left of the active one, in display order.
    ///
    /// The active tab's state lives directly in [`Session::document`],
    /// [`Session::view`] and [`Session::diagnostics`] — a zipper, not an indexed
    /// list. That shape is what let tabs arrive without touching the hundreds of
    /// places that already read `session.document`: the active tab is where it
    /// always was, and switching moves whole structs rather than re-pointing
    /// every reader through an index.
    left: Vec<Tab>,
    /// Tabs to the right of the active one, in display order.
    right: Vec<Tab>,
    /// Context keys that `when` clauses read.
    pub context: ContextKeys,
    /// Where cut and copy put their text.
    pub clipboard: Box<dyn Clipboard>,
    /// A transient message for the status bar.
    pub status: Option<String>,
    /// The open prompt — go to line, or the command palette — if there is one.
    pub prompt: Option<Prompt>,
    /// Commands the frontend implements, offered in the palette alongside the
    /// core's own.
    ///
    /// Filled in by the frontend at startup. The core cannot know whether a
    /// command it routes onward will be handled: the terminal frontend can format
    /// a document because it has a language-server client, and the GPU frontend
    /// cannot because it has neither. Listing something on the assumption that
    /// somebody downstream will handle it is how a palette comes to offer what
    /// the editor cannot do.
    pub frontend_commands: Vec<crate::commands::PaletteEntry>,
    /// The find bar, open or not.
    ///
    /// Always present so that `F3` still has a query to search for after the bar
    /// is closed, which is how VS Code behaves.
    pub find: Find,
    /// Semantic tokens for the open document, as the language server classified
    /// them.
    ///
    /// Beside the diagnostics and for the same reason: the frontends need one
    /// place to read from, whatever produced it. Empty when no server is running,
    /// when the server does not offer them, or when the answer has not arrived —
    /// and the lexer's own colouring stands in all three cases.
    pub semantic_tokens: Vec<deco_lsp::requests::SemanticSpan>,
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
            prompt: None,
            frontend_commands: Vec::new(),
            diagnostics: Vec::new(),
            semantic_tokens: Vec::new(),
            left: Vec::new(),
            right: Vec::new(),
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
        // A file already open in some tab is switched to, not opened twice: two
        // tabs onto one file would be two divergent copies of it, and whichever
        // was saved last would silently win.
        if let Some(index) = self.tab_of(&path) {
            self.switch_to(index);
            self.refresh_context();
            return;
        }

        let language = crate::document::language_for_path(&path);
        let settings = EditorSettings::resolve(&self.settings, language);
        let document = Document::from_file(path, text, settings);
        let view = View {
            height: self.view.height,
            width: self.view.width,
            ..Default::default()
        };

        // Into a fresh tab — unless the active tab is a pristine untitled
        // document, which is replaced. That is VS Code's rule, and it is what
        // keeps `deco file.rs` from starting with an empty tab beside the file.
        if !self.is_pristine_untitled() {
            let previous = Tab {
                document: std::mem::replace(
                    &mut self.document,
                    Document::untitled(Default::default()),
                ),
                view: std::mem::take(&mut self.view),
                diagnostics: std::mem::take(&mut self.diagnostics),
                semantic: std::mem::take(&mut self.semantic_tokens),
            };
            self.left.push(previous);
        }
        self.document = document;
        self.view = view;
        // The previous document's diagnostics point at line numbers in a file
        // that is no longer on screen. Carrying them over would decorate the
        // new one with the old one's errors.
        self.diagnostics.clear();
        // And the token list, which describes the other file's text.
        self.semantic_tokens.clear();
        // Same reasoning for the match list. The query survives, since searching
        // the next file for the same thing is a reasonable thing to want.
        self.find.close();
        self.refresh_context();
    }

    /// Whether the active tab is an untitled document nobody has typed into.
    fn is_pristine_untitled(&self) -> bool {
        self.document.path.is_none()
            && !self.document.dirty
            && self.document.buffer.text().is_empty()
    }

    /// The display index of the tab holding `path`, if any tab does.
    fn tab_of(&self, path: &Path) -> Option<usize> {
        let matches = |document: &Document| document.path.as_deref() == Some(path);
        if let Some(index) = self.left.iter().position(|tab| matches(&tab.document)) {
            return Some(index);
        }
        if matches(&self.document) {
            return Some(self.left.len());
        }
        self.right
            .iter()
            .position(|tab| matches(&tab.document))
            .map(|index| self.left.len() + 1 + index)
    }

    /// How many tabs are open. Never zero: the session always shows a document.
    pub fn tab_count(&self) -> usize {
        self.left.len() + 1 + self.right.len()
    }

    /// The display index of the active tab.
    pub fn active_tab(&self) -> usize {
        self.left.len()
    }

    /// One label per tab, in display order, for the tab bar.
    pub fn tab_labels(&self) -> Vec<TabLabel> {
        let label = |document: &Document, active: bool| TabLabel {
            title: document.title(),
            dirty: document.dirty,
            active,
        };
        let mut labels: Vec<TabLabel> = self
            .left
            .iter()
            .map(|tab| label(&tab.document, false))
            .collect();
        labels.push(label(&self.document, true));
        labels.extend(self.right.iter().map(|tab| label(&tab.document, false)));
        labels
    }

    /// Makes the tab at display index `index` active.
    ///
    /// The whole list is collected and re-split around the new index — O(n) in
    /// the number of tabs, which is single digits, in exchange for one obviously
    /// correct implementation instead of four rotation cases.
    fn switch_to(&mut self, index: usize) {
        if index == self.active_tab() {
            return;
        }
        let sizes = (self.view.width, self.view.height);
        let active = Tab {
            document: std::mem::replace(&mut self.document, Document::untitled(Default::default())),
            view: std::mem::take(&mut self.view),
            diagnostics: std::mem::take(&mut self.diagnostics),
            semantic: std::mem::take(&mut self.semantic_tokens),
        };
        let mut all: Vec<Tab> = self.left.drain(..).collect();
        all.push(active);
        all.append(&mut self.right);

        let index = index.min(all.len() - 1);
        let mut chosen = all.remove(index);
        // The terminal did not change size while the tab was in the background,
        // but the background tab's view remembers the size it last had.
        chosen.view.width = sizes.0;
        chosen.view.height = sizes.1;

        self.right = all.split_off(index);
        self.left = all;
        self.document = chosen.document;
        self.view = chosen.view;
        self.diagnostics = chosen.diagnostics;
        self.semantic_tokens = chosen.semantic;
        // The match list describes text that is no longer on screen.
        self.find.close();
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);
    }

    /// `ctrl+tab` / `ctrl+shift+tab`: the next or previous tab, wrapping.
    fn cycle_tab(&mut self, direction: Direction) -> Outcome {
        if self.tab_count() == 1 {
            return Outcome::Handled;
        }
        let count = self.tab_count();
        let target = match direction {
            Direction::Next => (self.active_tab() + 1) % count,
            Direction::Prev => (self.active_tab() + count - 1) % count,
        };
        self.switch_to(target);
        self.refresh_context();
        Outcome::Handled
    }

    /// `ctrl+w`: closes the active tab.
    ///
    /// A dirty document is refused rather than dropped — deco has no dialog to
    /// ask with, and losing edits to a keystroke is the worst thing an editor
    /// can do. Closing the last tab leaves an untitled document, because the
    /// session always shows something.
    fn close_active_tab(&mut self) -> Outcome {
        if self.document.dirty {
            return Outcome::Message(format!(
                "{} has unsaved changes — save it first",
                self.document.title()
            ));
        }

        let sizes = (self.view.width, self.view.height);
        let replacement = if let Some(tab) = if self.right.is_empty() {
            self.left.pop()
        } else {
            Some(self.right.remove(0))
        } {
            tab
        } else {
            Tab {
                document: Document::untitled(EditorSettings::resolve(&self.settings, None)),
                view: View::default(),
                diagnostics: Vec::new(),
                semantic: Vec::new(),
            }
        };

        self.document = replacement.document;
        self.view = replacement.view;
        self.view.width = sizes.0;
        self.view.height = sizes.1;
        self.diagnostics = replacement.diagnostics;
        self.semantic_tokens = replacement.semantic;
        self.find.close();
        self.refresh_context();
        Outcome::Handled
    }

    /// `ctrl+n`: a fresh untitled document in a new tab, focused.
    fn new_untitled_tab(&mut self) -> Outcome {
        let sizes = (self.view.width, self.view.height);
        let previous = Tab {
            document: std::mem::replace(
                &mut self.document,
                Document::untitled(EditorSettings::resolve(&self.settings, None)),
            ),
            view: std::mem::take(&mut self.view),
            diagnostics: std::mem::take(&mut self.diagnostics),
            semantic: std::mem::take(&mut self.semantic_tokens),
        };
        self.left.push(previous);
        self.view.width = sizes.0;
        self.view.height = sizes.1;
        self.find.close();
        self.refresh_context();
        Outcome::Handled
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
        let on_replace = find_focus && self.find.field() == crate::find::Field::Replace;
        // VS Code's key for "a quick-open widget has the keyboard". It takes
        // `editorTextFocus` away for the same reason the find bar does.
        let in_quick_open = self.prompt.is_some();
        self.context.set("inQuickOpen", in_quick_open);
        self.context
            .set("editorTextFocus", !find_focus && !in_quick_open);
        self.context.set("editorFocus", true);
        self.context.set("textInputFocus", true);
        self.context.set("findWidgetVisible", find_focus);
        // Exactly one of the two inputs holds the keyboard, which is what lets
        // `enter` mean "next match" in one and "replace" in the other.
        self.context
            .set("findInputFocussed", find_focus && !on_replace);
        self.context.set("replaceInputFocussed", on_replace);
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
        // The prompt first: it is drawn over the find bar, so it is what holds the
        // keyboard when both are open.
        if let Some(prompt) = &mut self.prompt {
            if prompt.consume(command, args, self.clipboard.as_mut()) {
                return Outcome::Handled;
            }
        }
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
            "actions.find" => self.open_find(false),
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
            "editor.action.startFindReplaceAction" => self.open_find(true),
            // The quick-open prompt. Same reasoning as the find bar: it needs the
            // whole session, not the document and view a command in `commands`
            // sees.
            "workbench.action.gotoLine" => {
                self.prompt = Some(Prompt::plain(PromptKind::GoToLine));
                Outcome::Handled
            }
            "workbench.action.showCommands" => {
                self.prompt = Some(Prompt::list(PromptKind::Commands, self.palette()));
                Outcome::Handled
            }
            // The list of files has to be walked from disk, which only a frontend
            // can do; it calls `offer_files` when it has one.
            "workbench.action.quickOpen" | "workbench.action.findInFiles" => {
                Outcome::Frontend(command.to_owned())
            }
            "workbench.action.closeQuickOpen" => {
                self.prompt = None;
                Outcome::Handled
            }
            "workbench.action.quickOpenSelectNext" => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.next();
                }
                Outcome::Handled
            }
            "workbench.action.quickOpenSelectPrevious" => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.previous();
                }
                Outcome::Handled
            }
            "workbench.action.acceptSelectedQuickOpenItem" => self.accept_prompt(now_ms),
            // Tabs. Session-level because they move whole documents around,
            // which a command in `commands` cannot see past.
            "workbench.action.nextEditor" => self.cycle_tab(Direction::Next),
            "workbench.action.previousEditor" => self.cycle_tab(Direction::Prev),
            "workbench.action.closeActiveEditor" => self.close_active_tab(),
            "workbench.action.files.newUntitledFile" => self.new_untitled_tab(),
            "deco.find.toggleField" => {
                self.find.toggle_field();
                Outcome::Handled
            }
            "editor.action.replaceOne" => self.replace_one(now_ms),
            "editor.action.replaceAll" => self.replace_all(now_ms),
            // Commands that need something the core has no concept of. Named
            // here rather than left to fall through as `NotFound`, so a typo in
            // a keybinding is still reported as unknown.
            "editor.action.showHover"
            | "editor.action.revealDefinition"
            | "editor.action.goToReferences"
            | "workbench.action.gotoSymbol"
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

    /// Opens the quick-open prompt over `files`.
    ///
    /// Called by the frontend once it has walked the workspace. Each entry's `id`
    /// is the path to open and its `title` is what to show, so typing matches the
    /// file name first and the rest of the path second — which is the order people
    /// think in.
    pub fn offer_files(&mut self, files: Vec<crate::commands::PaletteEntry>) {
        if files.is_empty() {
            self.status = Some("no files found here".to_owned());
            return;
        }
        self.prompt = Some(Prompt::list(PromptKind::Files, files));
        self.refresh_context();
    }

    /// Opens the search-results prompt over `results`.
    ///
    /// Separate from [`Session::offer_files`] only in what it says when there is
    /// nothing: an empty file list means the workspace is empty, and an empty
    /// result list means the term is not in it — different facts, and reporting
    /// one as the other would send the reader looking in the wrong place.
    pub fn offer_search_results(
        &mut self,
        needle: &str,
        results: Vec<crate::commands::PaletteEntry>,
    ) {
        if results.is_empty() {
            self.status = Some(format!("`{needle}` is not in any file here"));
            return;
        }
        self.prompt = Some(Prompt::list(PromptKind::SearchResults, results));
        self.refresh_context();
    }

    /// Opens the go-to-symbol prompt over `symbols`.
    ///
    /// Each entry's `id` is the document's own path, so accepting one goes through
    /// the same open-a-file-at-a-position path a search result does — which is
    /// what makes it land in the right tab even if the user switched tabs while
    /// the server was still answering.
    pub fn offer_symbols(&mut self, symbols: Vec<crate::commands::PaletteEntry>) {
        if symbols.is_empty() {
            self.status = Some("this server found no symbols in this file".to_owned());
            return;
        }
        self.prompt = Some(Prompt::list(PromptKind::Symbols, symbols));
        self.refresh_context();
    }

    /// What a project-wide search should look for.
    ///
    /// The selection, the word under the cursor, or whatever the find bar was last
    /// searching for — in that order, because that is the order of how recently the
    /// user said it.
    pub fn search_seed(&self) -> Option<String> {
        if let Some((text, _)) = self.seed_from_document() {
            return Some(text);
        }
        let query = self.find.query();
        (!query.is_empty()).then(|| query.to_owned())
    }

    /// Everything the palette can offer: this crate's commands and the
    /// frontend's.
    fn palette(&self) -> Vec<crate::commands::PaletteEntry> {
        let mut entries: Vec<crate::commands::PaletteEntry> = commands::PALETTE
            .iter()
            .map(|(id, title)| crate::commands::PaletteEntry::new(id, title))
            .collect();
        entries.extend(self.frontend_commands.iter().cloned());
        // The identifier is worth a column of its own here: it is what a
        // `keybindings.json` refers to, and the title does not tell you it.
        for entry in &mut entries {
            entry.detail = Some(entry.id.clone());
        }
        entries
    }

    /// Runs whatever the open prompt was asking for.
    fn accept_prompt(&mut self, now_ms: u64) -> Outcome {
        // Taken rather than borrowed: running a command needs `&mut self`, and a
        // command may open a prompt of its own — `Go to Line` chosen from the
        // palette does exactly that.
        let Some(prompt) = self.prompt.take() else {
            return Outcome::Handled;
        };
        match prompt.kind() {
            PromptKind::GoToLine => self.go_to_line(prompt.text()),
            PromptKind::Commands => match prompt.selected() {
                Some(entry) => {
                    let id = entry.id.clone();
                    self.run(&id, None, now_ms)
                }
                // Nothing matched what was typed. Closing without saying so would
                // look like the command had run.
                None => Outcome::Message(format!("no command matches `{}`", prompt.text())),
            },
            PromptKind::Files | PromptKind::SearchResults | PromptKind::Symbols => {
                match prompt.selected() {
                    // The frontend reads it: the core has no filesystem.
                    Some(entry) => Outcome::OpenFile {
                        path: PathBuf::from(&entry.id),
                        at: entry.at,
                    },
                    None => Outcome::Message(match prompt.kind() {
                        PromptKind::SearchResults => {
                            format!("no result matches `{}`", prompt.text())
                        }
                        PromptKind::Symbols => format!("no symbol matches `{}`", prompt.text()),
                        _ => format!("no file matches `{}`", prompt.text()),
                    }),
                }
            }
        }
    }

    /// Moves the cursor to a line the user typed, one-based as the status bar
    /// shows it.
    ///
    /// Accepts `12` and `12:5` — VS Code's `line:column` — because the status bar
    /// reports both and a reader who has one has usually read the other.
    fn go_to_line(&mut self, text: &str) -> Outcome {
        let text = text.trim();
        if text.is_empty() {
            return Outcome::Handled;
        }
        let (line, column) = match text.split_once(':') {
            Some((line, column)) => (line.trim(), Some(column.trim())),
            None => (text, None),
        };
        let Ok(line) = line.parse::<u32>() else {
            return Outcome::Message(format!("`{text}` is not a line number"));
        };
        let column = match column.map(str::parse::<u32>) {
            Some(Ok(column)) => column,
            Some(Err(_)) => return Outcome::Message(format!("`{text}` is not a line number")),
            None => 1,
        };

        let lines = self.document.buffer.line_count() as u32;
        if line == 0 || line > lines {
            // The count is part of the message: "out of range" without it leaves
            // the user guessing what the range was.
            return Outcome::Message(format!("line {line} is outside 1-{lines}"));
        }

        // Clamped rather than refused: a column past the end of the line is a
        // reasonable thing to ask for, and the end of the line is what was meant.
        let target = self
            .document
            .buffer
            .clamp_position(deco_core::position::Position::new(
                line - 1,
                column.saturating_sub(1),
            ));
        self.view.selections = deco_core::selection::SelectionSet::caret(target);
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);
        Outcome::Handled
    }

    /// Opens the find bar, seeding it from the selection.
    ///
    /// `editor.find.seedSearchStringFromSelection` is on by default in VS Code,
    /// and the reason is that selecting a word and pressing `ctrl+f` is how the
    /// find bar is usually reached.
    fn open_find(&mut self, replacing: bool) -> Outcome {
        let primary = *self.view.selections.primary();
        let seed =
            (!primary.is_empty()).then(|| self.document.buffer.text_in_range(primary.range()));
        // The start of the selection, not the cursor: seeding from a selection
        // must leave that same occurrence as the current match rather than
        // skipping to the next one.
        let origin = primary.start();
        if replacing {
            self.find.open_replace(seed, origin);
        } else {
            self.find.open(seed, origin);
        }
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

    /// `ctrl+h`'s `enter`: replaces the current match and moves to the next.
    ///
    /// A press that is not sitting on a match steps onto one instead of changing
    /// anything, which is VS Code's behaviour and the safe reading of an
    /// ambiguous keypress: replacing text the user cannot see would be worse than
    /// making them press the key twice.
    fn replace_one(&mut self, now_ms: u64) -> Outcome {
        if self.find.query().is_empty() {
            return Outcome::Message("nothing to replace".to_owned());
        }
        self.find.refresh(&self.document.buffer);
        if self.find.matches().is_empty() {
            return Outcome::Message(format!("no results for `{}`", self.find.query()));
        }

        let primary = *self.view.selections.primary();
        let current = deco_core::position::Range::new(primary.start(), primary.end());
        if self.find.ordinal(current).is_none() {
            return self.step_find(Direction::Next);
        }

        let replacement = self.find.replace().to_owned();
        let after = self.replace_range(current, &replacement, now_ms);
        // The document moved under the match list, so it has to be rebuilt before
        // anything is looked up in it.
        self.find.refresh(&self.document.buffer);
        if let Some(range) = self.find.first_at_or_after(after) {
            self.select_match(range);
        }
        Outcome::Handled
    }

    /// `ctrl+alt+enter`: replaces every match, in one undo step.
    ///
    /// One step because that is what the user asked for — one action — and
    /// because undoing a hundred replacements one at a time is not a recovery.
    fn replace_all(&mut self, now_ms: u64) -> Outcome {
        if self.find.query().is_empty() {
            return Outcome::Message("nothing to replace".to_owned());
        }
        self.find.refresh(&self.document.buffer);
        if self.find.matches().is_empty() {
            return Outcome::Message(format!("no results for `{}`", self.find.query()));
        }

        let replacement = self.find.replace().to_owned();
        // A match that already reads as the replacement is left out: replacing
        // `foo` with `foo` should not dirty the file or add an undo step. It is
        // reachable — a case-insensitive search for `foo` finds `FOO` too.
        let edits: Vec<deco_lsp::TextEdit> = self
            .find
            .matches()
            .iter()
            .filter(|range| self.document.buffer.text_in_range(**range) != replacement)
            .map(|range| deco_lsp::TextEdit {
                range: *range,
                new_text: replacement.clone(),
            })
            .collect();
        if edits.is_empty() {
            return Outcome::Message(format!("every match already reads `{replacement}`"));
        }

        // `TextEdit` is a range and a string, and `apply_edits` already turns a
        // batch of them into one transaction with one undo step. A second,
        // identical path would be a second place for that to be wrong.
        let count = match self.apply_edits(&edits, now_ms) {
            Ok(count) => count,
            Err(error) => return Outcome::Message(error.to_string()),
        };
        self.find.refresh(&self.document.buffer);
        Outcome::Message(format!(
            "replaced {count} {}",
            if count == 1 {
                "occurrence"
            } else {
                "occurrences"
            }
        ))
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
        let inverse = self.document.apply(&transaction);

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
        let inverse = self.document.apply(&transaction);

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

    // ---- Replace --------------------------------------------------------

    /// Presses every key in `keys`, in order.
    fn press_all(s: &mut Session, keys: &[&str]) {
        for key in keys {
            press(s, key);
        }
    }

    #[test]
    fn ctrl_h_opens_the_bar_with_the_replacement_focused() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+h");
        assert!(s.find.visible());
        assert!(s.find.replacing());
        assert_eq!(s.find.field(), crate::find::Field::Replace);
        assert_eq!(s.context.get("replaceInputFocussed"), Some(&json!(true)));
        assert_eq!(s.context.get("findInputFocussed"), Some(&json!(false)));
    }

    #[test]
    fn ctrl_h_seeds_the_query_from_the_selection_like_ctrl_f_does() {
        let mut s = searchable("hello world\n");
        s.view.selections = deco_core::selection::SelectionSet::single(
            deco_core::selection::Selection::new(Position::new(0, 6), Position::new(0, 11)),
        );
        press(&mut s, "ctrl+h");
        assert_eq!(s.find.query(), "world");
    }

    #[test]
    fn typing_with_the_replacement_focused_goes_into_the_replacement() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["b", "a", "r"]);
        assert_eq!(s.find.replace(), "bar");
        assert_eq!(s.find.query(), "", "the query is untouched");
        assert_eq!(s.document.buffer.text(), "foo\n");
    }

    #[test]
    fn tab_moves_between_the_two_inputs() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+f");
        press_all(&mut s, &["f", "o", "o"]);
        press(&mut s, "tab");
        assert_eq!(s.find.field(), crate::find::Field::Replace);
        press_all(&mut s, &["b", "a", "r"]);
        press(&mut s, "shift+tab");
        assert_eq!(s.find.field(), crate::find::Field::Query);
        // Each input kept its own text and its own caret.
        assert_eq!(s.find.query(), "foo");
        assert_eq!(s.find.replace(), "bar");
        // And typing now lands back in the query.
        press(&mut s, "x");
        assert_eq!(s.find.query(), "foox");
        assert_eq!(s.find.replace(), "bar");
    }

    #[test]
    fn tab_opens_the_replacement_row_from_a_plain_find_bar() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+f");
        assert!(!s.find.replacing());
        press(&mut s, "tab");
        assert!(s.find.replacing());
    }

    #[test]
    fn enter_replaces_the_current_match_and_moves_on() {
        let mut s = searchable("foo foo\n");
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["b", "a", "r"]);
        press(&mut s, "shift+tab");
        press_all(&mut s, &["f", "o", "o"]);
        press(&mut s, "tab");
        // The first match is current.
        assert_eq!(selected(&s), ((0, 0), (0, 3)));
        press(&mut s, "enter");
        assert_eq!(s.document.buffer.text(), "bar foo\n");
        // And the next match — which has moved — is now current.
        assert_eq!(selected(&s), ((0, 4), (0, 7)));
        press(&mut s, "enter");
        assert_eq!(s.document.buffer.text(), "bar bar\n");
    }

    #[test]
    fn a_replacement_is_one_undo_step() {
        let mut s = searchable("foo foo\n");
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["b", "a", "r"]);
        press(&mut s, "shift+tab");
        press_all(&mut s, &["f", "o", "o"]);
        press(&mut s, "tab");
        press(&mut s, "enter");
        assert_eq!(s.document.buffer.text(), "bar foo\n");
        s.run("undo", None, 0);
        assert_eq!(s.document.buffer.text(), "foo foo\n");
    }

    #[test]
    fn replacing_from_somewhere_that_is_not_a_match_steps_onto_one_first() {
        let mut s = searchable("xx\nfoo\n");
        s.find.set_query("foo".to_owned());
        // The cursor is on line 0, which is not a match.
        s.run("editor.action.replaceOne", None, 0);
        assert_eq!(
            s.document.buffer.text(),
            "xx\nfoo\n",
            "nothing should have been replaced yet"
        );
        assert_eq!(selected(&s), ((1, 0), (1, 3)));
        // The second press, now that the user can see what is about to change.
        s.run("editor.action.replaceOne", None, 0);
        assert_eq!(s.document.buffer.text(), "xx\n\n");
    }

    #[test]
    fn an_empty_replacement_deletes_the_match() {
        let mut s = searchable("foo bar\n");
        s.find.set_query("foo ".to_owned());
        s.run("editor.action.replaceOne", None, 0);
        s.run("editor.action.replaceOne", None, 0);
        assert_eq!(s.document.buffer.text(), "bar\n");
    }

    #[test]
    fn replace_all_changes_every_match_in_one_step() {
        let mut s = searchable("foo\nbar\nfoo\n");
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["b", "a", "z"]);
        press(&mut s, "shift+tab");
        press_all(&mut s, &["f", "o", "o"]);
        let outcome = s.run("editor.action.replaceAll", None, 0);
        assert_eq!(s.document.buffer.text(), "baz\nbar\nbaz\n");
        assert_eq!(
            outcome,
            Outcome::Message("replaced 2 occurrences".to_owned())
        );
        s.run("undo", None, 0);
        assert_eq!(
            s.document.buffer.text(),
            "foo\nbar\nfoo\n",
            "one undo should put all of it back"
        );
    }

    #[test]
    fn replace_all_counts_a_single_occurrence_in_the_singular() {
        let mut s = searchable("foo\n");
        s.find.set_query("foo".to_owned());
        assert_eq!(
            s.run("editor.action.replaceAll", None, 0),
            Outcome::Message("replaced 1 occurrence".to_owned())
        );
    }

    #[test]
    fn replace_all_handles_a_replacement_longer_than_what_it_replaces() {
        // The interesting case for back-to-front application: every edit after
        // the first would be misplaced if they were applied in document order
        // against shifting positions.
        let mut s = searchable("a a a\n");
        s.find.set_query("a".to_owned());
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["l", "o", "n", "g"]);
        s.run("editor.action.replaceAll", None, 0);
        assert_eq!(s.document.buffer.text(), "long long long\n");
    }

    #[test]
    fn replace_all_spanning_lines_lands_correctly() {
        let mut s = searchable("one\ntwo\none\n");
        s.find.set_query("one".to_owned());
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["x"]);
        s.run("editor.action.replaceAll", None, 0);
        assert_eq!(s.document.buffer.text(), "x\ntwo\nx\n");
    }

    #[test]
    fn replacing_with_nothing_to_replace_says_so() {
        let mut s = searchable("foo\n");
        for command in ["editor.action.replaceOne", "editor.action.replaceAll"] {
            assert_eq!(
                s.run(command, None, 0),
                Outcome::Message("nothing to replace".to_owned()),
                "{command}"
            );
        }
    }

    #[test]
    fn replacing_a_query_that_matches_nothing_says_so() {
        let mut s = searchable("foo\n");
        s.find.set_query("zzz".to_owned());
        assert_eq!(
            s.run("editor.action.replaceAll", None, 0),
            Outcome::Message("no results for `zzz`".to_owned())
        );
        assert!(!s.document.dirty);
    }

    #[test]
    fn replacing_text_with_itself_changes_nothing_and_says_why() {
        let mut s = searchable("foo\n");
        s.find.set_query("foo".to_owned());
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["f", "o", "o"]);
        assert_eq!(
            s.run("editor.action.replaceAll", None, 0),
            Outcome::Message("every match already reads `foo`".to_owned())
        );
        assert!(!s.document.dirty, "no undo step for a no-op");
    }

    #[test]
    fn a_case_insensitive_replace_all_rewrites_the_differing_cases_only() {
        let mut s = searchable("foo FOO\n");
        s.find.set_query("foo".to_owned());
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["f", "o", "o"]);
        // `foo` already reads as the replacement; `FOO` does not.
        assert_eq!(
            s.run("editor.action.replaceAll", None, 0),
            Outcome::Message("replaced 1 occurrence".to_owned())
        );
        assert_eq!(s.document.buffer.text(), "foo foo\n");
    }

    #[test]
    fn ctrl_alt_enter_replaces_everything_from_either_input() {
        let mut s = searchable("foo foo\n");
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["b", "a", "r"]);
        press(&mut s, "shift+tab");
        press_all(&mut s, &["f", "o", "o"]);
        // Still on the query, and the key works from here too.
        assert_eq!(s.find.field(), crate::find::Field::Query);
        press(&mut s, "ctrl+alt+enter");
        assert_eq!(s.document.buffer.text(), "bar bar\n");
    }

    #[test]
    fn escape_closes_the_replacement_row_too() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+h");
        press(&mut s, "escape");
        assert!(!s.find.visible());
        assert!(!s.find.replacing());
        assert_eq!(s.context.get("replaceInputFocussed"), Some(&json!(false)));
    }

    #[test]
    fn ctrl_f_after_ctrl_h_puts_the_keyboard_back_on_the_query() {
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+h");
        press_all(&mut s, &["b", "a", "r"]);
        press(&mut s, "ctrl+f");
        assert_eq!(s.find.field(), crate::find::Field::Query);
        assert_eq!(
            s.find.replace(),
            "bar",
            "the replacement is not thrown away"
        );
    }

    // ---- Quick open: go to line, and the command palette ----------------

    #[test]
    fn every_command_the_palette_offers_actually_runs() {
        // The registry is a list of strings beside two `match`es on strings, so
        // they can drift. This is the check that they have not: a palette entry
        // resolving to `NotFound` would be offered to the user and then report
        // itself as unknown when chosen.
        for (id, title) in commands::PALETTE {
            // A fresh session per entry, because several of them change state —
            // and `quit` is in the list, which is exactly the point.
            let mut s = session();
            s.open(PathBuf::from("/w/a.rs"), "fn main() {\n    let x = 1;\n}\n");
            s.resize(80, 10);
            assert_ne!(
                s.run(id, None, 0),
                Outcome::NotFound,
                "the palette offers `{title}` ({id}), which nothing implements"
            );
        }
    }

    // ---- Quick open --------------------------------------------------------

    fn file_entries(paths: &[&str]) -> Vec<commands::PaletteEntry> {
        paths
            .iter()
            .map(|path| commands::PaletteEntry::new(&format!("/w/{path}"), path))
            .collect()
    }

    #[test]
    fn ctrl_p_asks_the_frontend_for_the_file_list() {
        // The core has no filesystem, so it cannot build the list itself.
        let mut s = searchable("x\n");
        assert_eq!(
            press(&mut s, "ctrl+p"),
            Outcome::Frontend("workbench.action.quickOpen".to_owned())
        );
        assert!(s.prompt.is_none(), "the prompt waits for the list");
    }

    #[test]
    fn offering_files_opens_a_filtered_prompt() {
        let mut s = searchable("x\n");
        s.offer_files(file_entries(&["src/main.rs", "src/lib.rs", "README.md"]));
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::Files);
        assert_eq!(prompt.matches(), 3);
        assert_eq!(s.context.get("inQuickOpen"), Some(&json!(true)));
    }

    #[test]
    fn typing_narrows_the_file_list_by_name_then_by_path() {
        let mut s = searchable("x\n");
        s.offer_files(file_entries(&[
            "src/main.rs",
            "docs/main.md",
            "src/other.rs",
        ]));
        for c in "main".chars() {
            press(&mut s, &c.to_string());
        }
        assert_eq!(s.prompt.as_ref().unwrap().matches(), 2);
    }

    #[test]
    fn accepting_a_file_asks_the_frontend_to_open_it() {
        let mut s = searchable("x\n");
        s.offer_files(file_entries(&["src/main.rs", "README.md"]));
        for c in "readme".chars() {
            press(&mut s, &c.to_string());
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::OpenFile {
                path: PathBuf::from("/w/README.md"),
                at: None,
            }
        );
        assert!(s.prompt.is_none());
    }

    #[test]
    fn accepting_nothing_says_so_rather_than_closing_quietly() {
        let mut s = searchable("x\n");
        s.offer_files(file_entries(&["src/main.rs"]));
        for c in "zzzz".chars() {
            press(&mut s, &c.to_string());
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Message("no file matches `zzzz`".to_owned())
        );
    }

    #[test]
    fn an_empty_workspace_says_so_instead_of_opening_an_empty_list() {
        let mut s = searchable("x\n");
        s.offer_files(Vec::new());
        assert!(s.prompt.is_none());
        assert_eq!(s.status.as_deref(), Some("no files found here"));
    }

    #[test]
    fn typing_in_the_file_prompt_never_reaches_the_document() {
        let mut s = searchable("x\n");
        s.offer_files(file_entries(&["a.rs"]));
        for key in ["a", "backspace", "ctrl+z"] {
            press(&mut s, key);
        }
        assert_eq!(s.document.buffer.text(), "x\n");
    }

    // ---- Search in files ---------------------------------------------------

    #[test]
    fn ctrl_shift_f_asks_the_frontend_to_search() {
        let mut s = searchable("x\n");
        assert_eq!(
            press(&mut s, "ctrl+shift+f"),
            Outcome::Frontend("workbench.action.findInFiles".to_owned())
        );
    }

    #[test]
    fn the_search_seed_prefers_the_selection_then_the_word_then_the_find_query() {
        let mut s = searchable("alpha beta\n");
        // The word under the cursor.
        s.view.selections = deco_core::selection::SelectionSet::caret(Position::new(0, 7));
        assert_eq!(s.search_seed().as_deref(), Some("beta"));

        // A selection wins over the word it sits in.
        s.view.selections = deco_core::selection::SelectionSet::single(
            deco_core::selection::Selection::new(Position::new(0, 0), Position::new(0, 5)),
        );
        assert_eq!(s.search_seed().as_deref(), Some("alpha"));

        // Neither: the find bar's last query is the remaining evidence of intent.
        let mut blank = searchable("   \n");
        blank.view.selections = deco_core::selection::SelectionSet::caret(Position::new(0, 1));
        assert_eq!(blank.search_seed(), None);
        blank.find.set_query("gamma".to_owned());
        assert_eq!(blank.search_seed().as_deref(), Some("gamma"));
    }

    #[test]
    fn results_open_a_prompt_that_counts_matches() {
        let mut s = searchable("x\n");
        s.offer_search_results(
            "total",
            vec![
                commands::PaletteEntry::at(
                    "/w/a.rs",
                    "a.rs:2: let total = 1;",
                    Position::new(1, 8),
                ),
                commands::PaletteEntry::at("/w/b.rs", "b.rs:1: // total", Position::new(0, 3)),
            ],
        );
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::SearchResults);
        assert_eq!(prompt.matches(), 2);
    }

    #[test]
    fn accepting_a_result_asks_for_the_file_and_the_position() {
        let mut s = searchable("x\n");
        s.offer_search_results(
            "total",
            vec![commands::PaletteEntry::at(
                "/w/a.rs",
                "a.rs:2: let total = 1;",
                Position::new(1, 8),
            )],
        );
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::OpenFile {
                path: PathBuf::from("/w/a.rs"),
                at: Some(Position::new(1, 8)),
            }
        );
    }

    #[test]
    fn a_term_found_nowhere_says_that_rather_than_that_there_are_no_files() {
        // Two different facts. Reporting one as the other sends the reader looking
        // in the wrong place.
        let mut s = searchable("x\n");
        s.offer_search_results("zzz", Vec::new());
        assert!(s.prompt.is_none());
        assert_eq!(s.status.as_deref(), Some("`zzz` is not in any file here"));

        s.offer_files(Vec::new());
        assert_eq!(s.status.as_deref(), Some("no files found here"));
    }

    #[test]
    fn quick_open_still_opens_a_file_with_no_position() {
        let mut s = searchable("x\n");
        s.offer_files(vec![commands::PaletteEntry::new("/w/a.rs", "a.rs")]);
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::OpenFile {
                path: PathBuf::from("/w/a.rs"),
                at: None,
            }
        );
    }

    // ---- Go to symbol -----------------------------------------------------

    #[test]
    fn symbols_open_a_prompt_that_counts_symbols() {
        let mut s = searchable("x\n");
        s.offer_symbols(vec![
            commands::PaletteEntry::at("/w/a.txt", "Counter", Position::new(0, 11))
                .with_detail("struct"),
            commands::PaletteEntry::at("/w/a.txt", "Counter.bump", Position::new(3, 11))
                .with_detail("method"),
        ]);
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::Symbols);
        assert_eq!(prompt.matches(), 2);
        assert_eq!(prompt.kind().noun(2), "symbols");
        assert_eq!(prompt.kind().noun(1), "symbol");
    }

    #[test]
    fn accepting_a_symbol_asks_for_the_document_and_the_position() {
        // Through the same path a search result takes, which for a document that
        // is already open is a tab switch onto itself — so unsaved changes
        // survive going to a symbol in the file being edited.
        let mut s = searchable("x\n");
        s.offer_symbols(vec![commands::PaletteEntry::at(
            "/w/a.txt",
            "Counter.bump",
            Position::new(3, 11),
        )
        .with_detail("method")]);
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::OpenFile {
                path: PathBuf::from("/w/a.txt"),
                at: Some(Position::new(3, 11)),
            }
        );
    }

    #[test]
    fn a_symbol_can_be_found_by_typing_part_of_its_name() {
        let mut s = searchable("x\n");
        s.offer_symbols(vec![
            commands::PaletteEntry::at("/w/a.txt", "Counter.value", Position::new(1, 4)),
            commands::PaletteEntry::at("/w/a.txt", "Counter.bump", Position::new(3, 11)),
        ]);
        for key in ["b", "u", "m", "p"] {
            press(&mut s, key);
        }
        let prompt = s.prompt.as_ref().expect("still open");
        assert_eq!(prompt.matches(), 1);
        assert_eq!(
            prompt
                .selected()
                .map(|entry| entry.title.clone())
                .as_deref(),
            Some("Counter.bump")
        );
    }

    #[test]
    fn nothing_matching_what_was_typed_says_so_rather_than_closing_silently() {
        let mut s = searchable("x\n");
        s.offer_symbols(vec![commands::PaletteEntry::at(
            "/w/a.txt",
            "Counter",
            Position::new(0, 11),
        )]);
        for key in ["z", "z", "z"] {
            press(&mut s, key);
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Message("no symbol matches `zzz`".to_owned())
        );
    }

    #[test]
    fn a_file_with_no_symbols_says_that_rather_than_opening_an_empty_prompt() {
        let mut s = searchable("x\n");
        s.offer_symbols(Vec::new());
        assert!(s.prompt.is_none());
        assert_eq!(
            s.status.as_deref(),
            Some("this server found no symbols in this file")
        );
    }

    #[test]
    fn go_to_symbol_is_the_frontends_command_since_it_needs_a_server() {
        let mut s = searchable("x\n");
        assert_eq!(
            s.run("workbench.action.gotoSymbol", None, 0),
            Outcome::Frontend("workbench.action.gotoSymbol".to_owned())
        );
    }

    #[test]
    fn the_palette_gives_every_command_its_identifier_as_a_second_column() {
        // It is what a `keybindings.json` refers to, and the title does not say it.
        let s = searchable("x\n");
        let palette = s.palette();
        assert!(!palette.is_empty());
        assert!(palette
            .iter()
            .all(|entry| entry.detail.as_deref() == Some(entry.id.as_str())));
    }

    // ---- Tabs -------------------------------------------------------------

    /// Titles in display order, with `*` marking the active tab.
    fn tabs(s: &Session) -> Vec<String> {
        s.tab_labels()
            .iter()
            .map(|label| {
                if label.active {
                    format!("*{}", label.title)
                } else {
                    label.title.clone()
                }
            })
            .collect()
    }

    #[test]
    fn a_session_starts_with_one_tab() {
        let s = session();
        assert_eq!(s.tab_count(), 1);
        assert_eq!(tabs(&s), vec!["*Untitled"]);
    }

    #[test]
    fn opening_a_file_replaces_a_pristine_untitled_tab() {
        // `deco file.rs` must not start with an empty tab beside the file.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        assert_eq!(tabs(&s), vec!["*a.rs"]);
    }

    #[test]
    fn opening_a_second_file_adds_a_tab_and_focuses_it() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        s.open(PathBuf::from("/w/b.rs"), "b\n");
        assert_eq!(tabs(&s), vec!["a.rs", "*b.rs"]);
        assert_eq!(s.document.buffer.text(), "b\n");
    }

    #[test]
    fn an_untitled_tab_with_typing_in_it_is_not_replaced() {
        let mut s = session();
        press(&mut s, "x");
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        assert_eq!(tabs(&s), vec!["Untitled", "*a.rs"]);
    }

    #[test]
    fn opening_an_already_open_file_switches_to_its_tab() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        s.open(PathBuf::from("/w/b.rs"), "b\n");
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        assert_eq!(s.tab_count(), 2, "no third tab for a file already open");
        assert_eq!(tabs(&s), vec!["*a.rs", "b.rs"]);
    }

    #[test]
    fn ctrl_tab_cycles_forward_and_wraps() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        s.open(PathBuf::from("/w/b.rs"), "b\n");
        s.open(PathBuf::from("/w/c.rs"), "c\n");
        assert_eq!(tabs(&s), vec!["a.rs", "b.rs", "*c.rs"]);
        press(&mut s, "ctrl+tab");
        assert_eq!(
            tabs(&s),
            vec!["*a.rs", "b.rs", "c.rs"],
            "wrapped to the first"
        );
        press(&mut s, "ctrl+tab");
        assert_eq!(tabs(&s), vec!["a.rs", "*b.rs", "c.rs"]);
        press(&mut s, "ctrl+shift+tab");
        assert_eq!(tabs(&s), vec!["*a.rs", "b.rs", "c.rs"]);
        press(&mut s, "ctrl+shift+tab");
        assert_eq!(tabs(&s), vec!["a.rs", "b.rs", "*c.rs"], "wrapped backwards");
    }

    #[test]
    fn each_tab_keeps_its_own_cursor_and_text() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "aaaa\n");
        s.run("type", Some(&json!({ "text": "x" })), 0);
        let cursor_in_a = s.view.selections.primary().active;
        s.open(PathBuf::from("/w/b.rs"), "b\n");
        assert_eq!(s.document.buffer.text(), "b\n");
        press(&mut s, "ctrl+tab");
        assert_eq!(s.document.buffer.text(), "xaaaa\n");
        assert_eq!(s.view.selections.primary().active, cursor_in_a);
    }

    #[test]
    fn each_tab_keeps_its_own_diagnostics() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        s.set_diagnostics(vec![diagnostic(
            0,
            deco_lsp::Severity::Error,
            "problem in a",
        )]);
        s.open(PathBuf::from("/w/b.rs"), "b\n");
        assert!(s.diagnostics.is_empty(), "b has no problems");
        press(&mut s, "ctrl+tab");
        assert_eq!(s.diagnostics.len(), 1, "a's diagnostics came back with it");
        assert_eq!(s.diagnostics[0].message, "problem in a");
    }

    #[test]
    fn undo_history_survives_a_round_trip_through_the_background() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.txt"), "");
        press_all(&mut s, &["h", "i"]);
        s.open(PathBuf::from("/w/b.txt"), "b\n");
        press(&mut s, "ctrl+tab");
        s.run("undo", None, 0);
        assert_eq!(s.document.buffer.text(), "", "a's history still works");
    }

    #[test]
    fn switching_tabs_closes_the_find_bar() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.txt"), "foo\n");
        s.open(PathBuf::from("/w/b.txt"), "foo\n");
        press(&mut s, "ctrl+f");
        press(&mut s, "f");
        assert!(s.find.visible());
        press(&mut s, "ctrl+tab");
        assert!(!s.find.visible(), "its match list described the other tab");
        assert_eq!(s.find.query(), "f", "but the query survives for F3");
    }

    #[test]
    fn closing_a_tab_moves_to_its_neighbour() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        s.open(PathBuf::from("/w/b.rs"), "b\n");
        s.open(PathBuf::from("/w/c.rs"), "c\n");
        press(&mut s, "ctrl+shift+tab");
        assert_eq!(tabs(&s), vec!["a.rs", "*b.rs", "c.rs"]);
        press(&mut s, "ctrl+w");
        assert_eq!(
            tabs(&s),
            vec!["a.rs", "*c.rs"],
            "the right neighbour takes over"
        );
        press(&mut s, "ctrl+w");
        assert_eq!(
            tabs(&s),
            vec!["*a.rs"],
            "no right neighbour, so the left one"
        );
    }

    #[test]
    fn closing_a_dirty_tab_is_refused_with_its_name() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        press(&mut s, "x");
        assert_eq!(
            press(&mut s, "ctrl+w"),
            Outcome::Message("a.rs has unsaved changes — save it first".to_owned())
        );
        assert_eq!(s.tab_count(), 1, "nothing was closed");
        assert_eq!(s.document.buffer.text(), "xa\n", "nothing was lost");
    }

    #[test]
    fn closing_the_last_tab_leaves_an_untitled_document() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        press(&mut s, "ctrl+w");
        assert_eq!(tabs(&s), vec!["*Untitled"]);
        assert_eq!(s.document.buffer.text(), "");
        assert_eq!(s.tab_count(), 1);
    }

    #[test]
    fn ctrl_n_opens_a_new_untitled_tab() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        press(&mut s, "ctrl+n");
        assert_eq!(tabs(&s), vec!["a.rs", "*Untitled"]);
        press_all(&mut s, &["h", "i"]);
        assert_eq!(s.document.buffer.text(), "hi");
        press(&mut s, "ctrl+tab");
        assert_eq!(s.document.buffer.text(), "a\n", "the file is untouched");
    }

    #[test]
    fn cycling_with_one_tab_does_nothing() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        press(&mut s, "ctrl+tab");
        assert_eq!(tabs(&s), vec!["*a.rs"]);
    }

    #[test]
    fn a_background_tab_adopts_the_current_terminal_size_when_it_returns() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        s.open(PathBuf::from("/w/b.rs"), "b\n");
        // The terminal was resized while `a.rs` was in the background.
        s.resize(120, 40);
        press(&mut s, "ctrl+tab");
        assert_eq!((s.view.width, s.view.height), (120, 40));
    }

    #[test]
    fn tab_labels_carry_the_dirty_flag() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        press(&mut s, "x");
        s.run("workbench.action.files.newUntitledFile", None, 0);
        let labels = s.tab_labels();
        assert!(labels[0].dirty, "a.rs was edited");
        assert!(!labels[1].dirty);
        assert!(labels[1].active);
    }

    #[test]
    fn ctrl_g_opens_a_go_to_line_prompt() {
        let mut s = searchable("a\nb\nc\n");
        press(&mut s, "ctrl+g");
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::GoToLine);
        assert!(
            !prompt.has_list(),
            "a line number is not chosen from a list"
        );
        assert_eq!(s.context.get("inQuickOpen"), Some(&json!(true)));
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(false)));
    }

    #[test]
    fn typing_a_line_number_and_pressing_enter_jumps_there() {
        let mut s = searchable("one\ntwo\nthree\nfour\n");
        press(&mut s, "ctrl+g");
        press(&mut s, "3");
        assert_eq!(s.prompt.as_ref().unwrap().text(), "3");
        assert_eq!(s.document.buffer.text(), "one\ntwo\nthree\nfour\n");
        press(&mut s, "enter");
        assert!(s.prompt.is_none(), "accepting closes the prompt");
        assert_eq!(s.view.selections.primary().active, Position::new(2, 0));
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(true)));
    }

    #[test]
    fn a_line_and_column_are_both_honoured() {
        let mut s = searchable("one\ntwo\nthree\n");
        press(&mut s, "ctrl+g");
        for key in ["3", ":", "4"] {
            press(&mut s, key);
        }
        press(&mut s, "enter");
        assert_eq!(s.view.selections.primary().active, Position::new(2, 3));
    }

    #[test]
    fn a_column_past_the_end_of_the_line_lands_at_its_end() {
        let mut s = searchable("one\ntwo\n");
        press(&mut s, "ctrl+g");
        for key in ["1", ":", "9", "9"] {
            press(&mut s, key);
        }
        press(&mut s, "enter");
        assert_eq!(s.view.selections.primary().active, Position::new(0, 3));
    }

    #[test]
    fn a_line_outside_the_document_says_so_and_says_the_range() {
        let mut s = searchable("one\ntwo\n");
        press(&mut s, "ctrl+g");
        for key in ["9", "9"] {
            press(&mut s, key);
        }
        let outcome = press(&mut s, "enter");
        assert_eq!(
            outcome,
            Outcome::Message("line 99 is outside 1-3".to_owned())
        );
        assert_eq!(s.view.selections.primary().active, Position::ZERO);
    }

    #[test]
    fn something_that_is_not_a_number_is_refused() {
        let mut s = searchable("one\n");
        press(&mut s, "ctrl+g");
        for key in ["a", "b"] {
            press(&mut s, key);
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Message("`ab` is not a line number".to_owned())
        );
    }

    #[test]
    fn escape_closes_the_prompt_without_moving_the_cursor() {
        let mut s = searchable("one\ntwo\nthree\n");
        press(&mut s, "ctrl+g");
        press(&mut s, "3");
        press(&mut s, "escape");
        assert!(s.prompt.is_none());
        assert_eq!(s.view.selections.primary().active, Position::ZERO);
    }

    #[test]
    fn typing_into_a_prompt_never_reaches_the_document() {
        let mut s = searchable("x\n");
        press(&mut s, "ctrl+g");
        for key in ["1", "2", "backspace", "ctrl+z", "tab"] {
            press(&mut s, key);
        }
        assert_eq!(s.document.buffer.text(), "x\n");
        assert_eq!(s.prompt.as_ref().unwrap().text(), "1");
    }

    #[test]
    fn ctrl_shift_p_opens_the_palette_with_every_command() {
        let mut s = searchable("x\n");
        press(&mut s, "ctrl+shift+p");
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::Commands);
        assert!(prompt.has_list());
        assert_eq!(prompt.matches(), commands::PALETTE.len());
    }

    #[test]
    fn the_palette_offers_the_frontends_commands_too() {
        let mut s = searchable("x\n");
        s.frontend_commands.push(commands::PaletteEntry::new(
            "editor.action.showHover",
            "Show Hover",
        ));
        press(&mut s, "ctrl+shift+p");
        assert_eq!(
            s.prompt.as_ref().unwrap().matches(),
            commands::PALETTE.len() + 1
        );
    }

    #[test]
    fn choosing_a_command_from_the_palette_runs_it() {
        // A Rust file, because `commentLine` needs the language to have a token.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "fn main() {}\n");
        s.resize(80, 10);
        press(&mut s, "ctrl+shift+p");
        for c in "toggle line".chars() {
            press(&mut s, &c.to_string().replace(' ', "space"));
        }
        assert_eq!(
            s.prompt.as_ref().unwrap().selected().unwrap().id,
            "editor.action.commentLine"
        );
        press(&mut s, "enter");
        assert!(s.prompt.is_none());
        assert_eq!(s.document.buffer.text(), "// fn main() {}\n");
    }

    #[test]
    fn a_frontend_routed_command_chosen_from_the_palette_reaches_the_frontend() {
        let mut s = searchable("x\n");
        s.frontend_commands.push(commands::PaletteEntry::new(
            "editor.action.showHover",
            "Show Hover",
        ));
        press(&mut s, "ctrl+shift+p");
        for c in "hover".chars() {
            press(&mut s, &c.to_string());
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Frontend("editor.action.showHover".to_owned())
        );
    }

    #[test]
    fn a_palette_command_that_opens_a_prompt_of_its_own_works() {
        // `accept_prompt` takes the prompt rather than borrowing it, which is what
        // makes this possible at all.
        let mut s = searchable("one\ntwo\n");
        s.frontend_commands.push(commands::PaletteEntry::new(
            "workbench.action.gotoLine",
            "Go to Line",
        ));
        press(&mut s, "ctrl+shift+p");
        for c in "go to".chars() {
            press(&mut s, &c.to_string().replace(' ', "space"));
        }
        press(&mut s, "enter");
        let prompt = s.prompt.as_ref().expect("go to line should have opened");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::GoToLine);
    }

    #[test]
    fn arrow_keys_move_the_palette_selection_rather_than_the_cursor() {
        let mut s = searchable("one\ntwo\nthree\n");
        press(&mut s, "ctrl+shift+p");
        let first = s.prompt.as_ref().unwrap().selected().unwrap().id.clone();
        press(&mut s, "down");
        let second = s.prompt.as_ref().unwrap().selected().unwrap().id.clone();
        assert_ne!(first, second);
        assert_eq!(
            s.view.selections.primary().active,
            Position::ZERO,
            "the document cursor must not have moved"
        );
        press(&mut s, "up");
        assert_eq!(s.prompt.as_ref().unwrap().selected().unwrap().id, first);
    }

    #[test]
    fn accepting_when_nothing_matches_says_so_rather_than_looking_like_success() {
        let mut s = searchable("x\n");
        press(&mut s, "ctrl+shift+p");
        for c in "zzzz".chars() {
            press(&mut s, &c.to_string());
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Message("no command matches `zzzz`".to_owned())
        );
        assert!(s.prompt.is_none());
    }

    #[test]
    fn the_context_key_follows_the_prompt() {
        let mut s = searchable("x\n");
        assert_eq!(s.context.get("inQuickOpen"), Some(&json!(false)));
        press(&mut s, "ctrl+shift+p");
        assert_eq!(s.context.get("inQuickOpen"), Some(&json!(true)));
        press(&mut s, "escape");
        assert_eq!(s.context.get("inQuickOpen"), Some(&json!(false)));
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
