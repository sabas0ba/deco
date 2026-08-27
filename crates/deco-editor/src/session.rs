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

/// Rows the side bar spends on its heading before the tree starts.
///
/// The title and the blank line under it. Here rather than in a renderer
/// because the session subtracts them to know how many rows the tree can scroll
/// within, and two frontends drawing the same heading have to agree with it.
pub const EXPLORER_CHROME_ROWS: usize = 2;

/// Which region of the window has the keyboard.
///
/// VS Code's own division, and the reason its `when` clauses can say
/// `sideBarFocus`: a key means one thing in the text and another in a tree, and
/// the keymap is where that is decided rather than in each command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// The text.
    #[default]
    Editor,
    /// The side bar.
    SideBar,
    /// The panel.
    Panel,
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
    /// Whether the query being typed is the first half of a replace.
    ///
    /// `ctrl+shift+f` and `ctrl+shift+h` open the same prompt and differ only in
    /// what accepting it does, so which key was pressed has to survive until
    /// then.
    replacing_in_files: bool,
    /// The query a replace-in-files is waiting to be given a replacement for.
    replace_query: String,
    /// Whether the side bar is showing.
    ///
    /// Whether it *fits* is a different question, answered by
    /// [`crate::layout::regions`] against the size of the window — so a toggle
    /// on a narrow terminal is remembered and takes effect when the window
    /// grows, rather than being silently refused.
    side_bar: bool,
    /// Whether the panel is showing.
    panel: bool,
    /// Which region has the keyboard.
    focus: Focus,
    /// File operations that can be taken back, most recent last.
    ///
    /// The explorer's own undo, separate from the text's — which is what VS Code
    /// does too, and the only arrangement that makes sense here: `ctrl+z` in a
    /// buffer means "put my characters back", and having it also move files
    /// because the last thing you did happened to be in the tree would make it
    /// unpredictable in both places. Focus decides which stack a press reaches,
    /// the same way it decides everything else.
    ///
    /// Holds the *inverse* of what was done, so undoing is running what is on
    /// top. A delete contributes nothing — it has no inverse — so it clears the
    /// stack rather than leaving entries below it that `ctrl+z` would jump to.
    explorer_undo: Vec<crate::files::Operation>,
    /// The undo the frontend is carrying out, if one is in flight.
    ///
    /// Popped off [`Session::explorer_undo`] and held here until the frontend
    /// says whether it worked, so that a refusal can put it back. Without it a
    /// transient failure — undoing `a → b` while another program has just made
    /// an `a` — would eat the entry and leave nothing to retry.
    pending_undo: Option<crate::files::Operation>,
    /// Files a delete took away that a language server still has open.
    closed_documents: Vec<PathBuf>,
    /// The workspace tree, once a frontend has said where the workspace is.
    ///
    /// `None` until then, because the session does not derive the root itself:
    /// working it out needs a working directory and the path deco was started
    /// with, neither of which the core has. Making the root a full session
    /// concept is the first step of the roadmap's workspace-switching chapter;
    /// this is the tree's own copy of it, not that.
    explorer: Option<crate::Explorer>,
    /// The rectangle the frontend last handed over.
    ///
    /// Kept because toggling a region has to re-divide the same window, and the
    /// session is the only one that knows the division changed. Without it a
    /// toggle would have to wait for the next resize to take effect.
    screen: (usize, usize),
    /// The number the next multi-document edit will be tagged with.
    ///
    /// Handed out here because this is the layer that can see more than one
    /// document; a buffer's history only holds the number it was given.
    next_group: u64,
    /// What `git status` last said, if anyone has run it.
    ///
    /// Fed, exactly as directory listings are, and for the same reason: the
    /// core has no filesystem and no way to spawn a process. `None` covers
    /// three different situations that look the same from here — nobody has
    /// asked yet, there is no git, this is not a repository — and the frontend
    /// that ran the command is the one that can tell them apart.
    scm: Option<deco_scm::Status>,
    /// Whether the status is stale.
    ///
    /// The same shape as [`Session::directory_wanted`]: the session says what
    /// it would like to know, and whoever can find out does. Set when
    /// something has happened that a `git status` would report differently —
    /// not on a keystroke, because running git on every key would be a
    /// process per character.
    scm_wanted: bool,
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
    let Some(transaction) = build_transaction(document, edits)? else {
        return Ok(0);
    };
    Ok(commit(document, view, &transaction, now_ms, None))
}

/// Turns a server's edits into the one transaction that performs them, or says
/// why they cannot be performed at all.
///
/// `Ok(None)` means there was nothing to do. Separated from [`commit`] so that a
/// caller changing several documents can find out whether *all* of them can be
/// changed before changing any: everything that can refuse an edit — a range
/// that is not there, two edits over the same text — refuses here, with every
/// buffer still untouched.
fn build_transaction(
    document: &Document,
    edits: &[deco_lsp::TextEdit],
) -> Result<Option<deco_core::Transaction>, EditError> {
    use deco_core::{Change, Transaction};

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
        return Ok(None);
    }

    // Overlapping edits have no well-defined result. The specification
    // forbids them, so a server sending them is broken — and guessing which
    // to honour would corrupt the file silently, which is worse than
    // refusing and saying so.
    Transaction::new(changes)
        .map(Some)
        .map_err(|_| EditError::Overlapping)
}

/// Applies a prepared transaction, recording one undo step, and returns how many
/// replacements it made.
///
/// `group` tags that step as part of a change several documents share, which is
/// what lets one `ctrl+z` take all of them back together.
fn commit(
    document: &mut Document,
    view: &mut View,
    transaction: &deco_core::Transaction,
    now_ms: u64,
    group: Option<deco_core::Group>,
) -> usize {
    use deco_core::EditKind;

    let applied = transaction.changes().len();
    let before = view.selections.clone();
    let inverse = document.apply(transaction);

    let cursor = document.buffer.clamp_position(before.primary().active);
    let after = deco_core::SelectionSet::caret(cursor);
    view.selections = after.clone();
    match group {
        Some(group) => document
            .history
            .record_in_group(inverse, before, after, now_ms, group),
        None => document
            .history
            .record(inverse, EditKind::Discrete, before, after, now_ms),
    }
    document.dirty = true;
    // Revealed even for a background tab: when it is switched to, the cursor
    // should be where the edit left it rather than wherever it was parked.
    view.reveal_cursor(&document.buffer, &document.settings);
    applied
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
            side_bar: false,
            panel: false,
            focus: Focus::Editor,
            explorer: None,
            explorer_undo: Vec::new(),
            pending_undo: None,
            closed_documents: Vec::new(),
            // Replaced by the first `resize`, which every frontend does before
            // it draws. The default matches `View`'s so a session nobody sized
            // still lays out sensibly under test.
            screen: (80, 24),
            next_group: 0,
            scm: None,
            // Wanted from the start: the branch is worth showing before the
            // first save, not after it.
            scm_wanted: true,
            replacing_in_files: false,
            replace_query: String::new(),
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
        let mut all: Vec<Tab> = std::mem::take(&mut self.left);
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
        // VS Code's own keys for the chrome. Visible and focused are separate:
        // `ctrl+b` shows the side bar without taking the keyboard into it, so a
        // clause gated on `sideBarVisible` and one gated on `sideBarFocus` are
        // asking different questions.
        let regions = self.regions();
        self.context
            .set("sideBarVisible", regions.side_bar.is_some());
        self.context.set("panelVisible", regions.panel.is_some());
        self.context
            .set("sideBarFocus", self.focus == Focus::SideBar);
        self.context.set("panelFocus", self.focus == Focus::Panel);
        // The tree's own keys. `filesExplorerFocus` is VS Code's name for the
        // explorer having the keyboard, and `listFocus` for any list having it —
        // the explorer is the only list here, so today they agree, and a `when`
        // clause copied from VS Code that uses either one resolves.
        let explorer_focus = self.focus == Focus::SideBar && self.explorer.is_some();
        self.context.set("filesExplorerFocus", explorer_focus);
        self.context.set("listFocus", explorer_focus);
        self.context
            .set("explorerViewletVisible", regions.side_bar.is_some());
        // Everything below describes the text, and while a region has the
        // keyboard the text does not have it — which is what stops a binding
        // gated on `editorTextFocus` from resolving in a tree.
        let in_editor = self.focus == Focus::Editor;
        self.context.set(
            "editorTextFocus",
            in_editor && !find_focus && !in_quick_open,
        );
        self.context.set("editorFocus", in_editor);
        self.context.set("textInputFocus", in_editor);
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
            // An undo of something that happened to several documents at once
            // has to reach all of them. Intercepted here rather than in
            // `commands` for the usual reason: a command there sees one document
            // and this is the layer that can see the rest.
            //
            // Only when the step on top is a shared one. Ordinary editing —
            // which is almost every `ctrl+z` — falls through to the same
            // single-document undo it always used.
            "undo"
                if self.focus == Focus::Editor && self.document.history.undo_group().is_some() =>
            {
                let group = self.document.history.undo_group().expect("just checked");
                self.undo_group(group, false)
            }
            "redo"
                if self.focus == Focus::Editor && self.document.history.redo_group().is_some() =>
            {
                let group = self.document.history.redo_group().expect("just checked");
                self.undo_group(group, true)
            }
            // The chrome. Session-level because a region changes how much of
            // the window the text has, which every group and the wrap depend on.
            "workbench.action.toggleSidebarVisibility" => self.toggle_side_bar(),
            "workbench.action.togglePanel" => self.toggle_panel(),
            "workbench.action.closeSidebar" => self.show_side_bar(false),
            "workbench.action.closePanel" => self.show_panel(false),
            "workbench.files.action.focusFilesExplorer" => self.focus_region(Focus::SideBar),
            "revealInExplorer" => self.reveal_active_file(),
            // The tree's own keys. Routed before the focus guard below, because
            // unlike the editor's commands these are *for* whatever has the
            // keyboard. Whether the tree is what has it is decided inside, not
            // by a guard here: these commands exist whatever has focus, and an
            // arm that stopped matching would report them as unknown — which is
            // what the frontend says when a binding is a typo.
            "list.focusDown" | "list.focusUp" | "list.focusFirst" | "list.focusLast"
            | "list.expand" | "list.collapse" | "list.select" => self.explorer_key(command),
            // The tree's undo, reached by the same key as the text's and told
            // apart by what has the keyboard. The workspace-edit arms above are
            // gated on editor focus for the same reason: after a project-wide
            // replace the document has a shared step waiting, and `ctrl+z` in
            // the tree must still mean the tree's undo rather than reaching past
            // it into the text.
            "undo" if self.focus == Focus::SideBar => self.undo_file_operation(),
            // The prompts the tree's mutations ask through. They act on what is
            // *selected* in the tree, which is model state and exists whether or
            // not the tree has the keyboard — so no focus guard here, and
            // invoking one from the palette works. The keys are what needs
            // telling apart, and the keymap does it: `F2` and `delete` are bound
            // to these only under `sideBarFocus`, so in the text they still
            // rename a symbol and delete a character.
            "explorer.newFile" => self.open_tree_prompt(PromptKind::NewFile),
            "explorer.newFolder" => self.open_tree_prompt(PromptKind::NewFolder),
            "renameFile" => self.open_rename_file(),
            "deleteFile" => self.open_tree_prompt(PromptKind::ConfirmDelete),
            "workbench.action.focusSideBar" => self.focus_region(Focus::SideBar),
            "workbench.action.focusPanel" => self.focus_region(Focus::Panel),
            // VS Code's way back to the text from anywhere in the chrome.
            "workbench.action.focusActiveEditorGroup" => self.focus_region(Focus::Editor),
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
                self.replacing_in_files = false;
                self.prompt = Some(Prompt::seeded(
                    PromptKind::SearchQuery,
                    self.search_seed().unwrap_or_default(),
                ));
                self.refresh_context();
                Outcome::Handled
            }
            // The same first question, remembered as the first half of a
            // different one. VS Code opens its search view with the replace box
            // showing; deco has one prompt at a time, so it asks in the order
            // the answers are needed.
            "workbench.action.replaceInFiles" => {
                self.replacing_in_files = true;
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
            | "editor.action.rename"
            | "editor.action.quickFix"
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
            // The editor's own commands belong to the editor. While a region has
            // the keyboard they are swallowed rather than run: `commands` is
            // where typing, motion, undo and the clipboard live, and every one
            // of them acts on the document — which is not what has focus.
            //
            // A guard here rather than a `when` clause on each binding, because
            // the fallback that types an unbound printable key never went
            // through the keymap at all, and a clause cannot reach it.
            _ if self.focus != Focus::Editor => Outcome::Handled,
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
        self.prompt = Some(Prompt::prefixed(PromptKind::OpenPath, seed));
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
        let chosen_by_hand = self.document.language_pinned;
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
        // `Some` is a choice; `None` is "work it out from the name again", which
        // is what unpinning means.
        self.document.language_pinned = language.is_some();
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
            PromptKind::NewFile => {
                let name = prompt.text().to_owned();
                self.create_in_tree(&name, false)
            }
            PromptKind::NewFolder => {
                let name = prompt.text().to_owned();
                self.create_in_tree(&name, true)
            }
            PromptKind::RenameFile => {
                let name = prompt.text().to_owned();
                self.rename_in_tree(&name)
            }
            PromptKind::ConfirmDelete => {
                // Only a typed `y` goes through. Enter on an empty box is what
                // happens when somebody dismisses a prompt they did not read,
                // and it must not be the answer that deletes their file.
                if prompt.text().trim().eq_ignore_ascii_case("y") {
                    self.delete_in_tree()
                } else {
                    Outcome::Message("nothing was deleted".to_owned())
                }
            }
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
                // Not trimmed away entirely: a query of spaces is a real thing
                // to look for, and only an *empty* one has nothing to do.
                let typed = prompt.text();
                if typed.is_empty() {
                    self.replacing_in_files = false;
                    return Outcome::Message("nothing to search for".to_owned());
                }
                if self.replacing_in_files {
                    // The second half. The query is parked rather than carried
                    // in the prompt, because a prompt is a line of text and this
                    // one is about to be a different line of text.
                    self.replace_query = typed.to_owned();
                    self.prompt = Some(Prompt::plain(PromptKind::ReplaceQuery));
                    self.refresh_context();
                    return Outcome::Handled;
                }
                Outcome::SearchInFiles {
                    query: typed.to_owned(),
                    options: self.search_options,
                }
            }
            PromptKind::ReplaceQuery => {
                self.replacing_in_files = false;
                let query = std::mem::take(&mut self.replace_query);
                if query.is_empty() {
                    // Only reachable if the prompt was opened out of order.
                    return Outcome::Message("nothing to replace".to_owned());
                }
                // The replacement itself may be empty: "delete every occurrence
                // of this" is a thing people mean, and refusing it would make
                // the one destructive-looking case the one you cannot do.
                Outcome::ReplaceInFiles {
                    query,
                    replacement: prompt.text().to_owned(),
                    options: self.search_options,
                }
            }
            PromptKind::Rename => {
                let typed = prompt.text().trim();
                if typed.is_empty() {
                    return Outcome::Message("no new name given".to_owned());
                }
                // The prompt opens with the current name in it, so accepting it
                // unchanged is what happens when somebody presses F2 and then
                // enter. A round trip to the server for a rename to the same
                // name would come back as a diff of nothing, or — from a server
                // that does not check — as an edit per occurrence, marking every
                // file that mentions it dirty for no change at all.
                if self
                    .seed_from_document()
                    .is_some_and(|(name, _)| name == typed)
                {
                    return Outcome::Message(format!("`{typed}` is already its name"));
                }
                Outcome::Rename {
                    new_name: typed.to_owned(),
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
            PromptKind::CodeActions => match prompt.selected() {
                Some(entry) => Outcome::CodeAction(entry.id.clone()),
                None => Outcome::Message(format!("no action matches `{}`", prompt.text())),
            },
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

    /// Offers what a language server said it could do about the selection.
    ///
    /// Called by the frontend once the answer arrives, like
    /// [`Session::offer_symbols`]. Each entry's `id` is the frontend's own
    /// handle on the action, since the frontend is the one holding it.
    ///
    /// An empty list is reported rather than opening an empty prompt: `ctrl+.`
    /// on a line with nothing wrong with it is a reasonable thing to press, and
    /// a menu of nothing is a worse answer than a sentence.
    pub fn offer_code_actions(&mut self, actions: Vec<crate::commands::PaletteEntry>) {
        if actions.is_empty() {
            self.status = Some("no code actions here".to_owned());
            return;
        }
        self.prompt = Some(Prompt::list(PromptKind::CodeActions, actions));
        self.refresh_context();
    }

    /// Asks what to call the symbol under the cursor instead.
    ///
    /// Opened by the frontend rather than by `editor.action.rename` reaching
    /// here, because whether a rename is possible at all depends on a language
    /// server, which the frontend owns: a prompt that appears and then reports
    /// that this server cannot rename is a worse answer than not appearing.
    ///
    /// Seeded with the current name and with all of it selected, the way VS
    /// Code's rename box opens — so typing replaces it, and `end` keeps it to
    /// add a suffix.
    pub fn offer_rename(&mut self) -> Outcome {
        let Some((name, _)) = self.seed_from_document() else {
            return Outcome::Message("put the cursor on a name to rename it".to_owned());
        };
        self.prompt = Some(Prompt::seeded(PromptKind::Rename, name));
        self.refresh_context();
        Outcome::Handled
    }

    /// Resolves a server's [`deco_lsp::WorkspaceEdit`] against what is open.
    ///
    /// Nothing is changed. The plan that comes back names the files it still
    /// needs read — see [`crate::workspace::Plan::missing`] — and it is
    /// [`Session::apply_workspace_edit`] that acts on it.
    ///
    /// `resolve` turns one of the server's URIs into a path on the machine
    /// holding the files, and `version_of` answers with the version last sent to
    /// that server for a path. Both are callbacks because both belong to the LSP
    /// client, which lives in a frontend; the rules about what an unresolvable
    /// URI and a mismatched version *mean* live here, so that every frontend
    /// gets the same ones.
    pub fn plan_workspace_edit(
        &self,
        edit: &deco_lsp::WorkspaceEdit,
        resolve: impl Fn(&deco_lsp::uri::Uri) -> Option<PathBuf>,
        version_of: impl Fn(&Path) -> Option<i64>,
    ) -> Result<crate::workspace::Plan, crate::workspace::WorkspaceError> {
        crate::workspace::Plan::build(
            edit,
            resolve,
            |path| self.tab_of(path).is_some(),
            version_of,
        )
    }

    /// Plans a replacement of every occurrence of `needle` in `paths`.
    ///
    /// The result goes to [`Session::apply_workspace_edit`] like any other, so a
    /// replace across the workspace is one undoable action and files no tab
    /// holds are opened rather than written — the same rules a rename gets, for
    /// the same reasons.
    ///
    /// # The matches are found again here, not carried over
    ///
    /// The caller found these files by searching them, and it would be shorter
    /// to hand the positions along. It would also be wrong twice over. A search
    /// result names where a match *started*, and replacing needs where it ended;
    /// deriving the end from the needle's length assumes the fold that matched
    /// it was length-preserving, which case-insensitive matching does not
    /// promise. And a file the search read from disk may be open here with
    /// unsaved changes, in which case the buffer is the text that matters and
    /// the positions from disk point into a document that no longer exists.
    ///
    /// So each file is searched again, against the buffer when a tab holds one,
    /// by the same [`deco_core::search::find_all`] the find bar uses. Which also
    /// means the count reported afterwards is a count of what was replaced,
    /// rather than of what was found a moment earlier.
    ///
    /// `read` supplies the text of a file no tab holds, and is the caller's
    /// business because reading one is I/O — in a remote session, on another
    /// machine.
    pub fn plan_replacements(
        &self,
        paths: &[PathBuf],
        needle: &str,
        replacement: &str,
        options: deco_core::search::SearchOptions,
        mut read: impl FnMut(&Path) -> Result<String, String>,
    ) -> Result<crate::workspace::Plan, crate::workspace::WorkspaceError> {
        let mut documents = Vec::with_capacity(paths.len());
        for path in paths {
            let open = self.document_at_path(path);
            // Borrowed from the tab, or owned from the caller. The buffer is
            // built only in the second case: an open document already has one,
            // and rebuilding it would be the file's length of work per file.
            let (buffer, contents) = match open {
                Some(document) => (std::borrow::Cow::Borrowed(&document.buffer), None),
                None => {
                    let text = read(path).map_err(|reason| {
                        crate::workspace::WorkspaceError::Unreadable {
                            path: path.clone(),
                            reason,
                        }
                    })?;
                    (
                        std::borrow::Cow::Owned(deco_core::Buffer::from_text(&text)),
                        Some(text),
                    )
                }
            };

            let edits: Vec<deco_lsp::TextEdit> =
                deco_core::search::find_all(&buffer, needle, options)
                    .into_iter()
                    .map(|range| deco_lsp::TextEdit {
                        range,
                        new_text: replacement.to_owned(),
                    })
                    .collect();

            // A file whose matches were all in text that has since changed is
            // left out rather than opened for nothing.
            if edits.is_empty() {
                continue;
            }
            documents.push(crate::workspace::PlannedDocument {
                path: path.clone(),
                version: None,
                edits,
                open: open.is_some(),
                contents,
            });
        }
        Ok(crate::workspace::Plan::from_documents(documents))
    }

    /// Applies a planned workspace edit to every document it names, or to none.
    ///
    /// # Order
    ///
    /// Every transaction is built before any is applied. Building is where an
    /// edit can still be refused — overlapping ranges are caught there — so a
    /// refusal happens with every buffer still as it was. Only once all of them
    /// have been built does anything get written, and from that point nothing
    /// can fail.
    ///
    /// Files no tab holds are opened as background tabs from the text
    /// [`crate::workspace::Plan::with_contents`] supplied, and they are opened
    /// *after* the same check, so a refusal does not leave tabs behind either.
    ///
    /// Every document records its step under one shared group, which is what
    /// [`Session::run`] reads to undo the whole change at once.
    pub fn apply_workspace_edit(
        &mut self,
        mut plan: crate::workspace::Plan,
        now_ms: u64,
    ) -> Result<crate::workspace::Applied, crate::workspace::WorkspaceError> {
        use crate::workspace::WorkspaceError;

        // Documents this edit brings in, built here rather than opened, so that
        // a refusal below leaves the session with the tabs it started with.
        let mut opened: Vec<(Document, View)> = Vec::new();
        // For each planned document: where to find it when committing, and the
        // transaction to commit. `None` for a document with nothing to do.
        let mut prepared: Vec<(usize, Option<deco_core::Transaction>)> = Vec::new();

        for (index, planned) in plan.documents_mut().iter().enumerate() {
            let transaction = if planned.open {
                let document = self
                    .document_at_path(&planned.path)
                    .expect("planned as open, and nothing has closed a tab since");
                build_transaction(document, &planned.edits)
            } else {
                let text =
                    planned
                        .contents
                        .as_deref()
                        .ok_or_else(|| WorkspaceError::Unreadable {
                            path: planned.path.clone(),
                            reason: "its text was never supplied".to_owned(),
                        })?;
                let language = crate::document::language_for_path(&planned.path);
                let settings = EditorSettings::resolve(&self.settings, language);
                let document = Document::from_file(planned.path.clone(), text, settings);
                let built = build_transaction(&document, &planned.edits);
                opened.push((
                    document,
                    View {
                        height: self.view.height,
                        width: self.view.width,
                        ..Default::default()
                    },
                ));
                built
            };

            let transaction = transaction.map_err(|_| WorkspaceError::Overlapping {
                path: planned.path.clone(),
            })?;
            prepared.push((index, transaction));
        }

        // Past here nothing can refuse.
        let group = self.take_group();
        let mut applied = crate::workspace::Applied {
            documents: 0,
            edits: 0,
            opened: 0,
        };
        let mut newly_opened = opened.into_iter();

        for (index, transaction) in prepared {
            let planned = &plan.documents_mut()[index];
            let path = planned.path.clone();
            let was_open = planned.open;

            let (document, view) = if was_open {
                self.document_and_view_at_path(&path)
                    .expect("checked while planning")
            } else {
                let (document, view) = newly_opened
                    .next()
                    .expect("one was built for every document that was not open");
                // Pushed even when it had nothing to change: the file the server
                // named is part of what the user asked about, and a tab that
                // appears only sometimes is harder to reason about than one that
                // always does. `edits` below counts the real work.
                self.right.push(Tab {
                    document,
                    view,
                    diagnostics: Vec::new(),
                    semantic: Vec::new(),
                    find: Find::new(),
                });
                applied.opened += 1;
                let tab = self.right.last_mut().expect("just pushed");
                (&mut tab.document, &mut tab.view)
            };

            if let Some(transaction) = transaction {
                let count = commit(document, view, &transaction, now_ms, Some(group));
                applied.documents += 1;
                applied.edits += count;
            }
        }

        self.relayout();
        self.refresh_context();
        Ok(applied)
    }

    /// The next group number, and never this one again.
    fn take_group(&mut self) -> deco_core::Group {
        let group = deco_core::Group(self.next_group);
        // Saturating rather than wrapping: reusing a number would join two
        // unrelated changes into one undo step. At one group per refactor,
        // reaching the end of a `u64` is not a case that arises — but wrapping
        // silently into a *wrong* answer is not the way to handle it if it did.
        self.next_group = self.next_group.saturating_add(1);
        group
    }

    /// The document holding `path`, active tab or background.
    fn document_at_path(&self, path: &Path) -> Option<&Document> {
        let wanted = normalise(path);
        let matches =
            |document: &Document| document.path.as_deref().map(normalise) == Some(wanted.clone());
        if matches(&self.document) {
            return Some(&self.document);
        }
        self.left
            .iter()
            .chain(self.right.iter())
            .map(|tab| &tab.document)
            .find(|document| matches(document))
    }

    /// The same, with the view that goes with it.
    fn document_and_view_at_path(&mut self, path: &Path) -> Option<(&mut Document, &mut View)> {
        let wanted = normalise(path);
        let matches =
            |document: &Document| document.path.as_deref().map(normalise) == Some(wanted.clone());
        if matches(&self.document) {
            return Some((&mut self.document, &mut self.view));
        }
        self.left
            .iter_mut()
            .chain(self.right.iter_mut())
            .find(|tab| matches(&tab.document))
            .map(|tab| (&mut tab.document, &mut tab.view))
    }

    /// Undoes a change several documents share, in every document that took part.
    ///
    /// Called instead of the ordinary undo when the step on top of the active
    /// document's history is tagged. Every other document whose *next* step
    /// carries the same tag is undone with it — "next" being the point: a file
    /// edited by hand since the rename keeps that edit, and its share of the
    /// rename stays where it is in its own history, to come out when the edits
    /// on top of it have.
    fn undo_group(&mut self, group: deco_core::Group, redo: bool) -> Outcome {
        let mut documents = 0usize;
        for (document, view) in self.documents_and_views() {
            let next = if redo {
                document.history.redo_group()
            } else {
                document.history.undo_group()
            };
            if next != Some(group) {
                continue;
            }
            // The history applies its own transaction rather than going through
            // `Document::apply`, so the caches have to be dropped wholesale.
            document.invalidate();
            let selections = if redo {
                document.history.redo(&mut document.buffer)
            } else {
                document.history.undo(&mut document.buffer)
            };
            if let Some(selections) = selections {
                view.selections = selections;
                document.dirty = true;
                view.reveal_cursor(&document.buffer, &document.settings);
                documents += 1;
            }
        }

        self.relayout();
        let what = if redo { "Redone" } else { "Undone" };
        Outcome::Message(format!(
            "{what} across {}",
            if documents == 1 {
                "1 file".to_owned()
            } else {
                format!("{documents} files")
            }
        ))
    }

    /// Every open document and its view, active tab included.
    fn documents_and_views(&mut self) -> impl Iterator<Item = (&mut Document, &mut View)> {
        std::iter::once((&mut self.document, &mut self.view)).chain(
            self.left
                .iter_mut()
                .chain(self.right.iter_mut())
                .map(|tab| (&mut tab.document, &mut tab.view)),
        )
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
                self.scm_changed();
                return;
            }
        }
    }

    /// Marks the document as saved.
    pub fn mark_saved(&mut self) {
        self.document.dirty = false;
        self.document.history.break_group();
        // A write is the commonest reason `git status` now says something
        // else. Set on the *active* path only, which `mark_saved_at` reaches
        // through here for the active document and below for the rest.
        self.scm_changed();
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

    /// `ctrl+b`: shows or hides the side bar.
    fn toggle_side_bar(&mut self) -> Outcome {
        self.show_side_bar(!self.side_bar)
    }

    /// `ctrl+j`: shows or hides the panel.
    fn toggle_panel(&mut self) -> Outcome {
        self.show_panel(!self.panel)
    }

    fn show_side_bar(&mut self, showing: bool) -> Outcome {
        self.side_bar = showing;
        // Hiding what has the keyboard would leave the keyboard nowhere.
        if !showing && self.focus == Focus::SideBar {
            self.focus = Focus::Editor;
        }
        self.report_region("Side bar", showing, self.regions().side_bar.is_some())
    }

    fn show_panel(&mut self, showing: bool) -> Outcome {
        self.panel = showing;
        if !showing && self.focus == Focus::Panel {
            self.focus = Focus::Editor;
        }
        self.report_region("Panel", showing, self.regions().panel.is_some())
    }

    /// Re-divides the window and says what happened, if anything needs saying.
    ///
    /// Showing a region that does not fit is the one case worth a sentence: the
    /// key was pressed, nothing appeared, and without this that reads as an
    /// editor that ignored it. The state is kept all the same, so widening the
    /// window shows what was asked for.
    fn report_region(&mut self, what: &str, wanted: bool, fits: bool) -> Outcome {
        let (width, height) = self.screen;
        self.resize(width, height);
        if wanted && !fits {
            return Outcome::Message(format!(
                "no room for the {} in this window",
                what.to_lowercase()
            ));
        }
        Outcome::Handled
    }

    /// Opens one of the tree's prompts, refusing when there is nothing to ask
    /// about.
    fn open_tree_prompt(&mut self, kind: PromptKind) -> Outcome {
        if self.explorer.is_none() {
            return Outcome::Message("there is no workspace open".to_owned());
        }
        if kind == PromptKind::ConfirmDelete {
            let Some(row) = self.explorer.as_ref().and_then(crate::Explorer::selection) else {
                return Outcome::Message(crate::files::FileError::NoSelection.to_string());
            };
            // The name is in the question, so "delete permanently?" is never
            // asked about a file the reader has to go and look up.
            let what = if row.is_dir {
                format!("{} and everything in it", row.name)
            } else {
                row.name
            };
            // The box opens empty and the name goes in the status line: what is
            // typed is only the answer, and the thing being deleted is named
            // where it can be read without retyping it.
            self.prompt = Some(Prompt::seeded(kind, String::new()));
            self.status = Some(format!("delete {what}? this cannot be undone"));
            self.refresh_context();
            return Outcome::Handled;
        }
        self.prompt = Some(Prompt::seeded(kind, String::new()));
        self.refresh_context();
        Outcome::Handled
    }

    /// `F2` in the tree: the rename box, seeded with the current name.
    fn open_rename_file(&mut self) -> Outcome {
        let Some(row) = self.explorer.as_ref().and_then(crate::Explorer::selection) else {
            return Outcome::Message(crate::files::FileError::NoSelection.to_string());
        };
        self.prompt = Some(Prompt::seeded(PromptKind::RenameFile, row.name));
        self.refresh_context();
        Outcome::Handled
    }

    /// Builds the operation a typed name means, against what is selected.
    ///
    /// The directory a new file goes in is the selected row when it is a
    /// directory, and the selected row's parent when it is a file — which is
    /// what VS Code does, and what makes "new file" mean "next to this one"
    /// without a second question.
    fn target_dir(&self) -> Option<std::path::PathBuf> {
        let explorer = self.explorer.as_ref()?;
        match explorer.selection() {
            Some(row) if row.is_dir => Some(row.path),
            Some(row) => row.path.parent().map(Path::to_path_buf),
            // An empty tree still has a root to create things in.
            None => Some(explorer.root().to_path_buf()),
        }
    }

    /// `explorer.newFile` / `explorer.newFolder`: the name having been typed.
    ///
    /// Separate from opening the prompt, which is a frontend's job — this is the
    /// half that decides whether the answer is allowed.
    pub fn create_in_tree(&mut self, name: &str, folder: bool) -> Outcome {
        let Some(explorer) = self.explorer.as_ref() else {
            return Outcome::Message("no workspace to create anything in".to_owned());
        };
        let root = explorer.root().to_path_buf();
        let name = match crate::files::check_name(name) {
            Ok(name) => name,
            Err(error) => return Outcome::Message(error.to_string()),
        };
        let Some(dir) = self.target_dir() else {
            return Outcome::Message(crate::files::FileError::NoSelection.to_string());
        };
        let path = dir.join(name);
        if let Err(error) = crate::files::check_inside(&root, &path) {
            return Outcome::Message(error.to_string());
        }
        // Whether it exists is a question for the filesystem, and the tree's
        // answer is good enough to refuse on without asking: it lists what is
        // there. A race with another program is caught by the frontend, which
        // reports back and takes the undo entry off again.
        if explorer.rows().iter().any(|row| row.path == path) {
            return Outcome::Message(crate::files::FileError::Exists(name.to_owned()).to_string());
        }
        // The same check renaming makes, for the same reason: a tab can hold a
        // path the tree does not show, when another program deleted the file and
        // nothing here has noticed. Creating it again would succeed on disk and
        // then `Session::open` would switch to the *old* buffer rather than the
        // empty file — leaving the two disagreeing, and a save putting the old
        // contents back.
        let name = name.to_owned();
        if self.tab_of(&path).is_some() {
            return Outcome::Message(format!(
                "a tab is still open on `{name}` — close it before making a new one"
            ));
        }

        let operation = if folder {
            crate::files::Operation::CreateFolder(path)
        } else {
            crate::files::Operation::CreateFile(path)
        };
        self.record_file_operation(&operation);
        self.refresh_context();
        Outcome::FileOperation(operation)
    }

    /// `renameFile`: the new name having been typed.
    pub fn rename_in_tree(&mut self, name: &str) -> Outcome {
        let Some(explorer) = self.explorer.as_ref() else {
            return Outcome::Message("no workspace to rename anything in".to_owned());
        };
        let root = explorer.root().to_path_buf();
        let Some(row) = explorer.selection() else {
            return Outcome::Message(crate::files::FileError::NoSelection.to_string());
        };
        // Before trimming. The prompt opens seeded with the current name, so
        // accepting it unchanged must change nothing — and on a filesystem that
        // allows a name like `" report "`, trimming first would turn pressing
        // enter into a rename nobody asked for.
        if name == row.name {
            return Outcome::Handled;
        }
        let name = match crate::files::check_name(name) {
            Ok(name) => name,
            Err(error) => return Outcome::Message(error.to_string()),
        };
        let Some(dir) = row.path.parent() else {
            return Outcome::Message(crate::files::FileError::NoSelection.to_string());
        };
        let to = dir.join(name);
        if to == row.path {
            // Not an error, and not worth doing: renaming a file to what it is
            // called would still hit the disk and still invalidate the listing.
            return Outcome::Handled;
        }
        if let Err(error) = crate::files::check_inside(&root, &to) {
            return Outcome::Message(error.to_string());
        }
        if explorer.rows().iter().any(|other| other.path == to) {
            return Outcome::Message(crate::files::FileError::Exists(name.to_owned()).to_string());
        }
        // A tab can hold a path the tree does not show: a file deleted by
        // another program stays open here until something notices. Renaming onto
        // it would leave two buffers for one path, and whichever was saved last
        // would silently win — which is what `Session::open` exists to prevent,
        // reached by a different road.
        //
        // At *or under* it, because renaming a directory moves its whole
        // subtree: with `/w/b/x` open and `/w/a` renamed to `/w/b`, an exact
        // comparison against `/w/b` passes and then `/w/a/x` retargets onto the
        // path `/w/b/x` already holds — two buffers for one file by a longer
        // route. `open_paths_under` catches both, since a file path is its own
        // only descendant.
        let name = name.to_owned();
        if !self.open_paths_under(&to).is_empty() {
            return Outcome::Message(format!(
                "a tab is still open on `{name}` — close it before renaming onto it"
            ));
        }

        let operation = crate::files::Operation::Rename {
            directory: row.is_dir,
            from: row.path,
            to,
            expect: None,
        };
        self.record_file_operation(&operation);
        Outcome::FileOperation(operation)
    }

    /// `deleteFile`: the confirmation having been given.
    pub fn delete_in_tree(&mut self) -> Outcome {
        let Some(explorer) = self.explorer.as_ref() else {
            return Outcome::Message("no workspace to delete anything from".to_owned());
        };
        let root = explorer.root().to_path_buf();
        let Some(row) = explorer.selection() else {
            return Outcome::Message(crate::files::FileError::NoSelection.to_string());
        };
        if let Err(error) = crate::files::check_inside(&root, &row.path) {
            return Outcome::Message(error.to_string());
        }
        // The root itself is not a row, so this cannot delete the workspace —
        // checked anyway, because the cost of being wrong is the whole project.
        if row.path == root {
            return Outcome::Message("the workspace itself cannot be deleted".to_owned());
        }

        let operation = crate::files::Operation::Delete {
            directory: row.is_dir,
            path: row.path,
        };
        self.record_file_operation(&operation);
        Outcome::FileOperation(operation)
    }

    /// Lets go of every tab holding something under `gone`, and says how many.
    ///
    /// The buffers stay and their paths are dropped: the text is still the
    /// user's, and where it should live is a question only they can answer. A
    /// tab left pointing at a deleted path would recreate the file on the next
    /// save, or fail when its directory had gone too.
    ///
    /// Public because a *failed* recursive delete needs it as much as a
    /// successful one: `remove_dir_all` can remove half a tree and then stop,
    /// and the half that went is as gone as if it had all worked.
    pub fn detach_tabs_under(&mut self, gone: &Path) -> usize {
        let gone = normalise(gone);
        let affected = self.tabs_under(&gone);
        let held: Vec<PathBuf> = affected
            .iter()
            .filter_map(|index| self.path_of_tab(*index))
            .collect();
        // The paths first, while the tabs still have them: the server is holding
        // these open under URIs that no longer name anything.
        self.closed_documents.extend(held);

        let mut detached = 0usize;
        for index in affected {
            let Some(document) = self.document_at_index_mut(index) else {
                continue;
            };
            document.path = None;
            document.dirty = true;
            // Diagnostics and semantic tokens describe a file that is not there.
            // Every path that would refresh them returns early once the path is
            // `None`, so leaving them would keep squiggles from a deleted file on
            // screen for as long as the buffer lived.
            self.clear_analysis_of_tab(index);
            detached += 1;
        }
        detached
    }

    /// The paths of every open tab holding something under `path`.
    ///
    /// For a caller that has a filesystem and wants to ask about each one — a
    /// recursive delete that failed part way has removed some of these and not
    /// others, and only the disk knows which.
    pub fn open_paths_under(&self, path: &Path) -> Vec<PathBuf> {
        let under = normalise(path);
        self.tabs_under(&under)
            .into_iter()
            .filter_map(|index| self.path_of_tab(index))
            .collect()
    }

    /// Every path the tree knows about at or under `path`.
    ///
    /// For a caller that has a filesystem and needs to find out whether a delete
    /// that reported failure removed anything after all.
    pub fn known_paths_under(&self, path: &Path) -> Vec<PathBuf> {
        self.explorer
            .as_ref()
            .map(|explorer| explorer.known_paths_under(path))
            .unwrap_or_default()
    }

    /// Re-reads the directory a listing may have changed under.
    pub fn invalidate_directory(&mut self, dir: &Path) {
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.invalidate(dir);
        }
    }

    /// Drops what the tree remembers about `dir` and everything below it.
    ///
    /// For a path that is no longer the directory it was: invalidating would
    /// leave it in the map to be read again and answered as an empty folder,
    /// which is what it will look like from now on.
    pub fn forget_subtree(&mut self, dir: &Path) {
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.forget_under(dir);
        }
    }

    /// Re-reads `dir` and everything the tree knows below it.
    pub fn invalidate_subtree(&mut self, dir: &Path) {
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.invalidate_under(dir);
        }
    }

    /// Throws away the tree's undo history.
    ///
    /// For a recursive delete that may have half happened: something
    /// irreversible went, so every inverse below it describes a state that never
    /// existed. The same barrier a completed delete puts up, reached from the
    /// path where the delete reported failure.
    pub fn clear_file_undo(&mut self) {
        self.explorer_undo.clear();
        self.pending_undo = None;
    }

    /// Throws away the analysis attached to one tab.
    fn clear_analysis_of_tab(&mut self, index: usize) {
        let active = self.active_tab();
        if index == active {
            self.diagnostics.clear();
            self.semantic_tokens.clear();
        } else if index < active {
            if let Some(tab) = self.left.get_mut(index) {
                tab.diagnostics.clear();
                tab.semantic.clear();
            }
        } else if let Some(tab) = self.right.get_mut(index - active - 1) {
            tab.diagnostics.clear();
            tab.semantic.clear();
        }
    }

    /// Files the language server should be told are closed, and forgets them.
    ///
    /// Filled when a delete detaches a tab: the server has the file open under a
    /// URI that no longer names anything, and only a frontend can tell it. Drained
    /// rather than read, so one delete produces one `didClose` per file.
    pub fn take_closed_documents(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.closed_documents)
    }

    /// The path a tab holds, in display order.
    fn path_of_tab(&self, index: usize) -> Option<PathBuf> {
        let active = self.active_tab();
        let document = if index == active {
            &self.document
        } else if index < active {
            &self.left.get(index)?.document
        } else {
            &self.right.get(index - active - 1)?.document
        };
        document.path.clone()
    }

    /// Every tab holding `path` or something inside it, in display order.
    ///
    /// A file compares equal to itself; a directory catches its whole subtree.
    /// Normalised on both sides for the reason [`Session::tab_of`] gives: the
    /// same file can be spelled two ways, and a tab missed here keeps pointing
    /// at a path that no longer exists.
    fn tabs_under(&self, path: &Path) -> Vec<usize> {
        let wanted = normalise(path);
        (0..self.tab_count())
            .filter(|index| {
                self.path_of_tab(*index)
                    .map(|held| normalise(&held).starts_with(&wanted))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Points the tab at `index` at a different path.
    ///
    /// The buffer, its history and its unsaved changes all stay: the file moved,
    /// the document did not. Re-resolving the settings is what makes renaming
    /// `notes.txt` to `notes.md` start highlighting it as Markdown, unless the
    /// language was chosen by hand — in which case the choice outranks the
    /// extension, the same rule [`Session::rename_to`] follows for save-as.
    fn retarget_tab(&mut self, index: usize, to: PathBuf) {
        let Some(document) = self.document_at_index_mut(index) else {
            // An index no tab has: nothing to retarget, and inventing one would
            // be worse than doing nothing.
            return;
        };

        // Set when the inferred language changed, to whether there was one
        // before — a rename *away* from a recognised extension has to take the
        // server's work with it, and the server itself.
        let mut language_changed: Option<bool> = None;

        // A language the user picked by hand outranks the new extension, which
        // is the rule save-as follows too.
        let chosen_by_hand = document.language_pinned;
        let previous_path = document.path.clone();
        document.path = Some(to);
        if !chosen_by_hand {
            let inferred = document
                .path
                .as_deref()
                .and_then(crate::document::language_for_path)
                .map(str::to_owned);
            if inferred != document.language_id {
                let had_language = document.language_id.is_some();
                document.language_id = inferred;
                // The lexer goes with it. `set_language` rebuilds this and
                // `retarget_tab` did not, so a file renamed across languages
                // reported the new one and kept being highlighted as the old.
                document.syntax = deco_syntax::Syntax::new(document.language());
                language_changed = Some(had_language);
            }
        }
        let language = document.language().map(str::to_owned);

        // Re-resolved whatever tab this is. Doing it only for the active one
        // left a background tab renamed from `notes.txt` to `notes.md` still
        // treated as plain text for as long as the session lasted — switching to
        // a tab lays it out again, but does not re-resolve it.
        let settings = EditorSettings::resolve(&self.settings, language.as_deref());
        if let Some(document) = self.document_at_index_mut(index) {
            document.settings = settings;
            document.apply_overrides();
        }
        // Semantic tokens and diagnostics came from a server that was told about
        // the old path and the old language. When the rename takes the language
        // away entirely there is no server to correct them either — `attach`
        // returns early for a document with no language — so tokens from before
        // would keep overriding the freshly rebuilt lexer for good.
        if let Some(had_language) = language_changed {
            self.clear_analysis_of_tab(index);
            if had_language {
                if let Some(previous) = previous_path {
                    self.closed_documents.push(previous);
                }
            }
        }
        if index == self.active_tab() {
            self.report_unsupported();
        }
    }

    /// The document a display index holds, mutably.
    fn document_at_index_mut(&mut self, index: usize) -> Option<&mut Document> {
        let active = self.active_tab();
        if index == active {
            Some(&mut self.document)
        } else if index < active {
            self.left.get_mut(index).map(|tab| &mut tab.document)
        } else {
            self.right
                .get_mut(index - active - 1)
                .map(|tab| &mut tab.document)
        }
    }

    /// Puts an operation's inverse on the explorer's stack, if it has one.
    ///
    /// An operation with no inverse — a delete — does *not* clear the stack
    /// here. Recording happens before the frontend has tried, and a delete the
    /// filesystem refuses would otherwise throw away every earlier undo for a
    /// change that never happened. The clearing is in
    /// [`Session::file_operation_done`], where the delete is a fact.
    fn record_file_operation(&mut self, operation: &crate::files::Operation) {
        if let Some(inverse) = operation.inverse() {
            self.explorer_undo.push(inverse);
        }
    }

    /// Attaches what the moved file looks like to the undo waiting for it.
    ///
    /// Called by the frontend after a rename it has just carried out, because
    /// only it can look at the file. Undoing a rename otherwise names a path and
    /// trusts it — and a path is not a file: another program can remove the one
    /// that was renamed and leave something else where it was.
    pub fn stamp_last_undo(&mut self, stamp: crate::files::Stamp) {
        // Not while an undo is being carried out. That operation was *popped*
        // into `pending_undo`, so the top of the stack is the entry before it —
        // stamping there would describe the wrong file, and the next `ctrl+z`
        // would refuse to undo a rename that was perfectly undoable.
        if self.pending_undo.is_some() {
            return;
        }
        match self.explorer_undo.last_mut() {
            Some(crate::files::Operation::Rename { expect, .. })
            | Some(crate::files::Operation::DeleteIfEmpty { expect, .. }) => {
                *expect = Some(stamp);
            }
            _ => {}
        }
    }

    /// Whether the tree has anything to undo.
    pub fn can_undo_file_operation(&self) -> bool {
        !self.explorer_undo.is_empty()
    }

    /// `undo` while the tree has the keyboard: takes back the last operation.
    ///
    /// The undone operation's own inverse is **not** put back on. Doing that
    /// made `ctrl+z` a toggle: undo a rename, press it again, and the rename
    /// came back rather than the operation before it being undone — so every
    /// older entry was unreachable and the stack was one step deep in practice.
    /// Pressing it repeatedly now walks back through the history, and there is
    /// no redo for the tree.
    fn undo_file_operation(&mut self) -> Outcome {
        let Some(operation) = self.explorer_undo.last().cloned() else {
            return Outcome::Message("nothing in the tree to undo".to_owned());
        };
        // The same collision `rename_in_tree` refuses, asked again here. The
        // entry was recorded when the destination was free, and it need not
        // still be: rename `a` to `b`, open a new `a`, let something remove it,
        // and undoing would move `b` back onto the path that tab still holds —
        // two buffers for one file, from the key that is supposed to put things
        // as they were.
        //
        // Checked before popping, so a refusal leaves the undo where it is to be
        // tried again once the tab is closed.
        if let crate::files::Operation::Rename { to, .. } = &operation {
            if !self.open_paths_under(to).is_empty() {
                let name = to
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| to.display().to_string());
                return Outcome::Message(format!(
                    "a tab is still open on `{name}` — close it before undoing this"
                ));
            }
        }
        self.explorer_undo.pop();
        self.pending_undo = Some(operation.clone());
        Outcome::FileOperation(operation)
    }

    /// Takes an operation back off the stack after the disk refused it.
    ///
    /// The core recorded it before the frontend tried, because the frontend has
    /// to be told what to do before it can do it. When it does not work, the
    /// stack has to forget it — otherwise `ctrl+z` would offer to undo something
    /// that never happened.
    pub fn file_operation_failed(&mut self, operation: &crate::files::Operation, reason: &str) {
        match self.pending_undo.take() {
            // An undo that did not happen goes back on the stack, so it can be
            // tried again once whatever blocked it is out of the way.
            Some(pending) if &pending == operation => self.explorer_undo.push(pending),
            _ => {
                if let Some(inverse) = operation.inverse() {
                    if self.explorer_undo.last() == Some(&inverse) {
                        self.explorer_undo.pop();
                    }
                }
            }
        }
        // The path it was going to put something at is not what the tree
        // thinks it is: the attempt only happened because the tree believed
        // the name was free, so whatever it remembers there is a directory
        // that is gone. Forgotten rather than invalidated, because the
        // expansion is as stale as the listing — this is a different thing
        // under the same name, not the same thing with different contents.
        //
        // The success path does exactly this, for exactly this reason. Without
        // it here, the commonest way to *reach* the bug — a create refused
        // because another program took the name — is the one case not covered.
        if let (Some(explorer), Some(path)) = (self.explorer.as_mut(), operation.arriving()) {
            explorer.forget_under(path);
        }
        self.status = Some(format!("could not {}: {reason}", operation.describe()));
    }

    /// Retargets an open tab after its file moved, and re-reads the tree.
    ///
    /// Called by the frontend once the rename has actually happened. The tab and
    /// the file move together — that is the whole reason renaming goes through
    /// the session rather than being something the tree does on its own.
    pub fn file_operation_done(&mut self, operation: &crate::files::Operation) {
        self.pending_undo = None;
        // The keyboard follows a created file into the editor — the contract on
        // `Outcome::FileOperation` is that a created file is opened, and typing
        // into a tree that swallows the keys is an editor that ignores you.
        // Here rather than when the create was asked for, so a create the disk
        // refuses leaves the keyboard in the tree to try again.
        if matches!(operation, crate::files::Operation::CreateFile(_)) {
            self.focus = Focus::Editor;
        }
        // A tab pointing at something that has just been deleted would recreate
        // it on the next save, or fail fatally when its directory has gone too.
        // The buffer is kept and its path let go: the text is still the user's,
        // and where it should live is now a question only they can answer.
        let mut detached_tabs = 0usize;
        if let crate::files::Operation::Delete { path, .. }
        | crate::files::Operation::DeleteIfEmpty { path, .. } = operation
        {
            detached_tabs = self.detach_tabs_under(path);
        }

        // Nothing below a delete can be undone either: running an older inverse
        // would put a file back beside one that is now gone, which is not the
        // state anything was ever in. Here rather than when the delete was
        // recorded, so a refusal costs nothing.
        if matches!(operation, crate::files::Operation::Delete { .. }) {
            self.explorer_undo.clear();
        }
        if let crate::files::Operation::Rename { from, to, .. } = operation {
            // Every tab *under* `from`, not just one whose path equals it.
            // Renaming a directory moves its whole subtree on disk, and a tab
            // still pointing into the old tree would save to a path that is no
            // longer there — recreating the old directory, or failing.
            let from = normalise(from);
            for index in self.tabs_under(&from) {
                let path = self
                    .path_of_tab(index)
                    .expect("tabs_under only returns tabs with paths");
                // Stripped from the *normalised* path, which is what
                // `tabs_under` matched on. Against the raw one a tab spelled
                // `/w/./src/a.rs` is selected and then fails to strip, and is
                // left pointing into a directory that has moved.
                let moved = match normalise(&path).strip_prefix(&from) {
                    Ok(rest) if rest.as_os_str().is_empty() => to.clone(),
                    Ok(rest) => to.join(rest),
                    Err(_) => continue,
                };
                self.retarget_tab(index, moved);
            }
        }
        // A file that now exists, or no longer does, is a line `git status`
        // did not have before.
        self.scm_changed();
        if let (Some(explorer), Some(parent)) = (self.explorer.as_mut(), operation.parent()) {
            explorer.invalidate(parent);
            // What the tree remembered about the thing that moved or went. The
            // parent alone is not enough: a deleted directory keeps its cached
            // listing and its expansion, so creating one with the same name
            // later would show the old one's rows, already open.
            //
            // This half is only what *left* a path. What arrives at one is
            // below, in one place rather than per-operation.
            if let crate::files::Operation::Delete {
                path,
                directory: true,
            }
            | crate::files::Operation::DeleteIfEmpty {
                path,
                directory: true,
                ..
            } = operation
            {
                explorer.forget_under(path);
            }
            // The selection follows what was just made or moved. Without this,
            // creating a file leaves the selection on whatever was highlighted
            // before, and the next key — `F2`, `delete` — acts on *that*, which
            // is the wrong file and a destructive kind of wrong.
            //
            // `reveal` rather than a direct selection because the row does not
            // exist yet: the directory has just been invalidated and is read
            // again on the next turn. Landing the selection when the listing
            // arrives is exactly what `reveal` is for.
            // One rule, at every place something new arrives at a path: forget
            // what the tree remembered there first.
            //
            // A directory removed outside deco leaves its listing and its
            // expansion behind when a later mutation refreshes only the parent —
            // the row goes, the memory does not. Whatever then takes that name
            // inherits it: an empty folder rendering the old one's children, a
            // renamed directory showing a stranger's, a file with a subtree
            // hanging off it. In each case the tree never asks for a listing,
            // because it believes it already has one.
            //
            // Stated once, on the operation itself, having now been four
            // separate findings — created folder, renamed destination, the
            // created *file* nobody reported, and the same three again on the
            // path where the operation *failed*. See
            // [`crate::files::Operation::arriving`], which both this and
            // [`Session::file_operation_failed`] ask.
            if let Some(path) = operation.arriving() {
                explorer.forget_under(path);
            }

            match operation {
                crate::files::Operation::CreateFile(path)
                | crate::files::Operation::CreateFolder(path) => explorer.reveal(path),
                // After the forget above, so the source's own listings and
                // expansion land on a name with nothing left under it.
                crate::files::Operation::Rename { from, to, .. } => {
                    explorer.rekey_under(from, to);
                    explorer.reveal(to);
                }
                // Nothing to select: it is gone, and the clamp inside `fill`
                // puts the selection on a row that still exists.
                crate::files::Operation::Delete { .. }
                | crate::files::Operation::DeleteIfEmpty { .. } => {}
            }
        }
        // A file that was deleted or renamed away is no longer where the tree
        // has its selection; re-reading the directory is what fixes that, and
        // the clamp inside `fill` keeps the selection on a row that exists.
        self.status = Some(match detached_tabs {
            0 => operation.describe(),
            1 => format!(
                "{} — one open document no longer lives anywhere on disk; save \
                 it somewhere to keep it",
                operation.describe()
            ),
            n => format!(
                "{} — {n} open documents no longer live anywhere on disk; save \
                 them somewhere to keep them",
                operation.describe()
            ),
        });
        self.refresh_context();
    }

    /// Tells the session where the workspace is, creating the tree.
    ///
    /// The frontend works the root out — it needs a working directory and the
    /// path deco was started with — and hands it over. Called again for a
    /// different root, the tree starts over rather than merging: expansion state
    /// from one workspace means nothing in another.
    pub fn set_workspace_root(&mut self, root: impl Into<std::path::PathBuf>) {
        self.explorer = Some(crate::Explorer::new(root));
        // The stack holds absolute paths in the workspace being left. Undoing
        // one while looking at another would move or delete a file outside what
        // is on screen, which is the worst kind of surprise this can produce.
        self.explorer_undo.clear();
        self.refresh_context();
    }

    /// The workspace tree, if a root has been set.
    pub fn explorer(&self) -> Option<&crate::Explorer> {
        self.explorer.as_ref()
    }

    /// A directory the tree needs read, if any.
    ///
    /// The frontend asks after anything that could have expanded something, and
    /// keeps asking until it answers `None` — one listing per turn, so a deep
    /// reveal arrives a level at a time rather than in one blocking walk.
    pub fn directory_wanted(&self) -> Option<std::path::PathBuf> {
        self.explorer.as_ref().and_then(crate::Explorer::wanted)
    }

    /// Whether `git.enabled` leaves the feature on.
    ///
    /// VS Code's setting, with VS Code's default of `true`. Read here rather
    /// than in a frontend so that the two cannot disagree about it, and so
    /// that turning git off is one answer rather than one per frontend.
    pub fn git_enabled(&self) -> bool {
        self.settings.get_bool("git.enabled", None).unwrap_or(true)
    }

    /// Whether a fresh `git status` would be worth running.
    ///
    /// Always `false` when `git.enabled` is off: a setting that turns the
    /// feature off has to stop the process from being spawned, not just hide
    /// what it found.
    pub fn scm_wanted(&self) -> bool {
        self.scm_wanted && self.git_enabled()
    }

    /// What `git status` last said, if anything.
    ///
    /// Nothing at all once `git.enabled` is off, whatever was found before it
    /// was: a setting that turns the feature off has to take what is on screen
    /// with it, not only stop the next run.
    pub fn scm_status(&self) -> Option<&deco_scm::Status> {
        self.git_enabled().then_some(self.scm.as_ref()).flatten()
    }

    /// Says a run has begun, so the question does not need asking again.
    ///
    /// Separate from [`Session::fill_scm`] and called *first*, which is what
    /// makes a change during a run survive it: something saved while git is
    /// still thinking sets the flag again, the answer arrives and is stored
    /// without clearing it, and the next poll starts a fresh run. Clearing on
    /// the answer instead would drop that save silently, and the status bar
    /// would sit there being wrong until the one after it.
    pub fn scm_started(&mut self) {
        self.scm_wanted = false;
    }

    /// Hands over what `git status` said.
    ///
    /// `None` is a real answer, not "keep what you had": there is no git, or
    /// this is not a repository, or git refused. Keeping a stale branch name on
    /// screen after the repository went away would be worse than showing
    /// nothing.
    pub fn fill_scm(&mut self, status: Option<deco_scm::Status>) {
        self.scm = status;
    }

    /// Says the status is stale.
    ///
    /// Called for the things git would report differently: a save, a file
    /// created, renamed or deleted, and coming back to a window that may have
    /// been left while a commit happened in a terminal.
    pub fn scm_changed(&mut self) {
        self.scm_wanted = true;
    }

    /// Hands the tree what a directory contains.
    pub fn fill_directory(&mut self, dir: &std::path::Path, entries: Vec<crate::explorer::Entry>) {
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.fill(dir, entries);
        }
        self.refresh_context();
    }

    /// `revealInExplorer`: opens the tree onto the file being edited.
    ///
    /// An untitled document has no path to reveal, and saying so is better than
    /// a key that does nothing: the tree is showing, it just has nothing to
    /// point at yet.
    fn reveal_active_file(&mut self) -> Outcome {
        let Some(path) = self.document.path.clone() else {
            return Outcome::Message("this document has not been saved anywhere yet".to_owned());
        };
        if self.explorer.is_none() {
            return Outcome::Message("no workspace to reveal it in".to_owned());
        }
        if !self.side_bar {
            self.show_side_bar(true);
        }
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.reveal(&path);
        }
        self.refresh_context();
        Outcome::Handled
    }

    /// One of the tree's navigation keys.
    ///
    /// Does nothing unless the tree has the keyboard. The default keymap gates
    /// these on `sideBarFocus` so it never comes up there, but a hand-written
    /// binding without a `when` clause is allowed to exist, and moving a
    /// selection nobody can see while someone types in the editor is worse than
    /// a key that does nothing.
    fn explorer_key(&mut self, command: &str) -> Outcome {
        if self.focus != Focus::SideBar {
            return Outcome::Handled;
        }
        let Some(explorer) = self.explorer.as_mut() else {
            return Outcome::Handled;
        };
        match command {
            "list.focusDown" => explorer.select_next(),
            "list.focusUp" => explorer.select_previous(),
            "list.focusFirst" => explorer.select_first(),
            "list.focusLast" => explorer.select_last(),
            "list.expand" => explorer.expand(),
            "list.collapse" => explorer.collapse(),
            "list.select" => {
                // Enter opens a file and toggles a directory, which is what the
                // explorer does in VS Code and the only reading that makes the
                // same key useful on every row.
                let Some(row) = explorer.selection() else {
                    return Outcome::Handled;
                };
                if row.is_dir {
                    explorer.toggle();
                } else {
                    // The keyboard follows the file into the editor. Opening
                    // something and leaving the caret in the tree would mean a
                    // second keystroke before you could type in what you just
                    // asked for.
                    self.focus = Focus::Editor;
                    self.refresh_context();
                    return Outcome::OpenFile {
                        path: row.path,
                        at: None,
                    };
                }
            }
            _ => {}
        }
        // The side bar's height, so the tree can keep the selection on screen —
        // the model does not know how tall it is drawn.
        let height = self
            .regions()
            .side_bar
            .map(|rect| rect.height.saturating_sub(EXPLORER_CHROME_ROWS))
            .unwrap_or(0);
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.scroll_into_view(height);
        }
        self.refresh_context();
        Outcome::Handled
    }

    /// Moves the keyboard to a region, showing it first if it is hidden.
    ///
    /// Focusing something invisible is the one thing this must not do. Toggling
    /// deliberately does *not* focus — VS Code's `ctrl+b` leaves the caret in
    /// the text, and moving it would make showing the tree cost your place.
    fn focus_region(&mut self, focus: Focus) -> Outcome {
        match focus {
            Focus::SideBar if !self.side_bar => {
                self.show_side_bar(true);
            }
            Focus::Panel if !self.panel => {
                self.show_panel(true);
            }
            _ => {}
        }

        // A region that does not fit in this window cannot take the keyboard
        // either, however it was asked for.
        let regions = self.regions();
        let showing = match focus {
            Focus::Editor => true,
            Focus::SideBar => regions.side_bar.is_some(),
            Focus::Panel => regions.panel.is_some(),
        };
        if !showing {
            return Outcome::Message("no room for it in this window".to_owned());
        }

        self.focus = focus;
        self.refresh_context();
        Outcome::Handled
    }

    /// How the window is currently divided between editor, side bar and panel.
    ///
    /// Recomputed rather than stored: it is a pure function of the window size
    /// and two booleans, and a cached copy is one more thing that can be stale
    /// while the screen says otherwise.
    pub fn regions(&self) -> crate::layout::Regions {
        self.regions_for(self.screen.0, self.screen.1)
    }

    /// The same division, of a rectangle the caller names.
    ///
    /// For a renderer, which knows the area it is drawing into and should not
    /// have to assume it is the one the session was last resized to. The two
    /// agree in the editor — the frontend computes one from the other — but a
    /// renderer that took it on trust would be relying on that rather than on
    /// what is in front of it.
    pub fn regions_for(&self, width: usize, height: usize) -> crate::layout::Regions {
        crate::layout::regions(
            width,
            height,
            self.side_bar
                .then(|| deco_config::SideBarLocation::resolve(&self.settings)),
            self.panel,
        )
    }

    /// Which region has the keyboard.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Gives every group its size and the columns it leaves for text, without
    /// moving any window.
    fn lay_out(&mut self, width: usize, height: usize) {
        self.screen = (width, height);
        // The regions come off first: what is left is what the editor has, and
        // it is that rectangle the groups divide and the text wraps inside.
        let editor = self.regions().editor;
        let (width, height) = (editor.width, editor.height);

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

    /// A rename-shaped workspace edit: one replacement per named file.
    ///
    /// Each `(uri, line, from_len, to)` replaces `from_len` characters at the
    /// start of `line`, which is enough shape to tell whether the right text in
    /// the right file changed.
    fn workspace_edit(documents: &[(&str, u32, u32, &str)]) -> deco_lsp::WorkspaceEdit {
        let mut changes: Vec<deco_lsp::DocumentEdits> = Vec::new();
        for (uri, line, from_len, to) in documents {
            let edit = deco_lsp::TextEdit {
                range: deco_core::Range::new(
                    Position::new(*line, 0),
                    Position::new(*line, *from_len),
                ),
                new_text: (*to).to_owned(),
            };
            match changes.iter_mut().find(|d| d.uri.as_str() == *uri) {
                Some(seen) => seen.edits.push(edit),
                None => changes.push(deco_lsp::DocumentEdits {
                    uri: deco_lsp::uri::Uri::from_string(*uri),
                    version: None,
                    edits: vec![edit],
                }),
            }
        }
        deco_lsp::WorkspaceEdit { changes }
    }

    /// Plans and applies in one go, reading missing files from `on_disk`.
    fn apply_workspace(
        session: &mut Session,
        edit: &deco_lsp::WorkspaceEdit,
        on_disk: &[(&str, &str)],
    ) -> Result<crate::workspace::Applied, crate::workspace::WorkspaceError> {
        let plan = session
            .plan_workspace_edit(
                edit,
                |uri| uri.to_path(deco_lsp::uri::PathStyle::Unix).ok(),
                |_| None,
            )?
            .with_contents(|path| {
                on_disk
                    .iter()
                    .find(|(name, _)| Path::new(name) == path)
                    .map(|(_, text)| (*text).to_owned())
                    .ok_or_else(|| "no such file".to_owned())
            })?;
        session.apply_workspace_edit(plan, 0)
    }

    /// Answers both halves of a replace-in-files and returns what came out.
    fn replace_in_files(session: &mut Session, query: &str, replacement: &str) -> Outcome {
        session.run("workbench.action.replaceInFiles", None, 0);
        // The seed is selected, so typing replaces whatever was under the caret.
        for c in query.chars() {
            press(session, &c.to_string());
        }
        let first = session.run("workbench.action.acceptSelectedQuickOpenItem", None, 0);
        assert_eq!(first, Outcome::Handled, "the query is only the first half");
        assert_eq!(
            session.prompt.as_ref().map(|p| p.kind()),
            Some(crate::prompt::PromptKind::ReplaceQuery),
            "and the second prompt should be open"
        );
        for c in replacement.chars() {
            press(session, &c.to_string());
        }
        session.run("workbench.action.acceptSelectedQuickOpenItem", None, 0)
    }

    #[test]
    fn replacing_in_files_asks_what_and_then_what_with() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old\n");

        assert_eq!(
            replace_in_files(&mut s, "old", "new"),
            Outcome::ReplaceInFiles {
                query: "old".to_owned(),
                replacement: "new".to_owned(),
                options: Default::default(),
            }
        );
    }

    #[test]
    fn an_empty_replacement_deletes_and_is_not_refused() {
        // "take every occurrence of this out" is a thing people mean, and it
        // would be the one destructive-looking case that could not be done.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old\n");

        assert_eq!(
            replace_in_files(&mut s, "old", ""),
            Outcome::ReplaceInFiles {
                query: "old".to_owned(),
                replacement: String::new(),
                options: Default::default(),
            }
        );
    }

    #[test]
    fn find_in_files_still_searches_rather_than_replacing() {
        // The two commands open the same prompt, so the one that was pressed has
        // to survive until the prompt is accepted.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old\n");
        s.run("workbench.action.findInFiles", None, 0);
        for c in "old".chars() {
            press(&mut s, &c.to_string());
        }

        assert!(matches!(
            s.run("workbench.action.acceptSelectedQuickOpenItem", None, 0),
            Outcome::SearchInFiles { .. }
        ));
    }

    #[test]
    fn a_replace_left_half_finished_does_not_leak_into_the_next_search() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old\n");
        s.run("workbench.action.replaceInFiles", None, 0);
        s.run("workbench.action.closeQuickOpen", None, 0);

        // The next plain search must not turn into the replace that was abandoned.
        s.run("workbench.action.findInFiles", None, 0);
        for c in "old".chars() {
            press(&mut s, &c.to_string());
        }
        assert!(matches!(
            s.run("workbench.action.acceptSelectedQuickOpenItem", None, 0),
            Outcome::SearchInFiles { .. }
        ));
    }

    #[test]
    fn a_replacement_is_planned_against_the_buffer_not_the_file() {
        // The search read the file from disk; this tab has since changed. The
        // buffer is what a replace has to act on, or it would edit positions in
        // a document that no longer exists — and then save over the real one.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old old\n");
        press(&mut s, "x");

        let plan = s
            .plan_replacements(
                &[PathBuf::from("/w/a.rs")],
                "old",
                "new",
                Default::default(),
                |path| panic!("read {} when a tab holds it", path.display()),
            )
            .expect("the tab supplies the text");

        assert_eq!(plan.documents(), 1);
        assert_eq!(plan.edits(), 2, "both occurrences, found in the buffer");
    }

    #[test]
    fn a_file_no_tab_holds_is_planned_from_what_the_caller_read() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "nothing here\n");

        let plan = s
            .plan_replacements(
                &[PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")],
                "old",
                "new",
                Default::default(),
                |_| Ok("old and old again\n".to_owned()),
            )
            .expect("b.rs was supplied");

        // a.rs has no matches and so is not opened, changed or listed.
        assert_eq!(plan.documents(), 1);
        assert_eq!(plan.edits(), 2);
        assert_eq!(
            plan.missing().collect::<Vec<_>>(),
            [Path::new("/w/b.rs")],
            "the one file that has to be opened"
        );
    }

    #[test]
    fn a_replacement_reaches_every_file_as_one_undoable_step() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old\n");
        s.open(PathBuf::from("/w/b.rs"), "old and old\n");

        let plan = s
            .plan_replacements(
                &[PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")],
                "old",
                "new",
                Default::default(),
                |_| unreachable!("both are open"),
            )
            .expect("both are open");
        let applied = s.apply_workspace_edit(plan, 0).expect("nothing overlaps");

        assert_eq!(applied.documents, 2);
        assert_eq!(applied.edits, 3);
        assert_eq!(s.document.buffer.text(), "new and new\n");

        s.run("undo", None, 0);
        assert_eq!(s.document.buffer.text(), "old and old\n");
        assert_eq!(
            s.document_at_path(Path::new("/w/a.rs"))
                .unwrap()
                .buffer
                .text(),
            "old\n",
            "the other file came back in the same step"
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_refuses_the_whole_replacement() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old\n");

        let error = s
            .plan_replacements(
                &[PathBuf::from("/w/a.rs"), PathBuf::from("/w/gone.rs")],
                "old",
                "new",
                Default::default(),
                |_| Err("no such file".to_owned()),
            )
            .expect_err("one of the files is not there");

        assert!(matches!(
            error,
            crate::workspace::WorkspaceError::Unreadable { ref path, .. }
                if path == Path::new("/w/gone.rs")
        ));
        assert_eq!(s.document.buffer.text(), "old\n", "and nothing was changed");
    }

    #[test]
    fn the_rename_prompt_opens_with_the_current_name_in_it() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "fn greet() {}\n");
        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 5));

        assert_eq!(s.offer_rename(), Outcome::Handled);
        let prompt = s.prompt.as_ref().expect("a prompt");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::Rename);
        assert_eq!(prompt.text(), "greet");
    }

    #[test]
    fn accepting_the_rename_prompt_unchanged_asks_for_nothing() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "fn greet() {}\n");
        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 5));
        s.offer_rename();

        let outcome = s.run("workbench.action.acceptSelectedQuickOpenItem", None, 0);
        assert_eq!(
            outcome,
            Outcome::Message("`greet` is already its name".to_owned())
        );
    }

    #[test]
    fn a_new_name_asks_the_frontend_to_carry_it_out() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "fn greet() {}\n");
        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 5));
        s.offer_rename();
        for key in ["h", "i"] {
            press(&mut s, key);
        }

        assert_eq!(
            s.run("workbench.action.acceptSelectedQuickOpenItem", None, 0),
            Outcome::Rename {
                new_name: "hi".to_owned()
            },
            "the seed is selected, so typing replaces it"
        );
    }

    #[test]
    fn rename_needs_something_under_the_cursor() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "   \n");
        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 1));

        assert_eq!(
            s.offer_rename(),
            Outcome::Message("put the cursor on a name to rename it".to_owned())
        );
        assert!(s.prompt.is_none());
    }

    #[test]
    fn a_workspace_edit_reaches_every_open_document() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old();\n");
        s.open(PathBuf::from("/w/b.rs"), "old();\n");

        let applied = apply_workspace(
            &mut s,
            &workspace_edit(&[
                ("file:///w/a.rs", 0, 3, "new"),
                ("file:///w/b.rs", 0, 3, "new"),
            ]),
            &[],
        )
        .expect("both are open");

        assert_eq!(applied.documents, 2);
        assert_eq!(applied.edits, 2);
        assert_eq!(applied.opened, 0, "nothing had to be opened");
        assert_eq!(s.document.buffer.text(), "new();\n");
        assert_eq!(
            s.document_at_path(Path::new("/w/a.rs"))
                .unwrap()
                .buffer
                .text(),
            "new();\n",
            "the background tab too"
        );
    }

    #[test]
    fn a_file_no_tab_holds_is_opened_rather_than_written() {
        // Unsaved, so that nothing reaches the disk without the user saying so,
        // and visible, so that they know it is there to save.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old();\n");
        let tabs_before = s.tab_count();

        let applied = apply_workspace(
            &mut s,
            &workspace_edit(&[
                ("file:///w/a.rs", 0, 3, "new"),
                ("file:///w/far.rs", 0, 3, "new"),
            ]),
            &[("/w/far.rs", "old();\n")],
        )
        .expect("the missing file was supplied");

        assert_eq!(applied.opened, 1);
        assert_eq!(s.tab_count(), tabs_before + 1);
        let opened = s
            .document_at_path(Path::new("/w/far.rs"))
            .expect("opened as a tab");
        assert_eq!(opened.buffer.text(), "new();\n");
        assert!(opened.dirty, "unsaved, so ctrl+k s is what writes it");
    }

    #[test]
    fn a_file_that_cannot_be_read_changes_nothing_at_all() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old();\n");
        let tabs_before = s.tab_count();

        let error = apply_workspace(
            &mut s,
            &workspace_edit(&[
                ("file:///w/a.rs", 0, 3, "new"),
                ("file:///w/gone.rs", 0, 3, "new"),
            ]),
            &[],
        )
        .expect_err("one of the files is not there");

        assert!(matches!(
            error,
            crate::workspace::WorkspaceError::Unreadable { .. }
        ));
        assert_eq!(
            s.document.buffer.text(),
            "old();\n",
            "the half that could have been applied was not"
        );
        assert_eq!(s.tab_count(), tabs_before, "and no tab was left behind");
        assert!(!s.document.dirty);
    }

    #[test]
    fn overlapping_edits_change_nothing_at_all() {
        // The refusal has to happen before the *other* document is written, which
        // is the whole reason the transactions are built up front.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old();\n");
        s.open(PathBuf::from("/w/b.rs"), "old();\n");

        let mut edit = workspace_edit(&[("file:///w/a.rs", 0, 3, "new")]);
        edit.changes.push(deco_lsp::DocumentEdits {
            uri: deco_lsp::uri::Uri::from_string("file:///w/b.rs"),
            version: None,
            edits: vec![
                deco_lsp::TextEdit {
                    range: deco_core::Range::new(Position::new(0, 0), Position::new(0, 3)),
                    new_text: "one".to_owned(),
                },
                deco_lsp::TextEdit {
                    range: deco_core::Range::new(Position::new(0, 1), Position::new(0, 4)),
                    new_text: "two".to_owned(),
                },
            ],
        });

        let error = apply_workspace(&mut s, &edit, &[]).expect_err("b's edits overlap");
        assert!(matches!(
            error,
            crate::workspace::WorkspaceError::Overlapping { ref path } if path == Path::new("/w/b.rs")
        ));
        assert_eq!(
            s.document_at_path(Path::new("/w/a.rs"))
                .unwrap()
                .buffer
                .text(),
            "old();\n",
            "the file whose edits were fine is untouched"
        );
    }

    #[test]
    fn one_undo_takes_the_whole_edit_back() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old();\n");
        s.open(PathBuf::from("/w/b.rs"), "old();\n");
        apply_workspace(
            &mut s,
            &workspace_edit(&[
                ("file:///w/a.rs", 0, 3, "new"),
                ("file:///w/b.rs", 0, 3, "new"),
            ]),
            &[],
        )
        .expect("both are open");

        s.run("undo", None, 0);

        assert_eq!(s.document.buffer.text(), "old();\n");
        assert_eq!(
            s.document_at_path(Path::new("/w/a.rs"))
                .unwrap()
                .buffer
                .text(),
            "old();\n",
            "the file that was not on screen came back too"
        );
    }

    #[test]
    fn redo_puts_the_whole_edit_back() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old();\n");
        s.open(PathBuf::from("/w/b.rs"), "old();\n");
        apply_workspace(
            &mut s,
            &workspace_edit(&[
                ("file:///w/a.rs", 0, 3, "new"),
                ("file:///w/b.rs", 0, 3, "new"),
            ]),
            &[],
        )
        .expect("both are open");
        s.run("undo", None, 0);
        s.run("redo", None, 0);

        assert_eq!(s.document.buffer.text(), "new();\n");
        assert_eq!(
            s.document_at_path(Path::new("/w/a.rs"))
                .unwrap()
                .buffer
                .text(),
            "new();\n"
        );
    }

    #[test]
    fn typing_after_a_workspace_edit_undoes_on_its_own() {
        // The keystroke is this document's business. Only once it is undone is
        // the shared step next, and only then does undo reach the other files.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "old();\n");
        s.open(PathBuf::from("/w/b.rs"), "old();\n");
        apply_workspace(
            &mut s,
            &workspace_edit(&[
                ("file:///w/a.rs", 0, 3, "new"),
                ("file:///w/b.rs", 0, 3, "new"),
            ]),
            &[],
        )
        .expect("both are open");

        press(&mut s, "x");
        s.run("undo", None, 0);
        assert_eq!(
            s.document.buffer.text(),
            "new();\n",
            "the keystroke came out"
        );
        assert_eq!(
            s.document_at_path(Path::new("/w/a.rs"))
                .unwrap()
                .buffer
                .text(),
            "new();\n",
            "and the other file was left alone"
        );

        s.run("undo", None, 0);
        assert_eq!(
            s.document_at_path(Path::new("/w/a.rs"))
                .unwrap()
                .buffer
                .text(),
            "old();\n",
            "the shared step was next, and reached both"
        );
    }

    #[test]
    fn an_ordinary_undo_is_still_one_document() {
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "");
        s.open(PathBuf::from("/w/b.rs"), "");
        press(&mut s, "x");
        assert_eq!(
            s.run("undo", None, 0),
            Outcome::Handled,
            "not a group report"
        );
        assert_eq!(s.document.buffer.text(), "");
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
    fn ctrl_h_opens_the_bar_focused_on_the_query_when_there_is_none_yet() {
        // Nothing selected and nothing searched for, so there is nothing to
        // replace and the first thing typed is the query.
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+h");
        assert!(s.find.visible());
        assert!(s.find.replacing(), "the replacement row is still shown");
        assert_eq!(s.find.field(), crate::find::Field::Query);
        assert_eq!(s.context.get("findInputFocussed"), Some(&json!(true)));
        assert_eq!(s.context.get("replaceInputFocussed"), Some(&json!(false)));
    }

    #[test]
    fn ctrl_h_opens_the_bar_with_the_replacement_focused_once_there_is_a_query() {
        let mut s = searchable("foo\n");
        s.view.selections = deco_core::selection::SelectionSet::single(
            deco_core::selection::Selection::new(Position::new(0, 0), Position::new(0, 3)),
        );
        press(&mut s, "ctrl+h");
        assert_eq!(s.find.query(), "foo");
        assert_eq!(s.find.field(), crate::find::Field::Replace);
        assert_eq!(s.context.get("replaceInputFocussed"), Some(&json!(true)));
        assert_eq!(s.context.get("findInputFocussed"), Some(&json!(false)));
    }

    #[test]
    fn ctrl_h_focuses_the_replacement_when_a_query_was_typed_earlier() {
        // The query survives `close`, so a second `ctrl+h` still has something to
        // replace even though nothing is selected.
        let mut s = searchable("foo\n");
        press(&mut s, "ctrl+f");
        press_all(&mut s, &["f", "o", "o"]);
        press(&mut s, "escape");
        press(&mut s, "ctrl+h");
        assert_eq!(s.find.field(), crate::find::Field::Replace);
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
        press_all(&mut s, &["f", "o", "o"]);
        press(&mut s, "tab");
        press_all(&mut s, &["b", "a", "r"]);
        assert_eq!(s.find.replace(), "bar");
        assert_eq!(s.find.query(), "foo", "the query is untouched");
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
        press_all(&mut s, &["f", "o", "o"]);
        press(&mut s, "tab");
        press_all(&mut s, &["b", "a", "r"]);
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
        press_all(&mut s, &["f", "o", "o"]);
        press(&mut s, "tab");
        press_all(&mut s, &["b", "a", "r"]);
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
        press_all(&mut s, &["f", "o", "o"]);
        press(&mut s, "tab");
        press_all(&mut s, &["b", "a", "z"]);
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
        press_all(&mut s, &["f", "o", "o"]);
        press(&mut s, "tab");
        press_all(&mut s, &["b", "a", "r"]);
        press(&mut s, "shift+tab");
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
        press(&mut s, "tab");
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
    fn ctrl_b_and_ctrl_j_toggle_the_chrome() {
        let mut s = session();
        s.resize(80, 24);
        assert!(s.regions().side_bar.is_none(), "hidden to start with");

        press(&mut s, "ctrl+b");
        assert!(s.regions().side_bar.is_some());
        assert_eq!(s.context.get("sideBarVisible"), Some(&json!(true)));

        press(&mut s, "ctrl+j");
        assert!(s.regions().panel.is_some());
        assert_eq!(s.context.get("panelVisible"), Some(&json!(true)));

        press(&mut s, "ctrl+b");
        press(&mut s, "ctrl+j");
        assert!(s.regions().side_bar.is_none());
        assert!(s.regions().panel.is_none());
    }

    #[test]
    fn showing_a_region_gives_the_text_less_room_to_wrap_in() {
        // The whole reason the split lives in the session: the wrap width has to
        // move with it, or a line breaks where the renderer is not drawing.
        let mut s = session();
        s.open(PathBuf::from("/w/a.txt"), "x\n");
        s.resize(80, 24);
        let before = s.view.text_width;

        press(&mut s, "ctrl+b");
        assert!(
            s.view.text_width < before,
            "the side bar took columns from the text: {before} -> {}",
            s.view.text_width
        );
        assert_eq!(s.view.width, s.regions().editor.width);

        press(&mut s, "ctrl+j");
        assert_eq!(s.view.height, s.regions().editor.height);
    }

    // ---- The file tree ----------------------------------------------------

    /// A session with a workspace whose root has been listed.
    fn with_tree() -> Session {
        let mut s = session();
        s.resize(100, 30);
        s.set_workspace_root("/w");
        assert_eq!(s.directory_wanted().as_deref(), Some(Path::new("/w")));
        s.fill_directory(
            Path::new("/w"),
            vec![
                crate::explorer::Entry::dir("src"),
                crate::explorer::Entry::file("Cargo.toml"),
            ],
        );
        s
    }

    #[test]
    fn the_trees_keys_do_nothing_until_it_has_the_keyboard() {
        let mut s = with_tree();
        // Bound to `down`, but the caret is in the text.
        assert_eq!(s.run("list.focusDown", None, 0), Outcome::Handled);
        assert_eq!(
            s.explorer().unwrap().selection().unwrap().name,
            "src",
            "the selection did not move while the editor had focus"
        );

        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert_eq!(s.focus(), Focus::SideBar);
        assert_eq!(s.context.get("filesExplorerFocus"), Some(&json!(true)));
        s.run("list.focusDown", None, 0);
        assert_eq!(
            s.explorer().unwrap().selection().unwrap().name,
            "Cargo.toml"
        );
    }

    #[test]
    fn enter_on_a_file_opens_it_and_takes_the_keyboard_with_it() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0);
        assert_eq!(
            s.run("list.select", None, 0),
            Outcome::OpenFile {
                path: PathBuf::from("/w/Cargo.toml"),
                at: None,
            }
        );
        assert_eq!(
            s.focus(),
            Focus::Editor,
            "opening a file puts the keyboard where the file is"
        );
    }

    #[test]
    fn enter_on_a_directory_opens_it_and_stays_put() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert_eq!(s.run("list.select", None, 0), Outcome::Handled);
        assert_eq!(s.focus(), Focus::SideBar);
        assert_eq!(
            s.directory_wanted().as_deref(),
            Some(Path::new("/w/src")),
            "opening it is what asks for its contents"
        );
    }

    #[test]
    fn revealing_an_unsaved_document_says_so_rather_than_doing_nothing() {
        let mut s = with_tree();
        assert!(s.document.path.is_none());
        assert!(matches!(
            s.run("revealInExplorer", None, 0),
            Outcome::Message(_)
        ));
    }

    #[test]
    fn revealing_shows_the_side_bar_it_needs() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/src/main.rs"), "fn main() {}\n");
        assert!(s.regions().side_bar.is_none(), "hidden to start with");

        assert_eq!(s.run("revealInExplorer", None, 0), Outcome::Handled);
        assert!(s.regions().side_bar.is_some());
        // The directories above it were opened, so its listing is now wanted.
        assert_eq!(s.directory_wanted().as_deref(), Some(Path::new("/w/src")));
        s.fill_directory(
            Path::new("/w/src"),
            vec![crate::explorer::Entry::file("main.rs")],
        );
        assert_eq!(
            s.explorer().unwrap().selection().map(|r| r.path),
            Some(PathBuf::from("/w/src/main.rs"))
        );
    }

    // ---- Changing the files themselves -------------------------------------

    #[test]
    fn a_new_file_lands_beside_the_selected_one() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0); // Cargo.toml, a file
        assert_eq!(
            s.create_in_tree("notes.md", false),
            Outcome::FileOperation(crate::files::Operation::CreateFile(PathBuf::from(
                "/w/notes.md"
            ))),
            "a file's sibling, not a child of it"
        );
    }

    #[test]
    fn a_new_file_inside_the_selected_directory() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        // `src`, a directory, is the first row.
        assert_eq!(
            s.create_in_tree("main.rs", false),
            Outcome::FileOperation(crate::files::Operation::CreateFile(PathBuf::from(
                "/w/src/main.rs"
            )))
        );
    }

    #[test]
    fn a_name_with_a_path_in_it_is_refused() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert!(matches!(
            s.create_in_tree("../../etc/passwd", false),
            Outcome::Message(_)
        ));
        assert!(
            !s.can_undo_file_operation(),
            "a refusal leaves nothing on the stack"
        );
    }

    #[test]
    fn creating_something_that_is_already_there_is_refused() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0);
        assert!(matches!(
            s.create_in_tree("Cargo.toml", false),
            Outcome::Message(_)
        ));
    }

    #[test]
    fn renaming_a_file_moves_the_tab_that_holds_it() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/Cargo.toml"), "[package]\n");
        // A second tab, so the rename has to find the right one.
        s.open(PathBuf::from("/w/other.rs"), "fn other() {}\n");

        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0);
        let outcome = s.rename_in_tree("Cargo.lock");
        let operation = match outcome {
            Outcome::FileOperation(operation) => operation,
            other => panic!("expected an operation, got {other:?}"),
        };

        s.file_operation_done(&operation);
        assert!(
            s.tab_of(Path::new("/w/Cargo.lock")).is_some(),
            "the tab followed the file"
        );
        assert!(s.tab_of(Path::new("/w/Cargo.toml")).is_none());
        assert!(
            s.tab_of(Path::new("/w/other.rs")).is_some(),
            "the other tab was left alone"
        );
    }

    #[test]
    fn renaming_a_file_keeps_its_unsaved_text() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/Cargo.toml"), "[package]\n");
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0);
        let Outcome::FileOperation(operation) = s.rename_in_tree("Cargo.lock") else {
            panic!("expected an operation");
        };
        s.file_operation_done(&operation);
        assert_eq!(s.document.buffer.text(), "[package]\n");
    }

    #[test]
    fn what_was_just_created_is_what_is_selected() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        // `src` is selected, so the new file goes inside it.
        let Outcome::FileOperation(operation) = s.create_in_tree("new.rs", false) else {
            panic!("expected an operation");
        };
        s.file_operation_done(&operation);

        // `src` was selected, so the file went inside it — and revealing it
        // opened `src`, whose listing is now what the tree is waiting for.
        assert_eq!(s.directory_wanted().as_deref(), Some(Path::new("/w/src")));
        s.fill_directory(
            Path::new("/w/src"),
            vec![crate::explorer::Entry::file("new.rs")],
        );
        assert_eq!(
            s.explorer().unwrap().selection().map(|r| r.path),
            Some(PathBuf::from("/w/src/new.rs")),
            "the next F2 or delete must act on what was just made, not on what \
             happened to be highlighted before"
        );
    }

    #[test]
    fn undoing_a_create_deletes_it_and_undoing_a_rename_puts_it_back() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(create) = s.create_in_tree("new.rs", false) else {
            panic!("expected an operation");
        };
        s.file_operation_done(&create);

        // Creating a file put the keyboard in it; the tree's undo needs the
        // tree, which is the trip back a person makes with `ctrl+shift+e`.
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert!(s.can_undo_file_operation());
        assert_eq!(
            s.run("undo", None, 0),
            Outcome::FileOperation(crate::files::Operation::DeleteIfEmpty {
                path: PathBuf::from("/w/src/new.rs"),
                directory: false,
                expect: None,
            }),
            "undoing a create removes what it made — and only if it is still \
             what was made, rather than taking whatever has been written since"
        );
    }

    #[test]
    fn undoing_a_rename_through_the_prompts_puts_the_name_back() {
        // The whole sequence as a person does it, prompts included — which is
        // what the demonstration does, and where a focus bug would show.
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0); // Cargo.toml

        assert_eq!(s.run("renameFile", None, 0), Outcome::Handled);
        for c in "cargo.lock".chars() {
            s.handle_chord(Chord::char(c), 0);
        }
        let accepted = s.handle_chord(Chord::parse("enter").unwrap(), 0);
        let Outcome::FileOperation(rename) = accepted else {
            panic!("accepting the prompt should rename, got {accepted:?}");
        };
        s.file_operation_done(&rename);

        assert_eq!(
            s.focus(),
            Focus::SideBar,
            "answering the tree's prompt leaves the keyboard in the tree"
        );
        let undone = s.run("undo", None, 0);
        assert_eq!(
            undone,
            Outcome::FileOperation(crate::files::Operation::Rename {
                from: PathBuf::from("/w/cargo.lock"),
                to: PathBuf::from("/w/Cargo.toml"),
                expect: None,
                directory: false,
            }),
            "ctrl+z in the tree puts the name back"
        );
    }

    #[test]
    fn creating_a_file_leaves_the_keyboard_where_the_new_file_is() {
        // The sequence the demonstration walks: create through the prompt, then
        // type. If the keyboard were still in the tree, the typing would be
        // swallowed and the demonstration would show an editor that ignores it.
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("explorer.newFile", None, 0);
        for c in "new.rs".chars() {
            s.handle_chord(Chord::char(c), 0);
        }
        let Outcome::FileOperation(created) = s.handle_chord(Chord::parse("enter").unwrap(), 0)
        else {
            panic!("accepting the prompt should create the file");
        };
        s.file_operation_done(&created);
        // What the frontend does with a created file.
        s.open(PathBuf::from("/w/src/new.rs"), "");

        assert_eq!(
            s.focus(),
            Focus::Editor,
            "a created file is opened, and the keyboard goes with it"
        );
        s.handle_chord(Chord::char('x'), 0);
        assert_eq!(
            s.document.buffer.text(),
            "x",
            "typing after creating a file goes into the file"
        );
    }

    #[test]
    fn the_trees_undo_works_after_the_document_has_been_typed_in() {
        // The demonstration's whole sequence. Typing into the new file leaves
        // the *document* with an undo history, and the tree's `ctrl+z` has to
        // keep meaning the tree's undo — the two stacks are told apart by focus,
        // not by which was used last.
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("explorer.newFile", None, 0);
        for c in "new.rs".chars() {
            s.handle_chord(Chord::char(c), 0);
        }
        let Outcome::FileOperation(created) = s.handle_chord(Chord::parse("enter").unwrap(), 0)
        else {
            panic!("expected a create");
        };
        s.file_operation_done(&created);
        // The frontend's half: re-read the directory that changed, which is
        // what lets the reveal land the selection on the new file.
        assert_eq!(s.directory_wanted().as_deref(), Some(Path::new("/w/src")));
        s.fill_directory(
            Path::new("/w/src"),
            vec![crate::explorer::Entry::file("new.rs")],
        );
        s.open(PathBuf::from("/w/src/new.rs"), "");
        for c in "hello".chars() {
            s.handle_chord(Chord::char(c), 0);
        }
        assert_eq!(s.document.buffer.text(), "hello");

        // Back to the tree and rename it.
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("renameFile", None, 0);
        for c in "other.rs".chars() {
            s.handle_chord(Chord::char(c), 0);
        }
        let Outcome::FileOperation(renamed) = s.handle_chord(Chord::parse("enter").unwrap(), 0)
        else {
            panic!("expected a rename");
        };
        s.file_operation_done(&renamed);
        s.fill_directory(
            Path::new("/w/src"),
            vec![crate::explorer::Entry::file("other.rs")],
        );

        assert_eq!(s.focus(), Focus::SideBar);
        assert_eq!(
            s.handle_chord(Chord::parse("ctrl+z").unwrap(), 0),
            Outcome::FileOperation(crate::files::Operation::Rename {
                from: PathBuf::from("/w/src/other.rs"),
                to: PathBuf::from("/w/src/new.rs"),
                expect: None,
                directory: false,
            }),
            "the tree's undo, not the document's"
        );
        assert_eq!(
            s.document.buffer.text(),
            "hello",
            "and the text was left alone"
        );
    }

    #[test]
    fn renaming_a_directory_moves_every_tab_inside_it() {
        let mut s = with_tree();
        s.fill_directory(
            Path::new("/w/src"),
            vec![
                crate::explorer::Entry::file("main.rs"),
                crate::explorer::Entry::file("lib.rs"),
            ],
        );
        s.open(PathBuf::from("/w/src/main.rs"), "fn main() {}\n");
        s.open(PathBuf::from("/w/src/lib.rs"), "pub fn lib() {}\n");
        s.open(PathBuf::from("/w/Cargo.toml"), "[package]\n");

        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        // `src` is the first row.
        let Outcome::FileOperation(renamed) = s.rename_in_tree("source") else {
            panic!("expected a rename");
        };
        s.file_operation_done(&renamed);

        assert!(
            s.tab_of(Path::new("/w/source/main.rs")).is_some(),
            "a tab inside a renamed directory follows it"
        );
        assert!(s.tab_of(Path::new("/w/source/lib.rs")).is_some());
        assert!(s.tab_of(Path::new("/w/src/main.rs")).is_none());
        assert!(
            s.tab_of(Path::new("/w/Cargo.toml")).is_some(),
            "and a tab outside it is left alone"
        );
    }

    #[test]
    fn a_renamed_background_tab_gets_its_new_language() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/notes.txt"), "hello\n");
        // Another tab on top, so the renamed one is in the background.
        s.open(PathBuf::from("/w/Cargo.toml"), "[package]\n");
        let index = s.tab_of(Path::new("/w/notes.txt")).expect("it is open");

        s.retarget_tab(index, PathBuf::from("/w/notes.md"));
        let document = s.document_at_index_mut(index).expect("still open");
        assert_eq!(
            document.language(),
            Some("markdown"),
            "a background tab re-resolves its language, or it stays plain text \
             for the rest of the session"
        );
    }

    #[test]
    fn a_delete_the_disk_refuses_keeps_the_earlier_undos() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(created) = s.create_in_tree("new.rs", false) else {
            panic!("expected a create");
        };
        s.file_operation_done(&created);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert!(s.can_undo_file_operation());

        // A delete that never happens must not cost the create its undo.
        let Outcome::FileOperation(delete) = s.delete_in_tree() else {
            panic!("expected a delete");
        };
        s.file_operation_failed(&delete, "permission denied");
        assert!(
            s.can_undo_file_operation(),
            "nothing was deleted, so the earlier undo is still good"
        );
    }

    #[test]
    fn changing_workspace_forgets_the_other_ones_undos() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(created) = s.create_in_tree("new.rs", false) else {
            panic!("expected a create");
        };
        s.file_operation_done(&created);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert!(s.can_undo_file_operation());

        s.set_workspace_root("/elsewhere");
        assert!(
            !s.can_undo_file_operation(),
            "an undo holding paths in the old workspace must not run in the new one"
        );
    }

    #[test]
    fn the_trees_undo_wins_over_a_waiting_workspace_edit() {
        // After a project-wide replace the document has a shared step waiting.
        // `ctrl+z` in the tree must still be the tree's undo.
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(created) = s.create_in_tree("new.rs", false) else {
            panic!("expected a create");
        };
        s.file_operation_done(&created);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);

        assert!(
            matches!(
                s.run("undo", None, 0),
                Outcome::FileOperation(crate::files::Operation::DeleteIfEmpty { .. })
            ),
            "the tree's undo, even with the document holding a shared step"
        );
    }

    #[test]
    fn the_trees_undo_walks_back_rather_than_toggling() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(first) = s.create_in_tree("one.rs", false) else {
            panic!("expected a create");
        };
        s.file_operation_done(&first);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(second) = s.create_in_tree("two.rs", false) else {
            panic!("expected a create");
        };
        s.file_operation_done(&second);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);

        // Two presses must undo two operations, not undo one and put it back.
        let Outcome::FileOperation(undone_second) = s.run("undo", None, 0) else {
            panic!("expected the second create to be undone");
        };
        s.file_operation_done(&undone_second);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(undone_first) = s.run("undo", None, 0) else {
            panic!("expected the first create to be undone too");
        };
        assert_ne!(
            undone_first, undone_second,
            "pressing undo twice must reach the older entry, not toggle the newer"
        );
        assert!(!s.can_undo_file_operation(), "and then there are none left");
    }

    #[test]
    fn an_undo_the_disk_refuses_can_be_tried_again() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(created) = s.create_in_tree("new.rs", false) else {
            panic!("expected a create");
        };
        s.file_operation_done(&created);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);

        let Outcome::FileOperation(undo) = s.run("undo", None, 0) else {
            panic!("expected an undo");
        };
        s.file_operation_failed(&undo, "it has been written to since");
        assert!(
            s.can_undo_file_operation(),
            "an undo that did not happen is still there to try again"
        );
    }

    #[test]
    fn a_create_the_disk_refuses_leaves_the_keyboard_in_the_tree() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(operation) = s.create_in_tree("new.rs", false) else {
            panic!("expected a create");
        };
        s.file_operation_failed(&operation, "permission denied");
        assert_eq!(
            s.focus(),
            Focus::SideBar,
            "nothing was created, so there is nothing to have moved into"
        );
    }

    #[test]
    fn deleting_an_open_file_stops_its_tab_writing_it_back() {
        let mut s = with_tree();
        s.fill_directory(
            Path::new("/w/src"),
            vec![crate::explorer::Entry::file("main.rs")],
        );
        s.open(PathBuf::from("/w/src/main.rs"), "fn main() {}\n");
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.expand", None, 0);
        s.run("list.focusDown", None, 0);
        assert_eq!(s.explorer().unwrap().selection().unwrap().name, "main.rs");

        let Outcome::FileOperation(deleted) = s.delete_in_tree() else {
            panic!("expected a delete");
        };
        s.file_operation_done(&deleted);

        assert!(
            s.tab_of(Path::new("/w/src/main.rs")).is_none(),
            "no tab may still point at a file that has been deleted — saving it \
             would put the file back"
        );
    }

    #[test]
    fn renaming_across_languages_changes_the_lexer_too() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/notes.txt"), "# hello\n");
        s.open(PathBuf::from("/w/Cargo.toml"), "[package]\n");
        let index = s.tab_of(Path::new("/w/notes.txt")).expect("it is open");

        s.retarget_tab(index, PathBuf::from("/w/notes.md"));
        let document = s.document_at_index_mut(index).expect("still open");
        assert_eq!(document.language(), Some("markdown"));
        assert_eq!(
            document.syntax.source_scope(),
            deco_syntax::Syntax::new(Some("markdown")).source_scope(),
            "the lexer follows the language, or the file keeps being highlighted \
             as whatever it used to be"
        );
    }

    #[test]
    fn a_delete_carries_the_type_the_tree_was_showing() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        // `src`, a directory.
        let Outcome::FileOperation(operation) = s.delete_in_tree() else {
            panic!("expected a delete");
        };
        assert_eq!(
            operation,
            crate::files::Operation::Delete {
                path: PathBuf::from("/w/src"),
                directory: true,
            }
        );

        s.run("list.focusDown", None, 0); // Cargo.toml, a file
        let Outcome::FileOperation(operation) = s.delete_in_tree() else {
            panic!("expected a delete");
        };
        assert_eq!(
            operation,
            crate::files::Operation::Delete {
                path: PathBuf::from("/w/Cargo.toml"),
                directory: false,
            },
            "a file is deleted as a file, whatever the disk says by the time the \
             frontend gets there"
        );
    }

    #[test]
    fn detaching_a_deleted_tab_drops_its_diagnostics_and_tells_the_server() {
        let mut s = with_tree();
        s.fill_directory(
            Path::new("/w/src"),
            vec![crate::explorer::Entry::file("main.rs")],
        );
        s.open(PathBuf::from("/w/src/main.rs"), "fn main() {}\n");
        s.diagnostics = vec![deco_lsp::Diagnostic {
            range: deco_core::position::Range::new(
                deco_core::position::Position::new(0, 0),
                deco_core::position::Position::new(0, 2),
            ),
            severity: deco_lsp::diagnostics::Severity::Error,
            message: "something".to_owned(),
            source: None,
            code: None,
        }];

        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.expand", None, 0);
        s.run("list.focusDown", None, 0);
        let Outcome::FileOperation(deleted) = s.delete_in_tree() else {
            panic!("expected a delete");
        };
        s.file_operation_done(&deleted);

        assert!(
            s.diagnostics.is_empty(),
            "squiggles describing a file that is gone must not outlive it"
        );
        assert_eq!(
            s.take_closed_documents(),
            vec![PathBuf::from("/w/src/main.rs")],
            "the server is holding it open under a URI that names nothing"
        );
        assert!(
            s.take_closed_documents().is_empty(),
            "and taking them is what forgets them"
        );
    }

    #[test]
    fn renaming_away_from_a_language_drops_what_the_server_said() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/main.rs"), "fn main() {}\n");
        s.semantic_tokens = vec![deco_lsp::requests::SemanticSpan {
            range: deco_core::position::Range::new(
                deco_core::position::Position::new(0, 0),
                deco_core::position::Position::new(0, 2),
            ),
            token_type: "keyword".to_owned(),
            modifiers: Vec::new(),
        }];
        let index = s.tab_of(Path::new("/w/main.rs")).expect("it is open");

        // `.txt` has no language, so no server will ever correct these.
        s.retarget_tab(index, PathBuf::from("/w/main.txt"));
        assert!(
            s.semantic_tokens.is_empty(),
            "tokens from the old language must not outlive it — nothing would \
             ever replace them"
        );
        assert_eq!(
            s.take_closed_documents(),
            vec![PathBuf::from("/w/main.rs")],
            "and the server is told the file it knew is closed"
        );
    }

    #[test]
    fn tabs_can_be_let_go_one_file_at_a_time() {
        // What a half-finished recursive delete needs: some of a directory's
        // files are gone and the rest are not, and only the disk knows which.
        let mut s = with_tree();
        s.fill_directory(
            Path::new("/w/src"),
            vec![
                crate::explorer::Entry::file("gone.rs"),
                crate::explorer::Entry::file("kept.rs"),
            ],
        );
        s.open(PathBuf::from("/w/src/gone.rs"), "fn gone() {}\n");
        s.open(PathBuf::from("/w/src/kept.rs"), "fn kept() {}\n");

        let under = s.open_paths_under(Path::new("/w/src"));
        assert_eq!(under.len(), 2, "both tabs are under it");

        assert_eq!(s.detach_tabs_under(Path::new("/w/src/gone.rs")), 1);
        assert!(s.tab_of(Path::new("/w/src/gone.rs")).is_none());
        assert!(
            s.tab_of(Path::new("/w/src/kept.rs")).is_some(),
            "the file that survived keeps its tab"
        );
    }

    #[test]
    fn a_half_finished_delete_puts_the_same_barrier_up() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(created) = s.create_in_tree("new.rs", false) else {
            panic!("expected a create");
        };
        s.file_operation_done(&created);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert!(s.can_undo_file_operation());

        // A recursive delete that removed part of a tree and then stopped:
        // something irreversible went, so the inverses below it describe a
        // state that never existed.
        s.clear_file_undo();
        assert!(!s.can_undo_file_operation());
    }

    #[test]
    fn accepting_the_rename_prompt_unchanged_changes_nothing() {
        let mut s = with_tree();
        // A name a filesystem may allow and `check_name` would trim.
        s.fill_directory(
            Path::new("/w"),
            vec![crate::explorer::Entry::file(" report ")],
        );
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert_eq!(s.explorer().unwrap().selection().unwrap().name, " report ");

        assert_eq!(
            s.rename_in_tree(" report "),
            Outcome::Handled,
            "pressing enter on the seeded prompt must not rename the file to a \
             trimmed version of its own name"
        );
    }

    #[test]
    fn a_deleted_directorys_rows_do_not_come_back_with_its_name() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.expand", None, 0); // open `src`
        s.fill_directory(
            Path::new("/w/src"),
            vec![crate::explorer::Entry::file("old.rs")],
        );
        assert!(s
            .explorer()
            .unwrap()
            .rows()
            .iter()
            .any(|r| r.name == "old.rs"));

        let Outcome::FileOperation(deleted) = s.delete_in_tree() else {
            panic!("expected a delete");
        };
        s.file_operation_done(&deleted);
        // A directory of the same name exists again — a fresh, empty one.
        s.fill_directory(Path::new("/w"), vec![crate::explorer::Entry::dir("src")]);

        let rows = s.explorer().unwrap().rows();
        assert!(
            !rows.iter().any(|r| r.name == "old.rs"),
            "the deleted directory's rows must not be inherited by its name"
        );
        assert!(
            !rows.iter().find(|r| r.name == "src").unwrap().expanded,
            "nor its expansion"
        );
    }

    #[test]
    fn pinning_the_language_a_name_already_implies_still_counts_as_choosing() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/main.rs"), "fn main() {}\n");
        assert_eq!(s.document.language(), Some("rust"));
        // Choosing Rust for a file already inferred as Rust: by value alone this
        // is indistinguishable from never having chosen.
        s.set_language(Some("rust"));

        let index = s.tab_of(Path::new("/w/main.rs")).expect("it is open");
        s.retarget_tab(index, PathBuf::from("/w/main.txt"));
        assert_eq!(
            s.document_at_index_mut(index).unwrap().language(),
            Some("rust"),
            "a language pinned by hand survives a rename, whatever the new name \
             would have implied"
        );
    }

    #[test]
    fn renaming_onto_a_path_a_tab_still_holds_is_refused() {
        let mut s = with_tree();
        // A file another program deleted, still open here — the tree no longer
        // lists it, so nothing else stands in the way.
        s.open(PathBuf::from("/w/gone.rs"), "fn gone() {}\n");
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0); // Cargo.toml

        assert!(
            matches!(s.rename_in_tree("gone.rs"), Outcome::Message(_)),
            "two buffers for one path is how a save silently loses the other"
        );
    }

    #[test]
    fn creating_onto_a_path_a_tab_still_holds_is_refused() {
        let mut s = with_tree();
        // Deleted by another program, still open here, so the tree does not
        // list it and nothing else stands in the way.
        s.open(PathBuf::from("/w/gone.rs"), "fn gone() {}\n");
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0); // Cargo.toml, so the parent is `/w`

        assert!(
            matches!(s.create_in_tree("gone.rs", false), Outcome::Message(_)),
            "creating it would make an empty file that the old buffer then \
             writes over"
        );
    }

    #[test]
    fn renaming_a_directory_onto_one_holding_an_open_file_is_refused() {
        let mut s = with_tree();
        // `/w/b/x.rs` is open; `b` was removed by another program, so the tree
        // does not list it and the name looks free.
        s.open(PathBuf::from("/w/b/x.rs"), "fn x() {}\n");
        s.fill_directory(
            Path::new("/w"),
            vec![
                crate::explorer::Entry::dir("a"),
                crate::explorer::Entry::file("Cargo.toml"),
            ],
        );
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert_eq!(s.explorer().unwrap().selection().unwrap().name, "a");

        assert!(
            matches!(s.rename_in_tree("b"), Outcome::Message(_)),
            "renaming a directory onto one whose subtree a tab still holds \
             would make two buffers for the same file"
        );
    }

    #[test]
    fn stamping_does_not_touch_the_entry_below_the_one_being_undone() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0); // Cargo.toml
        let Outcome::FileOperation(first) = s.rename_in_tree("Cargo.lock") else {
            panic!("expected a rename");
        };
        s.file_operation_done(&first);
        s.fill_directory(
            Path::new("/w"),
            vec![
                crate::explorer::Entry::dir("src"),
                crate::explorer::Entry::file("Cargo.lock"),
            ],
        );
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(second) = s.rename_in_tree("Cargo.toml2") else {
            panic!("expected a second rename");
        };
        s.file_operation_done(&second);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);

        // Undoing the second: the frontend stamps whatever it just moved, and
        // that must not land on the first rename's entry, which is now on top.
        let Outcome::FileOperation(undo) = s.run("undo", None, 0) else {
            panic!("expected an undo");
        };
        s.stamp_last_undo(crate::files::Stamp {
            len: 999,
            modified: None,
        });
        s.file_operation_done(&undo);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);

        let Outcome::FileOperation(crate::files::Operation::Rename { expect, .. }) =
            s.run("undo", None, 0)
        else {
            panic!("the earlier rename should still be undoable");
        };
        assert_eq!(
            expect, None,
            "the first rename's entry must not carry a stamp taken from the \
             second rename's file"
        );
    }

    #[test]
    fn undoing_a_rename_onto_a_path_a_tab_holds_is_refused() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusDown", None, 0); // Cargo.toml
        let Outcome::FileOperation(renamed) = s.rename_in_tree("Cargo.lock") else {
            panic!("expected a rename");
        };
        s.file_operation_done(&renamed);

        // A new `Cargo.toml` gets opened, then removed by another program — so
        // the tree does not list it and the path looks free again.
        s.open(PathBuf::from("/w/Cargo.toml"), "someone else's\n");
        s.run("workbench.files.action.focusFilesExplorer", None, 0);

        assert!(
            matches!(s.run("undo", None, 0), Outcome::Message(_)),
            "undoing onto a path a tab still holds would make two buffers for it"
        );
        assert!(
            s.can_undo_file_operation(),
            "and the undo stays available for once the tab is closed"
        );
    }

    #[test]
    fn a_new_folder_does_not_inherit_a_vanished_ones_children() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.expand", None, 0); // open `src`
        s.fill_directory(
            Path::new("/w/src"),
            vec![crate::explorer::Entry::file("old.rs")],
        );
        assert!(s
            .explorer()
            .unwrap()
            .rows()
            .iter()
            .any(|r| r.name == "old.rs"));

        // `src` is removed outside deco. Something refreshes the *parent* — a
        // sibling mutation does exactly that — so the row goes, while `src`'s
        // own listing and expansion stay cached behind it.
        s.fill_directory(
            Path::new("/w"),
            vec![crate::explorer::Entry::file("Cargo.toml")],
        );

        let Outcome::FileOperation(made) = s.create_in_tree("src", true) else {
            panic!("expected a create");
        };
        s.file_operation_done(&made);
        s.fill_directory(
            Path::new("/w"),
            vec![
                crate::explorer::Entry::dir("src"),
                crate::explorer::Entry::file("Cargo.toml"),
            ],
        );

        assert!(
            !s.explorer()
                .unwrap()
                .rows()
                .iter()
                .any(|r| r.name == "old.rs"),
            "a new folder must not show the rows of the one that had its name"
        );
    }

    #[test]
    fn a_folder_that_could_not_be_made_still_clears_what_was_there() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.expand", None, 0); // open `src`
        s.fill_directory(
            Path::new("/w/src"),
            vec![crate::explorer::Entry::file("old.rs")],
        );

        // `src` is removed outside deco, and a refresh of the parent alone
        // drops its row while its listing and expansion stay cached behind it.
        s.fill_directory(
            Path::new("/w"),
            vec![crate::explorer::Entry::file("Cargo.toml")],
        );

        // deco tries to make a folder called `src` — the tree says the name is
        // free, which is the only reason this reaches a disk at all. It fails,
        // because another program recreated `src` in the meantime.
        let Outcome::FileOperation(made) = s.create_in_tree("src", true) else {
            panic!("expected a create");
        };
        s.file_operation_failed(&made, "File exists (os error 17)");
        s.fill_directory(
            Path::new("/w"),
            vec![
                crate::explorer::Entry::dir("src"),
                crate::explorer::Entry::file("Cargo.toml"),
            ],
        );

        assert!(
            !s.explorer()
                .unwrap()
                .rows()
                .iter()
                .any(|r| r.name == "old.rs"),
            "a stranger's directory must not be drawn with the dead one's rows"
        );
    }

    #[test]
    fn a_rename_that_failed_does_not_dress_the_blocker_in_old_rows() {
        let mut s = with_tree();
        s.fill_directory(
            Path::new("/w"),
            vec![
                crate::explorer::Entry::dir("a"),
                crate::explorer::Entry::dir("b"),
            ],
        );
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusFirst", None, 0);
        s.run("list.expand", None, 0); // `a`
        s.fill_directory(
            Path::new("/w/a"),
            vec![crate::explorer::Entry::file("mine.rs")],
        );
        while s
            .explorer()
            .and_then(|e| e.selection())
            .map(|row| row.path != Path::new("/w/b"))
            .unwrap_or(false)
        {
            s.run("list.focusDown", None, 0);
        }
        s.run("list.expand", None, 0); // `b`
        s.fill_directory(
            Path::new("/w/b"),
            vec![crate::explorer::Entry::file("theirs.rs")],
        );

        // `b` goes, outside deco, and only the parent is refreshed.
        s.fill_directory(Path::new("/w"), vec![crate::explorer::Entry::dir("a")]);

        // Renaming `a` onto the free name `b` fails: something else took it
        // back between the tree reading and deco trying.
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        s.run("list.focusFirst", None, 0);
        let Outcome::FileOperation(renamed) = s.rename_in_tree("b") else {
            panic!("expected a rename");
        };
        s.file_operation_failed(&renamed, "File exists (os error 17)");
        s.fill_directory(
            Path::new("/w"),
            vec![
                crate::explorer::Entry::dir("a"),
                crate::explorer::Entry::dir("b"),
            ],
        );

        let names: Vec<String> = s
            .explorer()
            .unwrap()
            .rows()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert!(
            !names.contains(&"theirs.rs".to_owned()),
            "whatever now holds `b` is not the `b` that had these: {names:?}"
        );
        assert!(
            names.contains(&"mine.rs".to_owned()),
            "and the source, which did not move, keeps its own: {names:?}"
        );
    }

    #[test]
    fn a_renamed_directory_does_not_inherit_the_destinations_leftovers() {
        // Puts the selection on a named path, whatever the tree's shape is.
        // Renaming the wrong row would prove nothing, and the clamp after a
        // refill moves the selection on its own.
        fn focus(s: &mut Session, path: &str) {
            s.run("workbench.files.action.focusFilesExplorer", None, 0);
            s.run("list.focusFirst", None, 0);
            for _ in 0..64 {
                let at = s
                    .explorer()
                    .and_then(|e| e.selection())
                    .is_some_and(|row| row.path == Path::new(path));
                if at {
                    return;
                }
                s.run("list.focusDown", None, 0);
            }
            panic!("no row for {path}");
        }

        let mut s = with_tree();
        s.fill_directory(
            Path::new("/w"),
            vec![
                crate::explorer::Entry::dir("a"),
                crate::explorer::Entry::dir("b"),
            ],
        );

        // `a` holds a directory of its own, which nobody opens: after the
        // rename it is the row that must be asked about rather than assumed.
        focus(&mut s, "/w/a");
        s.run("list.expand", None, 0);
        s.fill_directory(
            Path::new("/w/a"),
            vec![
                crate::explorer::Entry::dir("sub"),
                crate::explorer::Entry::file("mine.rs"),
            ],
        );

        // `b` holds a directory with the same name, and *that* one is open —
        // so the tree remembers something a level below `b` that re-keying `a`
        // over it cannot overwrite, because `a` has no listing that deep.
        focus(&mut s, "/w/b");
        s.run("list.expand", None, 0);
        s.fill_directory(Path::new("/w/b"), vec![crate::explorer::Entry::dir("sub")]);
        focus(&mut s, "/w/b/sub");
        s.run("list.expand", None, 0);
        s.fill_directory(
            Path::new("/w/b/sub"),
            vec![crate::explorer::Entry::file("theirs.rs")],
        );
        assert!(s
            .explorer()
            .unwrap()
            .rows()
            .iter()
            .any(|r| r.name == "theirs.rs"));

        // `b` is removed outside deco and a later refresh of the parent drops
        // its row — while its listings and expansions stay cached behind it.
        s.fill_directory(Path::new("/w"), vec![crate::explorer::Entry::dir("a")]);

        // Now rename `a` onto the free name `b`.
        focus(&mut s, "/w/a");
        let Outcome::FileOperation(renamed) = s.rename_in_tree("b") else {
            panic!("expected a rename");
        };
        s.file_operation_done(&renamed);
        s.fill_directory(Path::new("/w"), vec![crate::explorer::Entry::dir("b")]);

        let names: Vec<String> = s
            .explorer()
            .unwrap()
            .rows()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert!(
            names.contains(&"mine.rs".to_owned()),
            "the renamed directory keeps its own contents: {names:?}"
        );
        assert!(
            !names.contains(&"theirs.rs".to_owned()),
            "and none of what the old `b` left behind: {names:?}"
        );
    }

    #[test]
    fn a_delete_cannot_be_undone_and_does_not_offer_an_older_one() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(create) = s.create_in_tree("new.rs", false) else {
            panic!("expected an operation");
        };
        s.file_operation_done(&create);
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        assert!(s.can_undo_file_operation());

        // Now delete something. The create below it must not become what
        // `ctrl+z` offers — that would undo the wrong thing entirely.
        s.run("list.focusDown", None, 0);
        let Outcome::FileOperation(deleted) = s.delete_in_tree() else {
            panic!("expected an operation");
        };
        // The stack is cleared once the delete is a fact, not when it is asked
        // for — a refusal must not cost the earlier undos.
        s.file_operation_done(&deleted);
        assert!(
            !s.can_undo_file_operation(),
            "a delete clears the stack rather than hiding under it"
        );
        assert!(matches!(s.run("undo", None, 0), Outcome::Message(_)));
    }

    #[test]
    fn an_operation_the_disk_refused_comes_back_off_the_stack() {
        let mut s = with_tree();
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(operation) = s.create_in_tree("new.rs", false) else {
            panic!("expected an operation");
        };
        assert!(s.can_undo_file_operation());

        s.file_operation_failed(&operation, "permission denied");
        assert!(
            !s.can_undo_file_operation(),
            "nothing happened, so there is nothing to undo"
        );
    }

    #[test]
    fn the_trees_undo_does_not_touch_the_text() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/Cargo.toml"), "[package]\n");
        s.run("workbench.files.action.focusFilesExplorer", None, 0);
        let Outcome::FileOperation(create) = s.create_in_tree("new.rs", false) else {
            panic!("expected an operation");
        };
        s.file_operation_done(&create);

        // Back to the text, and `ctrl+z` there is the document's own undo.
        s.run("workbench.action.focusActiveEditorGroup", None, 0);
        assert_ne!(
            s.run("undo", None, 0),
            Outcome::FileOperation(crate::files::Operation::DeleteIfEmpty {
                path: PathBuf::from("/w/src/new.rs"),
                directory: false,
                expect: None,
            }),
            "undo in the text must not move files"
        );
    }

    #[test]
    fn a_region_that_does_not_fit_is_remembered_and_says_so() {
        // The key was pressed and nothing appeared, which without a sentence
        // reads as an editor that ignored it. The *state* is kept, so widening
        // the window shows what was asked for rather than needing another press.
        let mut s = session();
        s.resize(24, 24);

        let outcome = s.run("workbench.action.toggleSidebarVisibility", None, 0);
        assert_eq!(
            outcome,
            Outcome::Message("no room for the side bar in this window".to_owned())
        );
        assert!(s.regions().side_bar.is_none());

        s.resize(100, 24);
        assert!(
            s.regions().side_bar.is_some(),
            "a wider window shows what was already asked for"
        );
    }

    #[test]
    fn toggling_the_side_bar_leaves_the_keyboard_in_the_text() {
        // VS Code's behaviour, and the point of it: showing the tree should not
        // cost you your place in the file.
        let mut s = session();
        s.resize(80, 24);
        press(&mut s, "ctrl+b");

        assert_eq!(s.focus(), Focus::Editor);
        assert_eq!(s.context.get("sideBarFocus"), Some(&json!(false)));
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(true)));
    }

    #[test]
    fn focusing_a_region_shows_it_first() {
        // Focusing something invisible is the one thing this must not do.
        let mut s = session();
        s.resize(80, 24);

        assert_eq!(
            s.run("workbench.action.focusPanel", None, 0),
            Outcome::Handled
        );
        assert!(s.regions().panel.is_some());
        assert_eq!(s.focus(), Focus::Panel);
        assert_eq!(s.context.get("panelFocus"), Some(&json!(true)));
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(false)));
    }

    #[test]
    fn hiding_the_region_that_has_the_keyboard_gives_it_back() {
        let mut s = session();
        s.resize(80, 24);
        s.run("workbench.action.focusSideBar", None, 0);
        assert_eq!(s.focus(), Focus::SideBar);

        press(&mut s, "ctrl+b");
        assert_eq!(
            s.focus(),
            Focus::Editor,
            "the keyboard is nowhere otherwise"
        );
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(true)));
    }

    #[test]
    fn a_region_with_the_keyboard_does_not_get_typed_into() {
        // Every editing command acts on the document, and the document is not
        // what has focus. The unbound-printable fallback goes the same way,
        // which is why the guard is on the command rather than on the binding.
        let mut s = session();
        s.open(PathBuf::from("/w/a.txt"), "hello\n");
        s.resize(80, 24);
        s.run("workbench.action.focusSideBar", None, 0);

        press(&mut s, "x");
        press(&mut s, "enter");
        press(&mut s, "backspace");
        s.run("editor.action.selectAll", None, 0);

        assert_eq!(s.document.buffer.text(), "hello\n", "untouched");
        assert!(!s.document.dirty);

        // And it comes back the moment the editor has the keyboard again.
        s.run("workbench.action.focusActiveEditorGroup", None, 0);
        press(&mut s, "x");
        assert_eq!(s.document.buffer.text(), "xhello\n");
    }

    #[test]
    fn the_side_bar_goes_where_the_setting_says() {
        let mut s = session();
        s.set_workspace_settings(r#"{"workbench.sideBar.location": "right"}"#);
        s.resize(80, 24);
        press(&mut s, "ctrl+b");

        let regions = s.regions();
        assert_eq!(regions.editor.x, 0, "the text keeps the left edge");
        assert!(regions.side_bar.expect("showing").x > regions.editor.x);
    }

    #[test]
    fn an_unimplemented_command_says_which_feature_it_is() {
        // The panel exists now; a terminal to put in it does not, which is what
        // this key is still waiting on.
        let mut s = searchable("x\n");
        assert_eq!(
            s.run("workbench.action.terminal.toggleTerminal", None, 0),
            Outcome::Message("Toggle Terminal is not implemented yet".to_owned())
        );
        assert_eq!(
            s.status.as_deref(),
            Some("Toggle Terminal is not implemented yet")
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
