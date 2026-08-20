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

/// The two answers a permission prompt offers, as the identifiers its choices
/// carry.
///
/// Constants rather than literals in two places: the prompt builds them and the
/// submit reads them, and a typo in either would silently mean "deny".
const CONSENT_ALLOW: &str = "allow";
const CONSENT_DENY: &str = "deny";

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
    /// The find bar as this tab left it.
    ///
    /// Per tab rather than per session, so switching away parks the bar with the
    /// document it describes instead of throwing it away. A match list is stale
    /// only when it belongs to a *different* document; one match list shared by
    /// every tab was what made every switch discard it.
    find: Find,
}

/// One editor group, as something drawing it sees it.
///
/// # Why the renderer is given this rather than the session
///
/// A renderer that reaches into `session.document` can only ever draw the group
/// with the keyboard, because that is the only one the session exposes directly.
/// Naming what one group *is* — its document, its own view onto it, and the tabs
/// it holds — is what lets a second group be drawn beside the first.
///
/// Borrowed rather than owned: this is a description of state the session keeps,
/// built on demand, and it must not be a second copy that can disagree.
pub struct Pane<'a> {
    /// The document showing in this group.
    pub document: &'a Document,
    /// This group's own view onto it. Scroll position and cursor are per group,
    /// which is the point of splitting.
    pub view: &'a View,
    /// The server's classification of that document, if any.
    pub semantic: &'a [deco_lsp::requests::SemanticSpan],
    /// The problems published for it.
    pub diagnostics: &'a [deco_lsp::Diagnostic],
    /// The tabs open in this group, in display order.
    pub tabs: Vec<TabLabel>,
    /// Whether this is the group with the keyboard.
    ///
    /// The renderer needs it for more than decoration: the caret, and the find
    /// bar's match highlighting, belong to the group being typed into.
    pub focused: bool,
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

/// How many paths the recency list keeps.
///
/// Enough that every file of an ordinary session's worth of work is in it. Past that
/// the tail of the list falls back to the alphabetical order the walk produced, which
/// is what quick open did for every file before this existed.
const MAX_RECENT: usize = 64;

/// The picker row that means "work it out from the file name" rather than naming
/// a language.
///
/// deco's own namespace, because VS Code has no identifier for it: the choice is
/// not a language, and giving it one would make `[deco.language.auto]` look like
/// a settings key that does something.
const AUTO_LANGUAGE: &str = "deco.language.auto";

/// A path with its `.` and `..` segments resolved, without touching the disk.
///
/// Enough to tell `/w/src/main.rs` from `/w/./src/../src/main.rs`, which is what
/// decides whether two tabs are one file. Deliberately **not**
/// `fs::canonicalize`: the core has no filesystem, and a path that does not exist
/// yet — a file being created — has to normalise too.
///
/// Symlinks therefore still defeat it. Two names for one file through a link are
/// two tabs, which is the same answer VS Code gives.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Only when there is something to pop: a leading `..` is part of
                // the path's meaning and dropping it would change where it points.
                if matches!(
                    out.components().next_back(),
                    Some(std::path::Component::Normal(_))
                ) {
                    out.pop();
                } else {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Exactly the bytes to write for `document`.
///
/// Its *own* settings, not the session's: `files.insertFinalNewline` can differ
/// per language, and saving every tab must respect each one.
fn contents_of(document: &Document) -> String {
    let mut text = document.buffer.to_disk_string();
    if document.settings.insert_final_newline && !text.ends_with('\n') {
        let eol = document.buffer.line_ending().as_str();
        text.push_str(eol);
    }
    text
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
    /// The other editor group's view onto the same document, when the editor is
    /// split.
    ///
    /// A second *view*, not a second document: `ctrl+\` in VS Code shows one file
    /// in two groups, and one buffer with two views is what that is. Two documents
    /// would be two divergent copies of one file, which is exactly what
    /// [`Session::open`] refuses for tabs.
    ///
    /// [`Session::view`] is always the view of the group with the keyboard, and
    /// this is the other one — the same zipper the tabs use, so every command that
    /// reads `session.view` keeps working without knowing that groups exist.
    /// Whether the revert now in flight should close the tab when it lands.
    ///
    /// The frontend answers a `Revert` with the file's text and nothing else, so
    /// which of the two revert commands asked has to be remembered here.
    pending_close: bool,
    /// How a project-wide search matches.
    ///
    /// Its own, not the find bar's. They were one pair of booleans, so
    /// case-sensitivity set for a search across the workspace changed what the
    /// next `ctrl+f` matched — and VS Code keeps the two apart.
    search_options: deco_core::search::SearchOptions,
    /// Whether the last command was a quit that was refused for unsaved work.
    ///
    /// Cleared by anything else, so "again" means the next keystroke rather than
    /// the next quit whenever that happens to be.
    quit_refused: bool,
    split_view: Option<View>,
    /// Whether the group with the keyboard is the second one.
    ///
    /// Only meaningful while split. Kept so that the panes can be listed in the
    /// order they sit on screen while `view` stays the active one.
    split_focused: bool,
    /// Paths that have been on screen, most recently first.
    ///
    /// What makes `ctrl+p` fast: the file you want is usually one you just had open,
    /// and an alphabetical list buries it. VS Code orders quick open the same way.
    ///
    /// **This session only.** VS Code keeps its history in workspace storage; deco
    /// [writes no files](../../../docs/configuration.md), so the list starts empty
    /// each time rather than being persisted somewhere the user did not ask for.
    recent: Vec<PathBuf>,
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
    /// Whether the frontend can draw a line broken across several rows.
    ///
    /// Declared by the frontend for the same reason [`Session::frontend_commands`]
    /// is: the core cannot know what the thing drawing it is capable of. The GPU
    /// frontend lays out one document line per row, and a session that wrapped
    /// anyway would scroll and move the caret by rows that frontend never draws —
    /// putting the caret in one place and the text it is on in another.
    ///
    /// True by default, which is the terminal frontend and every test.
    pub frontend_wraps: bool,
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

/// Applies `edits` to one document and its view.
///
/// A free function because both callers reach a different pair: the active
/// document and its view, and a background tab's. Everything it does — clamping,
/// refusing overlaps, recording one undo step, marking dirty — belongs to the
/// document rather than to whichever of them is on screen.
fn apply_edits_to(
    document: &mut Document,
    view: &mut View,
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
                    document.buffer.clamp_position(edit.range.start),
                    document.buffer.clamp_position(edit.range.end),
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

    let before = view.selections.clone();
    let inverse = document.apply(&transaction);

    let cursor = document.buffer.clamp_position(before.primary().active);
    let after = deco_core::SelectionSet::caret(cursor);
    view.selections = after.clone();
    document
        .history
        .record(inverse, EditKind::Discrete, before, after, now_ms);
    document.dirty = true;
    // Revealed even for a background tab: when it is switched to, the cursor
    // should be where the edit left it rather than wherever it was parked.
    view.reveal_cursor(&document.buffer, &document.settings);
    Ok(applied)
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
            pending_close: false,
            search_options: deco_core::search::SearchOptions::default(),
            quit_refused: false,
            split_view: None,
            split_focused: false,
            // Seeded from the same platform the keymap was built for, so a
            // `!isMac` binding cannot be chosen and then gated out.
            context: ContextKeys::for_platform(platform),
            clipboard: Box::new(MemoryClipboard::default()),
            status: None,
            find: Find::new(),
            prompt: None,
            frontend_commands: Vec::new(),
            frontend_wraps: true,
            diagnostics: Vec::new(),
            semantic_tokens: Vec::new(),
            left: Vec::new(),
            right: Vec::new(),
            recent: Vec::new(),
            problems,
        };
        session.report_unsupported();
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

        // Shared across tabs even though the bar is not — see `switch_to`.
        let carried_query = self.find.query().to_owned();

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
                find: std::mem::replace(&mut self.find, Find::new()),
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
        // Same reasoning for the match list: this is a *different* document, so
        // the matches really are stale. The query survives, since searching the
        // next file for the same thing is a reasonable thing to want.
        self.find.close();
        if !carried_query.is_empty() {
            self.find.set_query(carried_query);
        }
        // A different file is a different gutter, so a different width for text.
        self.relayout();
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
        // Compared after normalising rather than as spelled. `src/main.rs` from
        // the command line and `/w/src/main.rs` from quick open are one file, and
        // an exact comparison made them two tabs — two buffers, two undo
        // histories, and whichever was saved last winning silently.
        //
        // Callers are expected to hand over absolute paths, and every one does;
        // normalising here as well is what keeps that from being a rule each new
        // caller has to remember.
        let wanted = normalise(path);
        let matches =
            |document: &Document| document.path.as_deref().map(normalise) == Some(wanted.clone());
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
    /// Every editor group, in the order they sit on screen.
    ///
    /// One today. The shape is here so that a renderer is written against groups
    /// from the start rather than against the one the session happens to expose.
    pub fn panes(&self) -> Vec<Pane<'_>> {
        let mut panes = vec![self.pane(&self.view, true)];
        if let Some(other) = &self.split_view {
            let other = self.pane(other, false);
            if self.split_focused {
                // The active view is the second group's, so it goes second on
                // screen and the stored one goes first.
                panes.insert(0, other);
            } else {
                panes.push(other);
            }
        }
        panes
    }

    /// How many editor groups there are.
    pub fn group_count(&self) -> usize {
        1 + usize::from(self.split_view.is_some())
    }

    /// One group, described for whoever is drawing it.
    fn pane<'a>(&'a self, view: &'a View, focused: bool) -> Pane<'a> {
        Pane {
            document: &self.document,
            view,
            semantic: &self.semantic_tokens,
            diagnostics: &self.diagnostics,
            tabs: self.tab_labels(),
            focused,
        }
    }

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
            find: std::mem::replace(&mut self.find, Find::new()),
        };
        // The search string is shared even though the bar is not, as it is in VS
        // Code: opening find in another file shows the same query, and `F3` in a
        // tab you have not searched yet looks for the last thing you looked for.
        let carried_query = active.find.query().to_owned();
        let mut all: Vec<Tab> = self.left.drain(..).collect();
        all.push(active);
        all.append(&mut self.right);

        let index = index.min(all.len() - 1);
        let mut chosen = all.remove(index);
        // The terminal did not change size while the tab was in the background,
        // but the background tab's view remembers the size it last had.
        chosen.view.width = sizes.0;
        chosen.view.height = sizes.1;
        // Its text width is not carried across: the gutter belongs to whichever
        // document is on screen, so `relayout` below asks again once this one is.

        self.right = all.split_off(index);
        self.left = all;
        self.document = chosen.document;
        self.view = chosen.view;
        self.diagnostics = chosen.diagnostics;
        self.semantic_tokens = chosen.semantic;
        // Carried rather than closed. It describes *this* tab's text, which is
        // what is on screen again — switching away and back finds the bar as it
        // was left, matches and all, which is what VS Code does.
        self.find = chosen.find;
        if self.find.query().is_empty() && !carried_query.is_empty() {
            self.find.set_query(carried_query);
        }
        // This document's gutter, then this document's caret.
        self.relayout();
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
    /// Splits the editor, giving the same document a second view.
    ///
    /// The new group starts where the old one is looking and takes the keyboard,
    /// which is what VS Code does — you split in order to work in the new one.
    /// Scrolling it afterwards leaves the first group where it was, which is the
    /// whole point: two places in one file, at once.
    fn split(&mut self) -> Outcome {
        if self.split_view.is_some() {
            // A third group would need a third view and a narrower column each;
            // saying so beats a key that silently does nothing.
            return Outcome::Message("the editor is already split".to_owned());
        }
        self.split_view = Some(self.view.clone());
        self.split_focused = true;
        // Two columns where there was one, so both are narrower and a wrapped line
        // breaks sooner in each.
        self.relayout();
        self.refresh_context();
        Outcome::Message("Split editor — ctrl+1 and ctrl+2 move between them".to_owned())
    }

    /// Moves the keyboard to group `index`, counting from zero on screen.
    fn focus_group(&mut self, index: usize) -> Outcome {
        let count = self.group_count();
        if index >= count {
            return Outcome::Message(match count {
                1 => "there is only one editor group".to_owned(),
                _ => format!("there are only {count} editor groups"),
            });
        }
        let wanted = index == 1;
        if wanted != self.split_focused {
            // The active view is always `self.view`, so moving the keyboard is a
            // swap rather than an index change — the same trick the tabs use.
            if let Some(other) = self.split_view.as_mut() {
                std::mem::swap(other, &mut self.view);
            }
            self.split_focused = wanted;
            // Closed, because the find state belongs to the tab and both groups
            // are showing the same one: its current match is where the *other*
            // group's cursor is. A find bar per group needs a tab list per group,
            // which is the other half of splitting.
            self.find.close();
            // The two columns can differ by a cell, so the view that just took the
            // keyboard needs the width of the column it is now in.
            self.relayout();
        }
        self.refresh_context();
        Outcome::Handled
    }

    /// Refuses to quit while anything is unsaved, and names what.
    ///
    /// The editor already refuses to close *one* unsaved document with `ctrl+w`;
    /// dropping all of them on `ctrl+q` applied that principle to the narrower of
    /// the two paths. A second press quits anyway, because a refusal with no way
    /// past it is a trap rather than a safeguard — and it has to be the very next
    /// keystroke, so that a `ctrl+q` typed minutes later starts the conversation
    /// again rather than acting on an answer nobody remembers giving.
    fn quit(&mut self) -> Outcome {
        if std::mem::take(&mut self.quit_refused) {
            return Outcome::Quit;
        }
        let unsaved: Vec<String> = self
            .documents()
            .filter(|document| document.dirty)
            .map(Document::title)
            .collect();
        if unsaved.is_empty() {
            return Outcome::Quit;
        }
        self.quit_refused = true;
        Outcome::Message(format!(
            "{} {} unsaved changes: {} — ctrl+q again to quit anyway",
            unsaved.len(),
            if unsaved.len() == 1 {
                "tab has"
            } else {
                "tabs have"
            },
            unsaved.join(", ")
        ))
    }

    /// Throws away this document's edits, optionally closing it afterwards.
    ///
    /// An untitled document reverts to empty, which is also what makes one
    /// closable: there is no file to re-read, and empty is what it was.
    ///
    /// The replacement goes through the undo history, so `ctrl+z` brings the edits
    /// back. A command whose whole purpose is to destroy work should not be the one
    /// command that cannot be taken back.
    fn revert(&mut self, and_close: bool) -> Outcome {
        if !self.document.dirty {
            return Outcome::Message(format!("{} has no changes", self.document.title()));
        }
        if self.document.path.is_some() {
            // The frontend has the filesystem; it reads and calls `revert_to`.
            self.pending_close = and_close;
            return Outcome::Revert;
        }
        self.revert_to("");
        if and_close {
            return self.close_active_tab();
        }
        Outcome::Message("Reverted".to_owned())
    }

    /// Replaces the document with `text`, as one undoable step, and marks it clean.
    ///
    /// Called by the frontend once it has re-read the file.
    pub fn revert_to(&mut self, text: &str) -> Outcome {
        let before = self.view.selections.clone();
        let end = self.document.buffer.end_position();
        let transaction = deco_core::Transaction::single(deco_core::Change::replace(
            deco_core::Range::new(deco_core::Position::ZERO, end),
            text.to_owned(),
        ));
        let inverse = self.document.apply(&transaction);
        let after = deco_core::SelectionSet::caret(
            self.document.buffer.clamp_position(before.primary().active),
        );
        self.view.selections = after.clone();
        self.document
            .history
            .record(inverse, deco_core::EditKind::Discrete, before, after, 0);
        self.mark_saved();
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);

        let title = self.document.title();
        if std::mem::take(&mut self.pending_close) {
            return self.close_active_tab();
        }
        Outcome::Message(format!("Reverted {title}"))
    }

    /// `ctrl+w`: closes the group when the editor is split, and the tab otherwise.
    ///
    /// VS Code's rule, and the useful one: having split, the first thing you want
    /// that key to do is put the screen back.
    fn close_editor(&mut self) -> Outcome {
        if self.split_view.is_some() {
            self.split_view = None;
            self.split_focused = false;
            self.relayout();
            self.refresh_context();
            return Outcome::Message("Closed the second group".to_owned());
        }
        self.close_active_tab()
    }

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
                find: Find::new(),
            }
        };

        self.document = replacement.document;
        self.view = replacement.view;
        self.view.width = sizes.0;
        self.view.height = sizes.1;
        self.diagnostics = replacement.diagnostics;
        self.semantic_tokens = replacement.semantic;
        self.find = replacement.find;
        self.relayout();
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
            find: std::mem::replace(&mut self.find, Find::new()),
        };
        self.left.push(previous);
        self.view.width = sizes.0;
        self.view.height = sizes.1;
        // A new tab has no matches to show, and the parked one belongs to the tab
        // it was parked with.
        self.find = Find::new();
        self.relayout();
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
        self.resolve_document_settings();
    }

    /// Resolves the open document's settings again, keeping what the file and the
    /// keyboard have said about it.
    ///
    /// Three things re-resolve: a workspace layer arriving, a rename, and a language
    /// change. Each replaces the whole `EditorSettings`, and two of the values in
    /// there did not come from `settings.json` — the indentation read from the file,
    /// and `alt+z`. Re-applied here rather than at each call site, because a fourth
    /// caller would otherwise silently lose them again.
    fn resolve_document_settings(&mut self) {
        self.document.settings = EditorSettings::resolve(&self.settings, self.document.language());
        self.document.apply_overrides();
        self.report_unsupported();
    }

    /// Adds anything the resolved settings ask for that deco does not do.
    ///
    /// Once each: re-resolving the same settings must not fill the problem list with
    /// copies of one complaint, and the frontend shows every entry.
    fn report_unsupported(&mut self) {
        if let Some(problem) = self.document.settings.unsupported() {
            if !self.problems.contains(&problem) {
                self.problems.push(problem);
            }
        }
    }

    /// Recomputes the context keys `when` clauses read.
    ///
    /// Called after anything that changes focus, selection or the document, so
    /// that a binding gated on `editorHasSelection` becomes active in the same
    /// frame the selection appears.
    pub fn refresh_context(&mut self) {
        self.note_active_document();
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
        // VS Code's own name for its search view being up. The widget is a prompt
        // here rather than a viewlet, but a `when` clause copied out of somebody's
        // keybindings.json should gate on the same thing.
        self.context
            .set("searchViewletVisible", self.searching_project());
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
        // "ctrl+q again" means the very next keystroke. Anything else in between
        // is a user who went back to work, and acting on their earlier answer
        // minutes later would be acting on one nobody remembers giving.
        if !matches!(
            command,
            "workbench.action.quit" | "workbench.action.closeWindow"
        ) {
            self.quit_refused = false;
        }

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
            // Needs the view as well as the document: whether the editor is
            // wrapping depends on how wide the window is, which is the view's.
            "editor.action.toggleWordWrap" => self.toggle_word_wrap(),
            // The find bar, for the same reason: it needs the whole document and
            // its own state, neither of which a command in `commands` can see.
            "actions.find" => self.open_find(false),
            "closeFindWidget" => {
                self.find.close();
                Outcome::Handled
            }
            "editor.action.nextMatchFindAction" => self.step_find(Direction::Next),
            "editor.action.previousMatchFindAction" => self.step_find(Direction::Prev),
            // Whichever search is being typed into. The two are separate: the find
            // bar's options belong to the document on screen, and a project
            // search's belong to the project search.
            "toggleFindCaseSensitive" => {
                if self.searching_project() {
                    self.search_options.case_sensitive = !self.search_options.case_sensitive;
                    // Not an early `return`: the tail of this function is what
                    // puts an `Outcome::Message` on the status bar, and a toggle
                    // that reports nothing is one nobody can tell they pressed.
                    self.report_search_options()
                } else {
                    self.find.toggle_case_sensitive();
                    self.find_query_changed()
                }
            }
            "toggleFindWholeWord" => {
                if self.searching_project() {
                    self.search_options.whole_word = !self.search_options.whole_word;
                    self.report_search_options()
                } else {
                    self.find.toggle_whole_word();
                    self.find_query_changed()
                }
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
            "workbench.action.quickOpen" => Outcome::Frontend(command.to_owned()),
            // Asks what to look for first. It used to search for the seed straight
            // away, which meant a project search could only ever look for what the
            // cursor happened to be on.
            "workbench.action.findInFiles" => {
                self.prompt = Some(Prompt::seeded(
                    PromptKind::SearchQuery,
                    self.search_seed().unwrap_or_default(),
                ));
                self.refresh_context();
                Outcome::Handled
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
            "workbench.action.closeActiveEditor" => self.close_editor(),
            "workbench.action.splitEditor" | "workbench.action.splitEditorRight" => self.split(),
            "workbench.action.focusFirstEditorGroup" => self.focus_group(0),
            "workbench.action.focusSecondEditorGroup" => self.focus_group(1),
            "workbench.action.focusThirdEditorGroup" => self.focus_group(2),
            "workbench.action.files.newUntitledFile" => self.new_untitled_tab(),
            "deco.find.toggleField" => {
                self.find.toggle_field();
                Outcome::Handled
            }
            "workbench.action.editor.changeLanguageMode" => self.offer_languages(),
            // Before `commands::execute` can answer `Outcome::Save`: a document
            // with no path cannot be written, and VS Code's `ctrl+s` opens Save As
            // rather than reporting a dead end. Saying "save it first" and then
            // refusing to save is how an untitled tab became impossible to close.
            "workbench.action.files.save" if self.document.path.is_none() => self.offer_save_as(),
            "workbench.action.files.saveAs" => self.offer_save_as(),
            "workbench.action.quit" | "workbench.action.closeWindow" => self.quit(),
            "workbench.action.files.revert" => self.revert(false),
            "workbench.action.revertAndCloseActiveEditor" => self.revert(true),
            "workbench.action.files.openFile" => self.offer_open_path(),
            "editor.action.replaceOne" => self.replace_one(now_ms),
            "editor.action.replaceAll" => self.replace_all(now_ms),
            // Commands that need something the core has no concept of. Named
            // here rather than left to fall through as `NotFound`, so a typo in
            // a keybinding is still reported as unknown.
            "editor.action.showHover"
            | "editor.action.revealDefinition"
            | "editor.action.goToReferences"
            | "workbench.action.gotoSymbol"
            | "workbench.action.selectTheme"
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
        // A command that resolved from a binding but that nothing handles would
        // otherwise do nothing at all, which is indistinguishable from an editor
        // that has stopped responding. Named rather than silent — and named
        // differently for a feature deco means to build than for an identifier
        // that does not exist here, because those call for different reactions.
        let outcome = match outcome {
            // A command the frontend declared is the frontend's, whatever its
            // name. The identifiers above are written down because they are
            // deco's own and fixed; an extension's are neither — they are
            // whatever is installed — so this is the only way one can be routed
            // at all, and it is what `frontend_commands` is for.
            Outcome::NotFound if self.frontend_owns(command) => {
                Outcome::Frontend(command.to_owned())
            }
            Outcome::NotFound => match commands::pending_title(command) {
                Some(title) => Outcome::Message(format!("{title} is not implemented yet")),
                None => {
                    self.status = Some(format!("there is no command `{command}`"));
                    Outcome::NotFound
                }
            },
            other => other,
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
    pub fn offer_files(&mut self, mut files: Vec<crate::commands::PaletteEntry>) {
        if files.is_empty() {
            self.status = Some("no files found here".to_owned());
            return;
        }
        self.order_by_recency(&mut files);
        self.prompt = Some(Prompt::list(PromptKind::Files, files));
        self.refresh_context();
    }

    /// Puts the files that have been on screen first, most recent first.
    ///
    /// The rest keep the order the frontend supplied, which is alphabetical. Stable,
    /// so a file the session has never seen sits exactly where it did before — the
    /// list only ever gains a preferred prefix.
    fn order_by_recency(&mut self, files: &mut [crate::commands::PaletteEntry]) {
        if self.recent.is_empty() {
            return;
        }
        // Compared lexically rather than as strings, because the walk and `ctrl+o`
        // spell a path differently — `src/main.rs` against `./src/main.rs` — and a
        // recent file the list failed to recognise would silently sink back into the
        // alphabet.
        let recent: Vec<PathBuf> = self.recent.iter().map(|path| normalise(path)).collect();
        // Cached, so a path is normalised once per row and not once per comparison.
        files.sort_by_cached_key(|entry| {
            let path = normalise(Path::new(&entry.id));
            recent
                .iter()
                .position(|seen| *seen == path)
                .unwrap_or(usize::MAX)
        });
    }

    /// Opens the search-results prompt over `results`.
    ///
    /// Separate from [`Session::offer_files`] only in what it says when there is
    /// nothing: an empty file list means the workspace is empty, and an empty
    /// result list means the term is not in it — different facts, and reporting
    /// one as the other would send the reader looking in the wrong place.
    /// Offers the decisions already made, so one can be taken back.
    ///
    /// Empty says so rather than opening a list with nothing in it: "no decisions
    /// to forget" is an answer, and an empty picker is a puzzle.
    pub fn offer_extension_permissions(
        &mut self,
        decisions: Vec<crate::commands::PaletteEntry>,
    ) -> Outcome {
        if decisions.is_empty() {
            return Outcome::Message(
                "no extension permission has been decided in this session".to_owned(),
            );
        }
        self.prompt = Some(Prompt::list(PromptKind::ExtensionPermissions, decisions));
        Outcome::Handled
    }

    /// Asks the user about a capability an extension wants.
    ///
    /// `what` describes the request in the words the user will read — the
    /// extension's name and what it is asking for — because a permission prompt
    /// that does not say who is asking is not a decision anyone can make.
    pub fn ask_extension_consent(&mut self, what: &str) {
        self.prompt = Some(Prompt::list(
            PromptKind::ExtensionConsent,
            vec![
                crate::commands::PaletteEntry::new(CONSENT_ALLOW, &format!("Allow — {what}")),
                crate::commands::PaletteEntry::new(
                    CONSENT_DENY,
                    &format!("Deny — refuse, and remember that for this session ({what})"),
                ),
            ],
        ));
    }

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

    /// Opens the language-mode picker.
    ///
    /// Owned by the core rather than a frontend: every language deco knows is
    /// compiled in, so there is nothing to walk and nothing to read.
    fn offer_languages(&mut self) -> Outcome {
        let mut entries = vec![
            // Detection rather than a choice, which is what VS Code offers first
            // and the only way back once a language has been picked by hand. The
            // detail says what it would decide, so the row is not a mystery.
            crate::commands::PaletteEntry::new(AUTO_LANGUAGE, "Auto Detect").with_detail(
                self.document
                    .path
                    .as_deref()
                    .and_then(crate::document::language_for_path)
                    .unwrap_or("no language"),
            ),
        ];
        entries.extend(
            crate::document::LANGUAGES
                .iter()
                .map(|(id, title)| crate::commands::PaletteEntry::new(id, title).with_detail(id)),
        );
        self.prompt = Some(Prompt::list(PromptKind::Languages, entries));
        self.refresh_context();
        Outcome::Handled
    }

    /// Opens the save-as prompt, seeded with this document's own path.
    ///
    /// Editing the path you are already in beats typing a whole one: "save this
    /// next to itself under another name" is what save-as is usually for.
    fn offer_save_as(&mut self) -> Outcome {
        let seed = self
            .document
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.prompt = Some(Prompt::seeded(PromptKind::SaveAs, seed));
        self.refresh_context();
        Outcome::Handled
    }

    /// Opens the open-file prompt, seeded with this document's directory.
    ///
    /// The directory and not the file: the point is to open something *else*, and
    /// a seed you have to delete the last component of is a seed that cost you.
    fn offer_open_path(&mut self) -> Outcome {
        let seed = self
            .document
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(|parent| {
                let mut text = parent.to_string_lossy().into_owned();
                if !text.is_empty() && !text.ends_with('/') && !text.ends_with('\\') {
                    text.push('/');
                }
                text
            })
            .unwrap_or_default();
        self.prompt = Some(Prompt::seeded(PromptKind::OpenPath, seed));
        self.refresh_context();
        Outcome::Handled
    }

    /// Adopts `path` as this document's, after the frontend has written it there.
    ///
    /// Everything the path decides is redone: the language, and so the lexer and
    /// the `[language]` settings, since `notes.txt` saved as `notes.toml` is a TOML
    /// file now. The document is clean, because what is on disk is what is in the
    /// buffer.
    ///
    /// A language the user chose by hand is **kept**. Having said "this is TOML"
    /// and then saved it, being told it is now plain text would undo a decision
    /// nobody revisited.
    pub fn rename_to(&mut self, path: PathBuf) -> Outcome {
        let chosen_by_hand = self.document.language()
            != self
                .document
                .path
                .as_deref()
                .and_then(crate::document::language_for_path);
        self.document.path = Some(path.clone());
        if !chosen_by_hand {
            self.set_language(None);
        }
        self.resolve_document_settings();
        self.mark_saved();
        Outcome::Message(format!("Saved {}", path.display()))
    }

    /// Opens the colour-theme picker over `themes`.
    ///
    /// Called by the frontend once it has found what is installed: the built-in
    /// themes are compiled in, but a marketplace theme is a file in an extension
    /// directory and the core has no filesystem.
    pub fn offer_themes(&mut self, themes: Vec<crate::commands::PaletteEntry>) {
        if themes.is_empty() {
            self.status = Some("no themes found".to_owned());
            return;
        }
        self.prompt = Some(Prompt::list(PromptKind::Themes, themes));
        self.refresh_context();
    }

    /// Uses `theme` from now on.
    ///
    /// Everything drawn is a function of the theme at render time, so there is
    /// nothing to invalidate — the next frame is already in the new colours.
    ///
    /// The choice lasts the session. Making it stick means `workbench.colorTheme`
    /// in your settings, which deco reads and never writes: an editor that edits
    /// your configuration behind you is worse than one that tells you what to put
    /// in it.
    pub fn set_theme(&mut self, theme: ColorTheme) -> Outcome {
        let report = format!(
            "Theme: {} — set `workbench.colorTheme` to keep it",
            theme.name
        );
        self.theme = theme;
        Outcome::Message(report)
    }

    /// Makes this document `language`, or `None` to go back to detecting it.
    ///
    /// Everything downstream of the identifier is rebuilt: the lexer, and the
    /// settings, which can be overridden per language. The context key follows
    /// too, so a `when` clause on `editorLangId` means what it says.
    ///
    /// The text is untouched. Nothing about a document's bytes depends on which
    /// language it is said to be — only on how it is read.
    pub fn set_language(&mut self, language: Option<&str>) -> Outcome {
        let resolved = match language {
            Some(language) => Some(language.to_owned()),
            None => self
                .document
                .path
                .as_deref()
                .and_then(crate::document::language_for_path)
                .map(str::to_owned),
        };
        self.document.language_id = resolved;
        self.document.syntax = deco_syntax::Syntax::new(self.document.language());
        self.resolve_document_settings();
        self.refresh_context();

        Outcome::Message(match self.document.language() {
            Some(language) => {
                format!("Language: {}", crate::document::language_title(language))
            }
            None => "Language: none — nothing matches this file name".to_owned(),
        })
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

    /// `alt+z`: wraps this document's long lines, or stops.
    ///
    /// Per document, because that is where the resolved settings live — so it is
    /// also per tab, and turning it on to read one Markdown file leaves the code in
    /// the next tab alone. It is not written anywhere: deco
    /// [does not write settings files](../../../docs/configuration.md), and a
    /// keystroke that silently edited one would be the wrong way to find that out.
    ///
    /// Turning it back on restores whatever `editor.wordWrap` says — including a
    /// `[language]` override of it — rather than assuming `"on"`. Somebody who
    /// configured `"bounded"` and pressed the key twice asked to get back what they
    /// had, not the viewport width.
    fn toggle_word_wrap(&mut self) -> Outcome {
        let wrapping = self.view.wrap_column(&self.document.settings) > 0;
        let configured =
            EditorSettings::resolve(&self.settings, self.document.language()).word_wrap;
        let wanted = if wrapping {
            deco_config::WordWrap::Off
        } else if configured == deco_config::WordWrap::Off {
            deco_config::WordWrap::On
        } else {
            configured
        };
        // Recorded on the document as well as applied, so that changing the language
        // — which resolves these settings from scratch — does not un-press the key.
        self.document.wrap_override = Some(wanted);
        self.document.settings.word_wrap = wanted;

        // The anchor and the caret both mean something different now: the rows a
        // window holds have changed under it.
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);
        if let Some(mut other) = self.split_view.take() {
            other.reveal_cursor(&self.document.buffer, &self.document.settings);
            self.split_view = Some(other);
        }

        match self.view.wrap_column(&self.document.settings) {
            0 => Outcome::Message("Word wrap off".to_owned()),
            column => Outcome::Message(format!("Word wrap on, at column {column}")),
        }
    }

    /// Whether the search prompt is what has the keyboard.
    fn searching_project(&self) -> bool {
        self.prompt
            .as_ref()
            .is_some_and(|prompt| prompt.kind() == PromptKind::SearchQuery)
    }

    /// Says which options a project search will use, since the prompt has no room
    /// to draw them and a toggle nobody can see is a toggle nobody trusts.
    fn report_search_options(&mut self) -> Outcome {
        let describe = |on: bool| if on { "on" } else { "off" };
        Outcome::Message(format!(
            "Search: case {}, whole word {}",
            describe(self.search_options.case_sensitive),
            describe(self.search_options.whole_word)
        ))
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

    /// Whether the frontend declared this command as one of its own.
    ///
    /// Consulted only after nothing here handled it, so a core command can never
    /// be shadowed by an extension declaring its identifier.
    fn frontend_owns(&self, command: &str) -> bool {
        self.frontend_commands
            .iter()
            .any(|entry| entry.id == command)
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
        //
        // Unless the frontend already said something, which it does for a command
        // that came from an extension: there the useful fact is *which* extension,
        // and an identifier the reader has no reason to have seen before is not a
        // reason to throw that away.
        for entry in &mut entries {
            if entry.detail.is_none() {
                entry.detail = Some(entry.id.clone());
            }
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
            PromptKind::SearchQuery => {
                let typed = prompt.text().trim();
                if typed.is_empty() {
                    return Outcome::Message("nothing to search for".to_owned());
                }
                Outcome::SearchInFiles {
                    query: typed.to_owned(),
                    options: self.search_options,
                }
            }
            PromptKind::SaveAs => {
                let typed = prompt.text().trim();
                if typed.is_empty() {
                    return Outcome::Message("no filename given".to_owned());
                }
                Outcome::SaveAs(PathBuf::from(typed))
            }
            PromptKind::OpenPath => {
                let typed = prompt.text().trim();
                if typed.is_empty() {
                    return Outcome::Message("no filename given".to_owned());
                }
                Outcome::OpenFile {
                    path: PathBuf::from(typed),
                    at: None,
                }
            }
            PromptKind::ExtensionPermissions => match prompt.selected() {
                Some(entry) => Outcome::ForgetExtensionPermission(entry.id.clone()),
                None => Outcome::Message(format!("no decision matches `{}`", prompt.text())),
            },
            PromptKind::ExtensionConsent => match prompt.selected() {
                Some(entry) => Outcome::ExtensionConsent {
                    allow: entry.id == CONSENT_ALLOW,
                },
                // Nothing matched what was typed, which for a two-choice prompt
                // means the filter hid both. Treated as no decision rather than
                // as a refusal: the extension is still waiting, and the prompt
                // can be opened again.
                None => Outcome::Message("no answer chosen".to_owned()),
            },
            PromptKind::Themes => match prompt.selected() {
                // The identifier is the file to read, empty for one compiled in.
                Some(entry) => Outcome::LoadTheme {
                    label: entry.title.clone(),
                    path: (!entry.id.is_empty()).then(|| PathBuf::from(&entry.id)),
                },
                None => Outcome::Message(format!("no theme matches `{}`", prompt.text())),
            },
            PromptKind::Languages => match prompt.selected() {
                Some(entry) if entry.id == AUTO_LANGUAGE => self.set_language(None),
                Some(entry) => {
                    let id = entry.id.clone();
                    self.set_language(Some(&id))
                }
                None => Outcome::Message(format!("no language matches `{}`", prompt.text())),
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
        let applied = apply_edits_to(&mut self.document, &mut self.view, edits, now_ms)?;
        if applied > 0 {
            self.refresh_context();
        }
        Ok(applied)
    }

    /// The same, for whichever open tab holds `path`.
    ///
    /// `None` when no tab does, which is the caller's cue that the file has to be
    /// changed on disk instead. Edits must reach the *buffer* of an open document
    /// rather than its file: a document with unsaved changes would overwrite them
    /// the next time it was saved, so an edit written past it is an edit that
    /// silently did not happen.
    pub fn apply_edits_to_path(
        &mut self,
        path: &Path,
        edits: &[deco_lsp::TextEdit],
        now_ms: u64,
    ) -> Option<Result<usize, EditError>> {
        if self.document.path.as_deref() == Some(path) {
            return Some(self.apply_edits(edits, now_ms));
        }
        let tab = self
            .left
            .iter_mut()
            .chain(self.right.iter_mut())
            .find(|tab| tab.document.path.as_deref() == Some(path))?;
        let applied = apply_edits_to(&mut tab.document, &mut tab.view, edits, now_ms);
        // No `refresh_context`: the context keys describe the document on screen,
        // and this one is not it.
        Some(applied)
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
        contents_of(&self.document)
    }

    /// Writes every unsaved document, using `write` for the bytes.
    ///
    /// The loop and its reporting live here so both frontends behave identically,
    /// and so the behaviour is testable with an in-memory `write`. The core still
    /// performs no I/O of its own: a path and the bytes go out, a result comes
    /// back, and what to do with them is the caller's business.
    ///
    /// Each write is reported individually, so one failure leaves that document
    /// dirty rather than marking the batch saved — a tab that looks saved and is
    /// not is how work gets lost. A failure does not stop the rest: the other
    /// documents still deserve to be written.
    ///
    /// A dirty *untitled* document is counted and skipped. There is no filename to
    /// write to, and inventing one would put the user's work somewhere they did not
    /// ask for.
    pub fn save_all(
        &mut self,
        mut write: impl FnMut(&Path, &str) -> Result<(), String>,
    ) -> Outcome {
        let pending = self.unsaved();
        let untitled = self.unsaved_untitled();
        if pending.is_empty() && untitled == 0 {
            return Outcome::Message("Nothing to save".to_owned());
        }

        let mut written = 0usize;
        let mut failures = Vec::new();
        for (path, contents) in pending {
            match write(&path, &contents) {
                Ok(()) => {
                    self.mark_saved_at(&path);
                    written += 1;
                }
                Err(error) => failures.push(error),
            }
        }

        let mut report = format!(
            "Saved {written} {}",
            if written == 1 { "file" } else { "files" }
        );
        if untitled > 0 {
            report.push_str(&format!(
                "; {untitled} {} no filename yet",
                if untitled == 1 {
                    "document has"
                } else {
                    "documents have"
                }
            ));
        }
        if !failures.is_empty() {
            report.push_str(&format!("; {} could not be written", failures.len()));
            // The reason belongs where a reader can go and find it: a status bar
            // has one line and several failures would each shorten the last.
            self.problems.extend(failures);
        }
        Outcome::Message(report)
    }

    /// Every document with unsaved changes and a filename, in tab order.
    ///
    /// For `workbench.action.files.saveAll`. Each pair is the path to write and
    /// exactly the bytes to write there, resolved through that document's own
    /// settings — a tab holding a `.md` file gets its own
    /// `files.insertFinalNewline` rather than the active document's.
    ///
    /// A dirty *untitled* document is left out: there is no filename to write to,
    /// and inventing one would put the user's work somewhere they did not ask for.
    /// [`Session::unsaved_untitled`] counts those so the frontend can say so.
    pub fn unsaved(&self) -> Vec<(PathBuf, String)> {
        self.documents()
            .filter(|document| document.dirty)
            .filter_map(|document| {
                let path = document.path.clone()?;
                Some((path, contents_of(document)))
            })
            .collect()
    }

    /// How many unsaved documents have no filename to be written to.
    pub fn unsaved_untitled(&self) -> usize {
        self.documents()
            .filter(|document| document.dirty && document.path.is_none())
            .count()
    }

    /// Every document, in tab order, the active one in its place among them.
    fn documents(&self) -> impl Iterator<Item = &Document> {
        self.left
            .iter()
            .map(|tab| &tab.document)
            .chain(std::iter::once(&self.document))
            .chain(self.right.iter().rev().map(|tab| &tab.document))
    }

    /// Marks the document at `path` as saved, wherever it is.
    ///
    /// Per-path rather than "all of them" so that a write which failed leaves that
    /// document dirty: a tab that looks saved and is not is how work gets lost.
    pub fn mark_saved_at(&mut self, path: &Path) {
        let holds = |document: &Document| document.path.as_deref() == Some(path);
        if holds(&self.document) {
            self.mark_saved();
            return;
        }
        for tab in self.left.iter_mut().chain(self.right.iter_mut()) {
            if holds(&tab.document) {
                tab.document.dirty = false;
                tab.document.history.break_group();
                return;
            }
        }
    }

    /// Marks the document as saved.
    pub fn mark_saved(&mut self) {
        self.document.dirty = false;
        self.document.history.break_group();
        self.refresh_context();
    }

    /// Tells the session how large the text area is.
    ///
    /// `width` is the whole area, gutters and separators included; the session
    /// works out from [`crate::layout`] how many columns each group leaves for
    /// text, because that is what decides where a wrapped line breaks. Doing the
    /// arithmetic here rather than in the frontend is what keeps the wrap and the
    /// drawing from disagreeing about the width.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.lay_out(width, height);
        // A window that is a different shape may no longer hold the caret.
        self.view
            .reveal_cursor(&self.document.buffer, &self.document.settings);
        if let Some(mut other) = self.split_view.take() {
            other.reveal_cursor(&self.document.buffer, &self.document.settings);
            self.split_view = Some(other);
        }
    }

    /// Gives every group its size and the columns it leaves for text, without
    /// moving any window.
    fn lay_out(&mut self, width: usize, height: usize) {
        let columns = crate::layout::column_widths(width, self.group_count());
        let gutter = crate::layout::gutter_width(&self.document);
        // The active group is the second one on screen while the split has the
        // keyboard — the same order `panes` reports.
        let active = usize::from(self.split_focused);
        // Zero for a frontend that does not wrap, which is what tells the view
        // there is nowhere to break.
        let text_width = |index: usize| {
            if !self.frontend_wraps {
                return 0;
            }
            columns
                .get(index)
                .copied()
                .unwrap_or(width)
                .saturating_sub(gutter)
        };

        self.view.width = width;
        self.view.height = height;
        self.view.text_width = text_width(active);
        let other_width = text_width(1 - active);
        if let Some(other) = self.split_view.as_mut() {
            other.width = width;
            other.height = height;
            // Both groups show the same document today, so one gutter serves both;
            // when they can differ this reads each pane's own.
            other.text_width = other_width;
        }
    }

    /// Moves the open document to the front of the recency list.
    ///
    /// Called from [`Session::refresh_context`], which runs after everything that
    /// changes what is on screen — so there is no set of call sites to keep in step,
    /// which is what a list like this usually goes wrong by. The guard makes the
    /// common case one comparison: on an ordinary keystroke the document is already
    /// at the front.
    fn note_active_document(&mut self) {
        let Some(path) = self.document.path.as_deref() else {
            // An untitled document has nothing to remember it by, and it is already
            // on screen.
            return;
        };
        if self.recent.first().is_some_and(|first| first == path) {
            return;
        }
        let path = path.to_owned();
        self.recent.retain(|seen| seen != &path);
        self.recent.insert(0, path);
        self.recent.truncate(MAX_RECENT);
    }

    /// Recomputes each group's text width for the size the session was last given.
    ///
    /// How many columns are left for text depends on the document's gutter and on
    /// how many groups share the screen, so anything that changes either — a tab
    /// switch, a split, a group closing — has to ask again. A stale width wraps the
    /// file on screen at the width of the one that used to be.
    ///
    /// Deliberately not a `resize`: nothing here scrolls. Focusing a group that was
    /// scrolled away from its own caret must leave it where it was, and re-laying
    /// out the same size is not a reason to move any window.
    fn relayout(&mut self) {
        let (width, height) = (self.view.width, self.view.height);
        self.lay_out(width, height);
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
        // A document with a name; a nameless one goes to the save-as prompt
        // instead, which has its own test.
        let mut s = searchable("x\n");
        assert_eq!(press(&mut s, "ctrl+s"), Outcome::Save);
        assert_eq!(press(&mut s, "ctrl+q"), Outcome::Quit);
    }

    #[test]
    fn ctrl_s_on_an_untitled_document_asks_where_to_put_it() {
        // It used to report "This document has no filename yet" — after having
        // aimed the write at whatever file deco was started with. Neither is a
        // save, and `ctrl+w` was meanwhile saying "save it first", so an untitled
        // tab could be neither saved nor closed.
        let mut s = session();
        s.resize(80, 10);
        press(&mut s, "y");
        assert_eq!(press(&mut s, "ctrl+s"), Outcome::Handled);
        let prompt = s
            .prompt
            .as_ref()
            .expect("the save-as prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::SaveAs);
        assert_eq!(prompt.text(), "", "nothing to seed it with");
    }

    #[test]
    fn an_untitled_document_can_be_closed_once_it_has_been_saved() {
        // The route out of the trap, end to end.
        let mut s = session();
        s.resize(80, 10);
        press(&mut s, "y");
        press(&mut s, "ctrl+s");
        for key in ["a", ".", "t", "x", "t"] {
            press(&mut s, key);
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::SaveAs(PathBuf::from("a.txt"))
        );
        // The frontend writes and reports back, which is what clears `dirty`.
        s.rename_to(PathBuf::from("/w/a.txt"));
        assert_eq!(press(&mut s, "ctrl+w"), Outcome::Handled);
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
        // The defaults still work. `ctrl+q` rather than `ctrl+s`, because an
        // untitled document routes the save key to the save-as prompt.
        assert_eq!(press(&mut s, "ctrl+q"), Outcome::Quit);
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
    fn the_files_you_have_had_open_come_first() {
        // What makes `ctrl+p` fast: the file you want is usually one you just had
        // open, and an alphabetical list buries it.
        let mut s = session();
        s.open(PathBuf::from("/w/zebra.rs"), "z\n");
        s.open(PathBuf::from("/w/apple.rs"), "a\n");

        s.offer_files(file_entries(&[
            "aardvark.rs",
            "apple.rs",
            "middle.rs",
            "zebra.rs",
        ]));
        assert_eq!(
            titles(&s),
            ["apple.rs", "zebra.rs", "aardvark.rs", "middle.rs"],
            "the two that were open, most recent first, then the alphabet"
        );
    }

    #[test]
    fn a_tab_switch_moves_a_file_back_to_the_front() {
        let mut s = session();
        s.open(PathBuf::from("/w/one.rs"), "1\n");
        s.open(PathBuf::from("/w/two.rs"), "2\n");
        s.run("workbench.action.previousEditor", None, 0);
        assert_eq!(s.document.title(), "one.rs");

        s.offer_files(file_entries(&["one.rs", "two.rs"]));
        assert_eq!(titles(&s), ["one.rs", "two.rs"]);
    }

    #[test]
    fn a_file_that_was_closed_is_still_remembered() {
        // VS Code remembers it too, and it is exactly the file you are most likely to
        // want back.
        let mut s = session();
        s.open(PathBuf::from("/w/gone.rs"), "g\n");
        s.open(PathBuf::from("/w/here.rs"), "h\n");
        s.run("workbench.action.previousEditor", None, 0);
        s.run("workbench.action.closeActiveEditor", None, 0);
        assert_eq!(s.document.title(), "here.rs");

        s.offer_files(file_entries(&["aaa.rs", "gone.rs", "here.rs"]));
        assert_eq!(titles(&s), ["here.rs", "gone.rs", "aaa.rs"]);
    }

    #[test]
    fn a_session_that_has_opened_nothing_lists_alphabetically() {
        // Which is what quick open did for every file before recency existed, and is
        // still the right answer with nothing to prefer.
        let mut s = session();
        s.offer_files(file_entries(&["aaa.rs", "bbb.rs", "ccc.rs"]));
        assert_eq!(titles(&s), ["aaa.rs", "bbb.rs", "ccc.rs"]);
    }

    #[test]
    fn a_path_spelled_differently_is_still_the_same_file() {
        // `ctrl+o` resolves what was typed; the walk joins onto the workspace root.
        // The two disagree about `./`, and a string comparison would sink the file
        // back into the alphabet.
        let mut s = session();
        s.open(PathBuf::from("/w/./src/../src/main.rs"), "m\n");
        s.offer_files(file_entries(&["aaa.rs", "src/main.rs"]));
        assert_eq!(titles(&s), ["src/main.rs", "aaa.rs"]);
    }

    #[test]
    fn recency_orders_equal_matches_and_no_more_than_that() {
        // Two rows that match `main` equally well: recency decides between them.
        let mut s = session();
        s.open(PathBuf::from("/w/main.md"), "d\n");
        s.offer_files(file_entries(&["main.rs", "main.md"]));
        for c in "main".chars() {
            press(&mut s, &c.to_string());
        }
        assert_eq!(titles(&s), ["main.md", "main.rs"], "the recent one first");
    }

    #[test]
    fn a_better_match_still_beats_a_recent_one() {
        // Recency orders equals; it does not outrank how well a row matches. Here
        // `main.rs` matches `main` as a prefix and `domain.rs` only contains it, and
        // `domain.rs` is the file that was open.
        let mut s = session();
        s.open(PathBuf::from("/w/domain.rs"), "d\n");
        s.offer_files(file_entries(&["main.rs", "domain.rs"]));
        for c in "main".chars() {
            press(&mut s, &c.to_string());
        }
        assert_eq!(titles(&s), ["main.rs", "domain.rs"]);
    }

    /// The titles the open prompt is offering, in order.
    fn titles(s: &Session) -> Vec<String> {
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        prompt
            .visible()
            .iter()
            .map(|entry| entry.title.clone())
            .collect()
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

    // ---- Detected indentation ----------------------------------------------

    #[test]
    fn opening_a_two_space_file_indents_by_two_whatever_the_setting_says() {
        // `editor.detectIndentation` is on by default, and this is the whole point
        // of it: the first `tab` in somebody else's project must not reindent it.
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "editor.tabSize", json!(4));
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(
            PathBuf::from("/w/a.ts"),
            "const a = {\n  b: {\n    c: 1,\n  },\n};\n",
        );

        assert_eq!(s.document.settings.tab_size, 2);
        assert!(s.document.indentation_overridden, "and it says so");

        s.view.selections = deco_core::SelectionSet::caret(Position::new(4, 0));
        press(&mut s, "tab");
        assert_eq!(
            s.document.buffer.line_content(4).unwrap().to_string(),
            "  };",
            "two, not four"
        );
    }

    #[test]
    fn a_file_that_agrees_with_the_setting_overrides_nothing() {
        // So the status bar has nothing to disclose, which is most files.
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "editor.tabSize", json!(2));
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.ts"), "a\n  b\n    c\n");
        assert_eq!(s.document.settings.tab_size, 2);
        assert!(!s.document.indentation_overridden);
    }

    #[test]
    fn a_tab_indented_file_switches_to_tabs_and_keeps_the_settings_width() {
        // How wide a tab is drawn is `editor.tabSize`'s business; the file only says
        // that it uses one.
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                Scope::User,
                r#"{"editor.tabSize": 8, "editor.insertSpaces": true}"#,
            )
            .unwrap();
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.go"), "func a() {\n\tb()\n}\n");

        assert!(!s.document.settings.insert_spaces);
        assert_eq!(s.document.settings.tab_size, 8);

        s.view.selections = deco_core::SelectionSet::caret(Position::new(2, 0));
        press(&mut s, "tab");
        assert_eq!(
            s.document.buffer.line_content(2).unwrap().to_string(),
            "\t}",
            "a tab, not eight spaces"
        );
    }

    #[test]
    fn detect_indentation_off_leaves_the_setting_alone() {
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                Scope::User,
                r#"{"editor.tabSize": 4, "editor.detectIndentation": false}"#,
            )
            .unwrap();
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.ts"), "a\n  b\n    c\n");
        assert_eq!(s.document.settings.tab_size, 4);
        assert!(!s.document.indentation_overridden);
    }

    #[test]
    fn a_language_change_does_not_lose_what_the_file_said() {
        // `ctrl+k m` resolves the settings from scratch. The file's indentation did
        // not change because the language did.
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "editor.tabSize", json!(4));
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.txt"), "a\n  b\n    c\n");
        assert_eq!(s.document.settings.tab_size, 2);

        s.set_language(Some("markdown"));
        assert_eq!(s.document.settings.tab_size, 2, "still the file's answer");
    }

    #[test]
    fn a_language_change_does_not_un_press_alt_z() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.txt"), "x\n");
        s.resize(40, 8);
        press(&mut s, "alt+z");
        assert!(s.view.wrap_column(&s.document.settings) > 0);

        s.set_language(Some("markdown"));
        assert!(
            s.view.wrap_column(&s.document.settings) > 0,
            "the keyboard said so, and re-resolving must not un-say it"
        );
    }

    #[test]
    fn workspace_settings_can_turn_the_detection_off_after_the_fact() {
        // The flag is read from the freshly resolved settings, so a workspace layer
        // arriving takes effect without the file being reopened.
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "editor.tabSize", json!(4));
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.ts"), "a\n  b\n    c\n");
        assert_eq!(s.document.settings.tab_size, 2);

        s.set_workspace_settings(r#"{"editor.detectIndentation": false}"#);
        assert_eq!(s.document.settings.tab_size, 4, "back to the setting");
        assert!(!s.document.indentation_overridden);
    }

    #[test]
    fn an_unsupported_auto_save_value_reaches_the_problem_list() {
        // The frontend shows every entry, which is how a setting that does nothing
        // manages to say so.
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "files.autoSave", json!("onFocusChange"));
        let s = Session::new(settings, None, Platform::Linux);
        assert_eq!(s.problems.len(), 1, "{:?}", s.problems);
        assert!(s.problems[0].contains("onFocusChange"), "{:?}", s.problems);
    }

    #[test]
    fn the_same_complaint_is_not_made_twice() {
        // The settings are re-resolved on a language change, a rename and a workspace
        // layer arriving; a problem list of copies is a problem list nobody reads.
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "files.autoSave", json!("onWindowChange"));
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.rs"), "x\n");
        s.set_language(Some("markdown"));
        s.set_workspace_settings("{}");
        assert_eq!(s.problems.len(), 1, "{:?}", s.problems);
    }

    #[test]
    fn a_supported_auto_save_value_says_nothing() {
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "files.autoSave", json!("afterDelay"));
        let s = Session::new(settings, None, Platform::Linux);
        assert!(s.problems.is_empty(), "{:?}", s.problems);
    }

    // ---- Word wrap ---------------------------------------------------------

    #[test]
    fn alt_z_wraps_the_document_and_says_where() {
        // The column is worth saying: it is the one thing about wrapping that is
        // not visible from the text, and it is what the setting controls.
        let mut s = session();
        s.open(
            PathBuf::from("/w/a.md"),
            &format!("{}\n", "word ".repeat(30)),
        );
        s.resize(24, 8);
        assert_eq!(
            press(&mut s, "alt+z"),
            Outcome::Message("Word wrap on, at column 20".to_owned()),
            "24 columns less a four-column gutter"
        );
        assert_eq!(
            press(&mut s, "alt+z"),
            Outcome::Message("Word wrap off".to_owned())
        );
    }

    #[test]
    fn wrapping_changes_how_many_rows_a_line_takes() {
        let mut s = session();
        s.open(
            PathBuf::from("/w/a.md"),
            &format!("{}\n", "word ".repeat(30)),
        );
        s.resize(24, 8);
        let rows = |s: &Session| {
            s.view
                .visible_rows(&s.document.buffer, &s.document.settings)
                .len()
        };
        assert_eq!(rows(&s), 2, "one long line and the empty last one");
        press(&mut s, "alt+z");
        assert_eq!(rows(&s), 8, "the window fills with rows of one line");
    }

    #[test]
    fn toggling_back_on_restores_what_the_setting_asked_for() {
        // Somebody who configured `bounded` and pressed the key twice asked to get
        // back what they had, not the width of the window.
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                Scope::User,
                r#"{"editor.wordWrap": "bounded", "editor.wordWrapColumn": 12}"#,
            )
            .unwrap();
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.md"), "x\n");
        s.resize(80, 8);

        assert_eq!(
            s.document.settings.word_wrap,
            deco_config::WordWrap::Bounded
        );
        press(&mut s, "alt+z");
        assert_eq!(s.document.settings.word_wrap, deco_config::WordWrap::Off);
        press(&mut s, "alt+z");
        assert_eq!(
            s.document.settings.word_wrap,
            deco_config::WordWrap::Bounded,
            "not `on`"
        );
        assert_eq!(s.view.wrap_column(&s.document.settings), 12);
    }

    #[test]
    fn a_language_override_of_word_wrap_is_what_the_toggle_restores() {
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(Scope::User, r#"{"[markdown]": {"editor.wordWrap": "on"}}"#)
            .unwrap();
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.md"), "x\n");
        s.resize(40, 8);
        assert!(
            s.view.wrap_column(&s.document.settings) > 0,
            "on for markdown"
        );
        press(&mut s, "alt+z");
        press(&mut s, "alt+z");
        assert_eq!(s.document.settings.word_wrap, deco_config::WordWrap::On);
    }

    #[test]
    fn word_wrap_is_per_tab() {
        // The settings are per document, so turning it on to read one file leaves
        // the code in the next tab alone — which is what makes the key worth having
        // rather than a setting to edit.
        let mut s = session();
        s.open(PathBuf::from("/w/prose.md"), "x\n");
        s.open(PathBuf::from("/w/main.rs"), "y\n");
        s.resize(40, 8);

        s.run("workbench.action.previousEditor", None, 0);
        assert_eq!(s.document.title(), "prose.md");
        press(&mut s, "alt+z");
        assert!(s.view.wrap_column(&s.document.settings) > 0);

        s.run("workbench.action.nextEditor", None, 0);
        assert_eq!(s.document.title(), "main.rs");
        assert_eq!(
            s.view.wrap_column(&s.document.settings),
            0,
            "the other tab is untouched"
        );

        s.run("workbench.action.previousEditor", None, 0);
        assert!(
            s.view.wrap_column(&s.document.settings) > 0,
            "and coming back finds it still on"
        );
    }

    #[test]
    fn a_split_group_wraps_at_its_own_narrower_width() {
        // Two groups share the width, so the same line wraps sooner in each. A
        // group left with the whole width's wrap column would draw past its column.
        let mut s = session();
        s.open(
            PathBuf::from("/w/a.md"),
            &format!("{}\n", "word ".repeat(30)),
        );
        s.resize(80, 8);
        press(&mut s, "alt+z");
        let single = s.view.wrap_column(&s.document.settings);

        s.run("workbench.action.splitEditor", None, 0);
        s.resize(80, 8);
        let split = s.view.wrap_column(&s.document.settings);
        assert!(split < single, "{split} should be narrower than {single}");
        let other = s.split_view.as_ref().unwrap().text_width;
        assert!(
            other.abs_diff(s.view.text_width) <= 1,
            "both groups get their own, within the cell the remainder goes to: \
             {other} and {}",
            s.view.text_width
        );
    }

    #[test]
    fn a_frontend_that_does_not_wrap_makes_the_setting_inert() {
        // The GPU frontend lays out one line per row. A session that wrapped anyway
        // would scroll and move the caret by rows nothing draws, putting the caret
        // in one place and the text it is on in another.
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                Scope::User,
                r#"{"editor.wordWrap": "wordWrapColumn", "editor.wordWrapColumn": 20}"#,
            )
            .unwrap();
        let mut s = Session::new(settings, None, Platform::Linux);
        s.frontend_wraps = false;
        s.open(
            PathBuf::from("/w/a.md"),
            &format!("{}\n", "word ".repeat(30)),
        );
        s.resize(80, 8);

        assert_eq!(
            s.view.wrap_column(&s.document.settings),
            0,
            "not even `wordWrapColumn`, which ignores the window"
        );
        assert_eq!(
            s.view
                .visible_rows(&s.document.buffer, &s.document.settings)
                .len(),
            2,
            "one row for the long line and one for the empty last one"
        );
    }

    // ---- Search in files ---------------------------------------------------

    #[test]
    fn ctrl_shift_f_asks_what_to_look_for() {
        // It used to search for the seed straight away, so a project search could
        // only ever look for what the cursor happened to be on.
        let mut s = searchable("alpha beta\n");
        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 1));
        assert_eq!(press(&mut s, "ctrl+shift+f"), Outcome::Handled);
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::SearchQuery);
        assert_eq!(
            prompt.text(),
            "alpha",
            "seeded with the word under the cursor"
        );
    }

    #[test]
    fn accepting_the_search_prompt_hands_over_the_query_and_its_options() {
        let mut s = searchable("x\n");
        s.run("workbench.action.findInFiles", None, 0);
        press(&mut s, "ctrl+x");
        for key in ["t", "o", "d", "o"] {
            press(&mut s, key);
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::SearchInFiles {
                query: "todo".to_owned(),
                options: deco_core::search::SearchOptions::default(),
            }
        );
    }

    #[test]
    fn a_project_search_and_the_find_bar_have_their_own_options() {
        // One pair of booleans meant case-sensitivity set for a search across the
        // workspace changed what the next ctrl+f matched. VS Code keeps them apart.
        let mut s = searchable("x\n");
        s.run("workbench.action.findInFiles", None, 0);
        press(&mut s, "alt+c");
        assert_eq!(
            s.status.as_deref(),
            Some("Search: case on, whole word off"),
            "a toggle nobody can see is a toggle nobody trusts"
        );
        assert!(
            !s.find.options().case_sensitive,
            "the find bar is untouched"
        );

        press(&mut s, "ctrl+x");
        press(&mut s, "x");
        let Outcome::SearchInFiles { options, .. } = press(&mut s, "enter") else {
            panic!("the search should run");
        };
        assert!(options.case_sensitive, "the search kept its own");
    }

    #[test]
    fn an_empty_search_query_says_so_rather_than_walking_the_workspace() {
        let mut s = session();
        s.resize(80, 10);
        s.run("workbench.action.findInFiles", None, 0);
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Message("nothing to search for".to_owned())
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

    // ---- Save as, and open by path ----------------------------------------

    #[test]
    fn save_as_seeds_the_prompt_with_the_current_path() {
        // Editing the path you are already in beats typing a whole one.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/notes.txt"), "text\n");
        assert_eq!(press(&mut s, "ctrl+shift+s"), Outcome::Handled);
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::SaveAs);
        assert_eq!(prompt.text(), "/w/notes.txt");
    }

    #[test]
    fn open_file_seeds_the_prompt_with_the_directory_only() {
        // The point is to open something else, so a seed whose last component you
        // have to delete is a seed that cost you.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/src/main.rs"), "fn main() {}\n");
        s.run("workbench.action.files.openFile", None, 0);
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::OpenPath);
        assert_eq!(prompt.text(), "/w/src/");
    }

    #[test]
    fn an_untitled_document_gets_an_empty_seed_rather_than_a_guess() {
        let mut s = session();
        s.resize(80, 10);
        s.run("workbench.action.files.saveAs", None, 0);
        assert_eq!(s.prompt.as_ref().unwrap().text(), "");
    }

    #[test]
    fn accepting_save_as_hands_the_path_to_the_frontend() {
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/notes.txt"), "text\n");
        s.run("workbench.action.files.saveAs", None, 0);
        // `ctrl+x` clears a one-line input, which is how a seed is replaced rather
        // than appended to. `ctrl+a` is swallowed: half a selection in a field with
        // no selection would be worse than none.
        press(&mut s, "ctrl+x");
        for key in ["a", ".", "t", "o", "m", "l"] {
            press(&mut s, key);
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::SaveAs(PathBuf::from("a.toml"))
        );
    }

    #[test]
    fn accepting_a_typed_path_opens_it_the_same_way_quick_open_does() {
        let mut s = searchable("x\n");
        s.run("workbench.action.files.openFile", None, 0);
        press(&mut s, "ctrl+x");
        for key in ["a", ".", "r", "s"] {
            press(&mut s, key);
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::OpenFile {
                path: PathBuf::from("a.rs"),
                at: None,
            }
        );
    }

    #[test]
    fn an_empty_path_says_so_rather_than_writing_somewhere() {
        // Untitled, so the seed is empty to begin with.
        let mut s = session();
        s.resize(80, 10);
        s.run("workbench.action.files.saveAs", None, 0);
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Message("no filename given".to_owned())
        );
        s.run("workbench.action.files.openFile", None, 0);
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Message("no filename given".to_owned())
        );
    }

    #[test]
    fn renaming_redetects_the_language_from_the_new_name() {
        // `notes.txt` saved as `notes.toml` is a TOML file now.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/notes.txt"), "name = 1\n");
        assert_eq!(s.document.language(), None);

        assert_eq!(
            s.rename_to(PathBuf::from("/w/notes.toml")),
            Outcome::Message("Saved /w/notes.toml".to_owned())
        );
        assert_eq!(s.document.language(), Some("toml"));
        assert!(s.document.syntax.is_active());
        assert!(
            !s.document.dirty,
            "what is on disk is what is in the buffer"
        );
        assert_eq!(s.document.path.as_deref(), Some(Path::new("/w/notes.toml")));
    }

    #[test]
    fn renaming_keeps_a_language_that_was_chosen_by_hand() {
        // Having said "this is TOML" and then saved it, being told it is now plain
        // text would undo a decision nobody revisited.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/notes.txt"), "name = 1\n");
        s.set_language(Some("toml"));

        s.rename_to(PathBuf::from("/w/other.txt"));
        assert_eq!(s.document.language(), Some("toml"));
    }

    #[test]
    fn renaming_a_detected_language_still_follows_the_name() {
        // The mirror of the case above: nothing was chosen by hand here, so the
        // name is still the only evidence there is.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/main.rs"), "fn main() {}\n");
        assert_eq!(s.document.language(), Some("rust"));
        s.rename_to(PathBuf::from("/w/main.py"));
        assert_eq!(s.document.language(), Some("python"));
    }

    // ---- Panes ------------------------------------------------------------

    #[test]
    fn there_is_one_pane_and_it_describes_the_active_group() {
        let mut s = searchable("one\ntwo\n");
        s.open(PathBuf::from("/w/b.rs"), "fn main() {}\n");
        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 3));

        let panes = s.panes();
        assert_eq!(panes.len(), 1);
        let pane = &panes[0];
        assert!(pane.focused, "the only group has the keyboard");
        assert_eq!(pane.document.path.as_deref(), Some(Path::new("/w/b.rs")));
        assert_eq!(pane.view.cursor(), Position::new(0, 3));
        assert_eq!(pane.tabs.len(), 2, "both tabs belong to this group");
        assert!(pane.tabs.iter().any(|tab| tab.active));
    }

    #[test]
    fn a_pane_borrows_rather_than_copies() {
        // A second copy of the document could disagree with the one being edited.
        let mut s = searchable("x\n");
        press(&mut s, "y");
        let panes = s.panes();
        assert_eq!(panes[0].document.buffer.text(), "yx\n");
    }

    // ---- Picker selection --------------------------------------------------

    #[test]
    fn typing_in_a_picker_selects_the_best_match() {
        // `enter` runs whatever is selected, so it has to be the best match for
        // what has been typed. The selection used to follow the previously
        // selected entry — which starts on row 0, whatever the registry listed
        // first, so nobody chose it — and stayed there however badly it ranked.
        let mut s = searchable("x\n");
        s.run("workbench.action.showCommands", None, 0);
        press(&mut s, "down");
        let arrowed = s.prompt.as_ref().unwrap().selected().unwrap().id.clone();

        for key in ["u", "n", "d", "o"] {
            press(&mut s, key);
        }
        let prompt = s.prompt.as_ref().expect("still open");
        assert_eq!(prompt.selected_row(), 0);
        assert_ne!(prompt.selected().unwrap().id, arrowed);
        assert_eq!(prompt.selected().unwrap().id, "undo");
    }

    #[test]
    fn deleting_from_a_picker_reranks_too() {
        // Widening is a new query, so the best match for it is what should be
        // selected rather than the best match for the longer one.
        let mut s = searchable("x\n");
        s.run("workbench.action.editor.changeLanguageMode", None, 0);
        for key in ["j", "s", "o", "n"] {
            press(&mut s, key);
        }
        assert_eq!(s.prompt.as_ref().unwrap().selected().unwrap().title, "JSON");
        press(&mut s, "backspace");
        let prompt = s.prompt.as_ref().unwrap();
        assert_eq!(prompt.selected_row(), 0, "the top of the narrowed list");
    }

    // ---- One file, one tab -------------------------------------------------

    #[test]
    fn a_file_reached_by_two_spellings_is_one_tab() {
        // `deco src/main.rs` and then picking the same file from `ctrl+p` used to
        // open it twice: two buffers, two undo histories, and whichever was saved
        // last winning silently.
        let mut s = session();
        s.resize(80, 10);
        s.open(PathBuf::from("/w/src/main.rs"), "fn main() {}\n");
        s.open(PathBuf::from("/w/./src/../src/main.rs"), "fn main() {}\n");
        assert_eq!(s.tab_count(), 1);
    }

    #[test]
    fn switching_to_it_keeps_the_edits_rather_than_rereading() {
        // The point of one tab per file: the second open is a switch, so unsaved
        // work is still there.
        let mut s = session();
        s.resize(80, 10);
        s.open(PathBuf::from("/w/a.rs"), "saved\n");
        press(&mut s, "y");
        s.open(PathBuf::from("/w/./a.rs"), "saved\n");
        assert_eq!(s.document.buffer.text(), "ysaved\n");
        assert_eq!(s.tab_count(), 1);
    }

    #[test]
    fn normalising_leaves_a_leading_parent_alone() {
        // `../a.rs` points somewhere; dropping the `..` would change where.
        assert_eq!(normalise(Path::new("../a.rs")), PathBuf::from("../a.rs"));
        assert_eq!(
            normalise(Path::new("../../a.rs")),
            PathBuf::from("../../a.rs")
        );
        assert_eq!(normalise(Path::new("/../a.rs")), PathBuf::from("/../a.rs"));
    }

    #[test]
    fn normalising_resolves_what_it_can() {
        assert_eq!(normalise(Path::new("/w/./a.rs")), PathBuf::from("/w/a.rs"));
        assert_eq!(
            normalise(Path::new("/w/src/../a.rs")),
            PathBuf::from("/w/a.rs")
        );
        assert_eq!(normalise(Path::new("/w/a.rs")), PathBuf::from("/w/a.rs"));
    }

    #[test]
    fn two_different_files_are_still_two_tabs() {
        let mut s = session();
        s.resize(80, 10);
        s.open(PathBuf::from("/w/a.rs"), "a\n");
        s.open(PathBuf::from("/w/b.rs"), "b\n");
        assert_eq!(s.tab_count(), 2);
    }

    // ---- Reverting and quitting -------------------------------------------

    #[test]
    fn reverting_an_untitled_document_empties_it_and_makes_it_closable() {
        // There is no file to re-read, and empty is what it was — which is the
        // route out of a scratch buffer that could otherwise be neither saved nor
        // closed.
        let mut s = session();
        s.resize(80, 10);
        press(&mut s, "y");
        assert!(s.document.dirty);

        assert_eq!(
            s.run("workbench.action.files.revert", None, 0),
            Outcome::Message("Reverted".to_owned())
        );
        assert_eq!(s.document.buffer.text(), "");
        assert!(!s.document.dirty);
        assert_eq!(press(&mut s, "ctrl+w"), Outcome::Handled);
    }

    #[test]
    fn reverting_a_file_asks_the_frontend_for_what_is_on_disk() {
        let mut s = searchable("saved\n");
        press(&mut s, "y");
        assert_eq!(
            s.run("workbench.action.files.revert", None, 0),
            Outcome::Revert
        );

        // What the frontend does with it.
        assert_eq!(
            s.revert_to("saved\n"),
            Outcome::Message("Reverted a.txt".to_owned())
        );
        assert_eq!(s.document.buffer.text(), "saved\n");
        assert!(!s.document.dirty);
    }

    #[test]
    fn a_revert_can_be_undone() {
        // A command whose whole purpose is to destroy work should not be the one
        // command that cannot be taken back.
        let mut s = searchable("saved\n");
        press(&mut s, "y");
        assert_eq!(s.document.buffer.text(), "ysaved\n");
        s.run("workbench.action.files.revert", None, 0);
        s.revert_to("saved\n");

        press(&mut s, "ctrl+z");
        assert_eq!(s.document.buffer.text(), "ysaved\n");
    }

    #[test]
    fn revert_and_close_closes_once_the_text_comes_back() {
        let mut s = searchable("saved\n");
        s.open(PathBuf::from("/w/b.rs"), "fn main() {}\n");
        press(&mut s, "y");
        assert_eq!(
            s.run("workbench.action.revertAndCloseActiveEditor", None, 0),
            Outcome::Revert
        );
        assert_eq!(s.tab_count(), 2);
        s.revert_to("fn main() {}\n");
        assert_eq!(s.tab_count(), 1, "reverted, then closed");
    }

    #[test]
    fn reverting_a_clean_document_says_there_is_nothing_to_do() {
        let mut s = searchable("saved\n");
        assert_eq!(
            s.run("workbench.action.files.revert", None, 0),
            Outcome::Message("a.txt has no changes".to_owned())
        );
    }

    #[test]
    fn quitting_with_unsaved_work_refuses_and_names_it() {
        // The editor already refuses to close one unsaved document with ctrl+w;
        // dropping all of them on ctrl+q applied that principle to the narrower of
        // the two paths.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/b.rs"), "fn main() {}\n");
        press(&mut s, "y");

        assert_eq!(
            press(&mut s, "ctrl+q"),
            Outcome::Message(
                "1 tab has unsaved changes: b.rs — ctrl+q again to quit anyway".to_owned()
            )
        );
    }

    #[test]
    fn every_unsaved_tab_is_named_not_just_the_one_on_screen() {
        let mut s = searchable("x\n");
        press(&mut s, "y");
        s.open(PathBuf::from("/w/b.rs"), "fn main() {}\n");
        press(&mut s, "z");

        let Outcome::Message(report) = press(&mut s, "ctrl+q") else {
            panic!("quit should be refused");
        };
        assert!(
            report.starts_with("2 tabs have unsaved changes: "),
            "{report}"
        );
        assert!(report.contains("a.txt"), "{report}");
        assert!(report.contains("b.rs"), "{report}");
    }

    #[test]
    fn a_second_quit_goes_through() {
        let mut s = searchable("x\n");
        press(&mut s, "y");
        assert!(matches!(press(&mut s, "ctrl+q"), Outcome::Message(_)));
        assert_eq!(press(&mut s, "ctrl+q"), Outcome::Quit);
    }

    #[test]
    fn anything_in_between_starts_the_conversation_again() {
        // Acting minutes later on an answer nobody remembers giving is how a
        // confirmation becomes a formality.
        let mut s = searchable("x\n");
        press(&mut s, "y");
        assert!(matches!(press(&mut s, "ctrl+q"), Outcome::Message(_)));
        press(&mut s, "left");
        assert!(
            matches!(press(&mut s, "ctrl+q"), Outcome::Message(_)),
            "the refusal should be offered again"
        );
    }

    #[test]
    fn quitting_with_nothing_unsaved_just_quits() {
        let mut s = searchable("x\n");
        assert_eq!(press(&mut s, "ctrl+q"), Outcome::Quit);
    }

    // ---- Split editor -----------------------------------------------------

    #[test]
    fn splitting_gives_the_same_document_a_second_view() {
        // One buffer, two views. Two documents would be two divergent copies of
        // one file, which is what `open` refuses for tabs.
        let mut s = searchable("one\ntwo\nthree\n");
        assert_eq!(s.group_count(), 1);
        press(&mut s, "ctrl+\\");
        assert_eq!(s.group_count(), 2);

        let panes = s.panes();
        assert_eq!(panes.len(), 2);
        assert!(std::ptr::eq(panes[0].document, panes[1].document));
        // The new group starts where the old one was looking and takes the
        // keyboard, because you split in order to work in the new one.
        assert!(!panes[0].focused);
        assert!(panes[1].focused);
    }

    #[test]
    fn each_group_scrolls_and_moves_on_its_own() {
        // The whole point: two places in one file, at once.
        let mut s = searchable(&"line\n".repeat(60));
        s.resize(80, 10);
        press(&mut s, "ctrl+\\");
        s.view.scroll_top = 40;
        s.view.selections = deco_core::SelectionSet::caret(Position::new(42, 0));

        let panes = s.panes();
        assert_eq!(panes[0].view.scroll_top, 0, "the first group stayed put");
        assert_eq!(panes[1].view.scroll_top, 40);
    }

    #[test]
    fn ctrl_1_and_ctrl_2_move_the_keyboard_between_the_groups() {
        let mut s = searchable(&"line\n".repeat(60));
        s.resize(80, 10);
        press(&mut s, "ctrl+\\");
        s.view.scroll_top = 40;

        press(&mut s, "ctrl+1");
        assert_eq!(s.view.scroll_top, 0, "the first group's view is now active");
        assert!(s.panes()[0].focused);

        press(&mut s, "ctrl+2");
        assert_eq!(s.view.scroll_top, 40, "and the second group's is back");
        assert!(s.panes()[1].focused);
    }

    #[test]
    fn typing_goes_into_the_group_with_the_keyboard() {
        // Both groups show the edit, since there is one document — but only the
        // focused view's cursor moved.
        let mut s = searchable("abc\n");
        press(&mut s, "ctrl+\\");
        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 3));
        press(&mut s, "d");

        assert_eq!(s.document.buffer.text(), "abcd\n");
        let panes = s.panes();
        assert_eq!(panes[1].view.cursor(), Position::new(0, 4));
        assert_eq!(
            panes[0].view.cursor(),
            Position::new(0, 0),
            "the other group's cursor stayed where it was"
        );
    }

    #[test]
    fn splitting_twice_says_it_is_already_split() {
        let mut s = searchable("x\n");
        press(&mut s, "ctrl+\\");
        assert_eq!(
            press(&mut s, "ctrl+\\"),
            Outcome::Message("the editor is already split".to_owned())
        );
        assert_eq!(s.group_count(), 2);
    }

    #[test]
    fn focusing_a_group_that_is_not_there_says_so() {
        let mut s = searchable("x\n");
        assert_eq!(
            press(&mut s, "ctrl+2"),
            Outcome::Message("there is only one editor group".to_owned())
        );
        press(&mut s, "ctrl+\\");
        assert_eq!(
            press(&mut s, "ctrl+3"),
            Outcome::Message("there are only 2 editor groups".to_owned())
        );
    }

    #[test]
    fn ctrl_w_closes_the_group_before_it_closes_the_tab() {
        // Having split, the first thing that key should do is put the screen back.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/b.rs"), "fn main() {}\n");
        press(&mut s, "ctrl+\\");
        assert_eq!(s.tab_count(), 2);

        assert_eq!(
            press(&mut s, "ctrl+w"),
            Outcome::Message("Closed the second group".to_owned())
        );
        assert_eq!(s.group_count(), 1);
        assert_eq!(s.tab_count(), 2, "and the tab is still open");

        // With one group again, it closes the tab as it always did.
        press(&mut s, "ctrl+w");
        assert_eq!(s.tab_count(), 1);
    }

    #[test]
    fn moving_between_groups_closes_the_find_bar() {
        // Its matches were found against the other view, and its current match is
        // where that group's cursor is.
        let mut s = searchable("hello hello\n");
        press(&mut s, "ctrl+\\");
        s.run("actions.find", None, 0);
        assert!(s.find.visible());
        press(&mut s, "ctrl+1");
        assert!(!s.find.visible());
    }

    // ---- Colour theme -----------------------------------------------------

    #[test]
    fn ctrl_k_ctrl_t_asks_the_frontend_for_the_installed_themes() {
        // The frontend's, because a marketplace theme is a file in an extension
        // directory and the core has no filesystem.
        let mut s = searchable("x\n");
        assert_eq!(press(&mut s, "ctrl+k"), Outcome::Handled);
        assert_eq!(
            press(&mut s, "ctrl+t"),
            Outcome::Frontend("workbench.action.selectTheme".to_owned())
        );
    }

    #[test]
    fn choosing_a_theme_names_the_file_to_read() {
        let mut s = searchable("x\n");
        s.offer_themes(vec![
            commands::PaletteEntry::new("", "Default Dark Modern").with_detail("dark"),
            commands::PaletteEntry::new("/ext/owl.json", "Night Owl").with_detail("dark"),
        ]);
        let prompt = s.prompt.as_ref().expect("a picker should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::Themes);
        assert_eq!(prompt.matches(), 2);

        for key in ["o", "w", "l"] {
            press(&mut s, key);
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::LoadTheme {
                label: "Night Owl".to_owned(),
                path: Some(PathBuf::from("/ext/owl.json")),
            }
        );
    }

    #[test]
    fn a_builtin_theme_names_no_file() {
        let mut s = searchable("x\n");
        s.offer_themes(vec![commands::PaletteEntry::new(
            "",
            "Default Light Modern",
        )]);
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::LoadTheme {
                label: "Default Light Modern".to_owned(),
                path: None,
            }
        );
    }

    #[test]
    fn the_theme_list_keeps_the_order_it_was_given() {
        // The built-ins are the ones that always work, so they stay at the top
        // rather than being buried by whatever is installed.
        let mut s = searchable("x\n");
        s.offer_themes(vec![
            commands::PaletteEntry::new("", "Default Dark Modern"),
            commands::PaletteEntry::new("", "Default Light Modern"),
            commands::PaletteEntry::new("/ext/a.json", "Aardvark"),
        ]);
        let titles: Vec<String> = s
            .prompt
            .as_ref()
            .unwrap()
            .visible()
            .iter()
            .map(|entry| entry.title.clone())
            .collect();
        assert_eq!(
            titles,
            ["Default Dark Modern", "Default Light Modern", "Aardvark"]
        );
    }

    #[test]
    fn setting_a_theme_says_how_to_keep_it() {
        // deco reads `workbench.colorTheme` and never writes it: an editor that
        // edits your configuration behind you is worse than one that tells you
        // what to put in it.
        let mut s = searchable("x\n");
        let light = deco_theme::defaults::builtin("Default Light Modern").unwrap();
        assert_eq!(
            s.set_theme(light),
            Outcome::Message(
                "Theme: Default Light Modern — set `workbench.colorTheme` to keep it".to_owned()
            )
        );
        assert_eq!(s.theme.name, "Default Light Modern");
    }

    #[test]
    fn nothing_matching_what_was_typed_in_the_theme_picker_says_so() {
        let mut s = searchable("x\n");
        s.offer_themes(vec![commands::PaletteEntry::new("", "Default Dark Modern")]);
        for key in ["z", "z"] {
            press(&mut s, key);
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Message("no theme matches `zz`".to_owned())
        );
    }

    #[test]
    fn an_empty_theme_list_reports_rather_than_opening_a_picker() {
        let mut s = searchable("x\n");
        s.offer_themes(Vec::new());
        assert!(s.prompt.is_none());
        assert_eq!(s.status.as_deref(), Some("no themes found"));
    }

    // ---- Change language mode ---------------------------------------------

    #[test]
    fn ctrl_k_m_offers_every_language_and_auto_detect() {
        let mut s = searchable("x\n");
        assert_eq!(press(&mut s, "ctrl+k"), Outcome::Handled);
        assert_eq!(press(&mut s, "m"), Outcome::Handled);
        let prompt = s.prompt.as_ref().expect("a picker should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::Languages);
        assert_eq!(prompt.matches(), crate::document::LANGUAGES.len() + 1);
        // Detection first, because it is the only way back once a language has
        // been chosen by hand.
        assert_eq!(
            prompt
                .selected()
                .map(|entry| entry.title.clone())
                .as_deref(),
            Some("Auto Detect")
        );
    }

    #[test]
    fn choosing_a_language_relexes_and_reresolves_the_settings() {
        // A `.txt` file that is really TOML: nothing about its name says so, so
        // the lexer is idle until it is told.
        let mut settings = deco_config::Settings::with_defaults();
        settings
            .load_layer(
                Scope::User,
                r#"{ "editor.tabSize": 4, "[toml]": { "editor.tabSize": 2 } }"#,
            )
            .unwrap();
        let mut s = Session::new(settings, None, Platform::Linux);
        s.resize(80, 10);
        s.open(PathBuf::from("/w/notes.txt"), "name = \"deco\"\n");
        assert_eq!(s.document.language(), None);
        assert!(!s.document.syntax.is_active());
        assert_eq!(s.document.settings.tab_size, 4);

        assert_eq!(
            s.set_language(Some("toml")),
            Outcome::Message("Language: TOML".to_owned())
        );
        assert_eq!(s.document.language(), Some("toml"));
        assert!(s.document.syntax.is_active(), "the lexer wakes up");
        assert_eq!(
            s.document.settings.tab_size, 2,
            "and `[toml]` now applies to this document"
        );
        assert_eq!(s.context.get("editorLangId"), Some(&json!("toml")));
    }

    #[test]
    fn auto_detect_goes_back_to_what_the_file_name_says() {
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/main.rs"), "fn main() {}\n");
        s.set_language(Some("python"));
        assert_eq!(s.document.language(), Some("python"));

        assert_eq!(
            s.set_language(None),
            Outcome::Message("Language: Rust".to_owned())
        );
        assert_eq!(s.document.language(), Some("rust"));
    }

    #[test]
    fn auto_detect_on_a_file_nothing_matches_says_so() {
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/notes.txt"), "text\n");
        assert_eq!(
            s.set_language(None),
            Outcome::Message("Language: none — nothing matches this file name".to_owned())
        );
        assert_eq!(s.document.language(), None);
        assert_eq!(s.context.get("editorLangId"), None);
    }

    #[test]
    fn accepting_a_language_from_the_picker_applies_it() {
        let mut s = searchable("x\n");
        s.run("workbench.action.editor.changeLanguageMode", None, 0);
        for key in ["r", "u", "s", "t"] {
            press(&mut s, key);
        }
        assert_eq!(
            press(&mut s, "enter"),
            Outcome::Message("Language: Rust".to_owned())
        );
        assert_eq!(s.document.language(), Some("rust"));
        assert!(s.prompt.is_none(), "the picker closes");
    }

    #[test]
    fn changing_the_language_leaves_the_text_alone() {
        // Nothing about a document's bytes depends on which language it is said
        // to be, and an undo step here would be a lie.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/notes.txt"), "name = 1\n");
        s.set_language(Some("toml"));
        assert_eq!(s.document.buffer.text(), "name = 1\n");
        assert!(!s.document.dirty, "and it is not an edit");
    }

    #[test]
    fn a_language_deco_has_no_name_for_is_shown_as_its_identifier() {
        // One can arrive from a settings file or a server. Showing the identifier
        // is more useful than showing nothing.
        assert_eq!(crate::document::language_title("rust"), "Rust");
        assert_eq!(crate::document::language_title("brainfuck"), "brainfuck");
    }

    #[test]
    fn every_language_the_file_name_can_detect_is_offerable() {
        // Otherwise a document could be in a mode the picker cannot get back to.
        for name in [
            "a.rs",
            "a.ts",
            "a.tsx",
            "a.js",
            "a.jsx",
            "a.py",
            "a.go",
            "a.c",
            "a.cpp",
            "a.java",
            "a.rb",
            "a.sh",
            "a.json",
            "a.jsonc",
            "a.toml",
            "a.yaml",
            "a.md",
            "a.html",
            "a.css",
            "a.sql",
            "a.lua",
            "a.xml",
            "Makefile",
            "Dockerfile",
            "Cargo.toml",
        ] {
            let detected = crate::document::language_for_path(Path::new(name))
                .unwrap_or_else(|| panic!("{name} should detect"));
            assert!(
                crate::document::LANGUAGES
                    .iter()
                    .any(|(id, _)| *id == detected),
                "{detected} is detected from {name} but is not in LANGUAGES"
            );
        }
    }

    #[test]
    fn the_picker_orders_titles_the_way_a_reader_scans_them() {
        // Byte order would put every capital below every lowercase letter, so
        // `JSON` would come before `Java` and the list would be unpredictable to
        // scan. Asserted through the real picker rather than against a copy of the
        // comparison.
        let mut s = searchable("x\n");
        s.run("workbench.action.editor.changeLanguageMode", None, 0);
        press(&mut s, "j");
        let titles: Vec<String> = s
            .prompt
            .as_ref()
            .expect("open")
            .visible()
            .iter()
            .map(|entry| entry.title.clone())
            .collect();
        assert_eq!(
            titles,
            [
                "Java",
                "JavaScript",
                "JavaScript React",
                "JSON",
                "JSON with Comments"
            ]
        );
    }

    // ---- Save All ---------------------------------------------------------

    /// A session with three tabs, two of them edited.
    fn three_tabs() -> Session {
        let mut s = session();
        s.resize(80, 10);
        s.open(PathBuf::from("/w/a.txt"), "a\n");
        s.open(PathBuf::from("/w/b.txt"), "b\n");
        s.open(PathBuf::from("/w/c.txt"), "c\n");
        // Edit the first and the last, leaving the middle one clean.
        s.run("workbench.action.previousEditor", None, 0);
        s.run("workbench.action.previousEditor", None, 0);
        press(&mut s, "x");
        s.run("workbench.action.nextEditor", None, 0);
        s.run("workbench.action.nextEditor", None, 0);
        press(&mut s, "y");
        s
    }

    #[test]
    fn save_all_writes_every_edited_tab_and_leaves_the_clean_ones_alone() {
        let mut s = three_tabs();
        let mut written = Vec::new();
        let outcome = s.save_all(|path, contents| {
            written.push((path.to_path_buf(), contents.to_owned()));
            Ok(())
        });

        assert_eq!(
            written,
            vec![
                (PathBuf::from("/w/a.txt"), "xa\n".to_owned()),
                (PathBuf::from("/w/c.txt"), "yc\n".to_owned()),
            ],
            "in tab order, and only the edited ones"
        );
        assert_eq!(outcome, Outcome::Message("Saved 2 files".to_owned()));
        // And nothing is dirty afterwards, including the tabs off screen.
        assert!(s.unsaved().is_empty());
    }

    #[test]
    fn a_failed_write_leaves_that_document_dirty() {
        // A tab that looks saved and is not is how work gets lost.
        let mut s = three_tabs();
        let outcome = s.save_all(|path, _| {
            if path == Path::new("/w/a.txt") {
                Err("/w/a.txt: permission denied".to_owned())
            } else {
                Ok(())
            }
        });

        let still = s.unsaved();
        assert_eq!(still.len(), 1);
        assert_eq!(still[0].0, PathBuf::from("/w/a.txt"));
        assert_eq!(
            outcome,
            Outcome::Message("Saved 1 file; 1 could not be written".to_owned())
        );
        // The reason is kept where a reader can find it: a status bar has one line.
        assert_eq!(s.problems, ["/w/a.txt: permission denied"]);
    }

    #[test]
    fn an_untitled_document_is_counted_rather_than_given_a_name() {
        let mut s = session();
        s.resize(80, 10);
        press(&mut s, "x");
        let outcome = s.save_all(|_, _| panic!("nothing to write"));
        assert_eq!(
            outcome,
            Outcome::Message("Saved 0 files; 1 document has no filename yet".to_owned())
        );
        assert_eq!(s.unsaved_untitled(), 1);
    }

    #[test]
    fn save_all_with_nothing_to_save_says_so() {
        let mut s = session();
        s.resize(80, 10);
        s.open(PathBuf::from("/w/a.txt"), "a\n");
        assert_eq!(
            s.save_all(|_, _| panic!("nothing to write")),
            Outcome::Message("Nothing to save".to_owned())
        );
    }

    #[test]
    fn each_tab_is_written_with_its_own_settings() {
        // `files.insertFinalNewline` can differ per language, and a batch save has
        // to respect each tab's rather than the active one's.
        //
        // `files.eol` is pinned rather than left at `auto`: a document with no
        // existing line ending takes the platform's, so the newline this appends
        // would be CRLF on Windows and the assertion would be about the host
        // instead of about the setting under test.
        let mut settings = deco_config::Settings::with_defaults();
        settings
            .load_layer(
                Scope::User,
                r#"{ "files.eol": "\n",
                     "files.insertFinalNewline": false,
                     "[markdown]": { "files.insertFinalNewline": true } }"#,
            )
            .unwrap();
        let mut s = Session::new(settings, None, Platform::Linux);
        s.resize(80, 10);
        s.open(PathBuf::from("/w/notes.md"), "notes");
        s.open(PathBuf::from("/w/a.txt"), "plain");
        press(&mut s, "x");
        s.run("workbench.action.previousEditor", None, 0);
        press(&mut s, "y");

        let mut written = std::collections::HashMap::new();
        s.save_all(|path, contents| {
            written.insert(path.to_path_buf(), contents.to_owned());
            Ok(())
        });
        assert_eq!(
            written.get(Path::new("/w/notes.md")).map(String::as_str),
            Some("ynotes\n"),
            "markdown gets its own final newline"
        );
        assert_eq!(
            written.get(Path::new("/w/a.txt")).map(String::as_str),
            Some("xplain"),
            "and the text file does not"
        );
    }

    #[test]
    fn ctrl_k_s_saves_everything() {
        let mut s = three_tabs();
        assert_eq!(press(&mut s, "ctrl+k"), Outcome::Handled, "the chord waits");
        assert_eq!(press(&mut s, "s"), Outcome::SaveAll);
    }

    // ---- No bound key does nothing ----------------------------------------

    #[test]
    fn every_default_binding_resolves_to_something_that_answers() {
        // The guard that makes a dead key impossible to add by accident. A command
        // nothing handles and that is not on `commands::PENDING` returns
        // `NotFound`, which the frontend has nowhere to put — so the key would do
        // nothing at all, which is indistinguishable from a hung editor.
        let mut dead = Vec::new();
        for rule in deco_keymap::defaults::default_rules(Platform::Linux) {
            let mut s = searchable("fn main() {}\n");
            let command = &rule.binding().command;
            if s.run(command, None, 0) == Outcome::NotFound {
                dead.push(command.clone());
            }
        }
        dead.sort();
        dead.dedup();
        assert!(
            dead.is_empty(),
            "these bound commands answer nothing — implement them or add them to \
             commands::PENDING: {dead:?}"
        );
    }

    #[test]
    fn an_unimplemented_command_says_which_feature_it_is() {
        let mut s = searchable("x\n");
        assert_eq!(
            s.run("workbench.action.togglePanel", None, 0),
            Outcome::Message("Toggle Panel is not implemented yet".to_owned())
        );
        assert_eq!(
            s.status.as_deref(),
            Some("Toggle Panel is not implemented yet")
        );
    }

    #[test]
    fn an_identifier_that_does_not_exist_says_that_instead() {
        // A different fact from "not built yet", and usually a typo in somebody's
        // keybindings.json rather than a missing feature.
        let mut s = searchable("x\n");
        assert_eq!(s.run("editor.action.nonsense", None, 0), Outcome::NotFound);
        assert_eq!(
            s.status.as_deref(),
            Some("there is no command `editor.action.nonsense`")
        );
    }

    #[test]
    fn nothing_pending_is_offered_in_the_palette() {
        // A palette entry has to work when chosen. One that only apologises is
        // worse than a shorter list.
        let s = searchable("x\n");
        let offered: Vec<&str> = s
            .palette()
            .iter()
            .map(|e| e.id.clone())
            .map(|id| {
                commands::PENDING
                    .iter()
                    .find(|(pending, _)| *pending == id)
                    .map(|(pending, _)| *pending)
                    .unwrap_or("")
            })
            .filter(|id| !id.is_empty())
            .collect();
        assert!(offered.is_empty(), "{offered:?}");
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
    fn switching_tabs_parks_the_find_bar_with_its_own_tab() {
        // The bar belongs to the tab, so switching away puts it down rather than
        // throwing it away, and switching back finds it as it was left.
        let mut s = session();
        s.resize(80, 10);
        s.open(PathBuf::from("/w/a.txt"), "foo\n");
        s.open(PathBuf::from("/w/b.txt"), "bar\n");
        press(&mut s, "ctrl+f");
        press(&mut s, "b");
        assert!(s.find.visible());
        assert_eq!(s.find.matches().len(), 1, "b.txt has one `b`");

        // To a.txt, which has no bar of its own.
        press(&mut s, "ctrl+tab");
        assert!(!s.find.visible(), "a.txt was never searched");
        assert_eq!(
            s.find.query(),
            "b",
            "the search string is shared, as it is in VS Code"
        );

        // And back.
        press(&mut s, "ctrl+tab");
        assert!(s.find.visible(), "b.txt's bar is where it was left");
        assert_eq!(s.find.matches().len(), 1);
    }

    #[test]
    fn two_tabs_can_be_searching_for_different_things() {
        // One match list per session was what made a switch have to discard it.
        let mut s = session();
        s.resize(80, 10);
        s.open(PathBuf::from("/w/a.txt"), "aaa\n");
        press(&mut s, "ctrl+f");
        press(&mut s, "a");
        s.open(PathBuf::from("/w/b.txt"), "bbbb\n");
        press(&mut s, "ctrl+f");
        // The query came over, so replace it.
        press(&mut s, "ctrl+x");
        press(&mut s, "b");
        assert_eq!(s.find.matches().len(), 4);

        press(&mut s, "ctrl+shift+tab");
        assert_eq!(s.find.query(), "a", "a.txt kept its own");
        assert_eq!(s.find.matches().len(), 3);
    }

    #[test]
    fn a_new_document_in_the_same_tab_still_drops_the_matches() {
        // `open` on a pristine untitled tab replaces the document rather than
        // adding a tab, and then the matches really are stale.
        let mut s = session();
        s.resize(80, 10);
        press(&mut s, "ctrl+f");
        press(&mut s, "x");
        s.open(PathBuf::from("/w/a.txt"), "xxx\n");
        assert!(!s.find.visible());
        assert_eq!(s.find.query(), "x", "the query still survives");
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
