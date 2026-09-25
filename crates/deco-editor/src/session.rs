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
/// The prompt builds these identifiers and the submit handler reads them. Shared
/// constants prevent a typo in either place from being treated as "deny".
const CONSENT_ALLOW: &str = "allow";
const CONSENT_DENY: &str = "deny";
const CHECKOUT_CANCEL: &str = "cancel";

/// The length of `text` in UTF-16 code units, which is how positions count.
fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// Why a batch of server-computed edits could not be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    /// Two of the edits covered the same text.
    ///
    /// Overlapping edits have no well-defined result. Reject the entire batch
    /// without changing the document.
    #[error("the server sent overlapping edits, which have no well-defined result")]
    Overlapping,
}

/// Rows the side bar spends on its heading before the tree starts.
///
/// The title and the blank line under it. Defined here rather than in a renderer
/// because the session subtracts these rows to compute how many rows the tree can
/// scroll within. Every frontend that draws the heading must use the same value.
pub const EXPLORER_CHROME_ROWS: usize = 2;

/// Which region of the window has the keyboard.
///
/// This follows VS Code's division, which its `when` clauses expose as
/// `sideBarFocus`. A key can mean one thing in the text and another in a tree. The
/// keymap resolves that difference, not each command.
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

/// Which view the side bar is showing.
///
/// VS Code calls these viewlets and switches between them with
/// `workbench.view.*`. The side bar is one region that shows one of several views.
/// Two are implemented. Search is the third view that the [chrome](crate::layout)
/// lists as not yet implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SideBarView {
    /// The [file tree](crate::explorer). What the side bar opens on.
    #[default]
    Explorer,
    /// The [source-control view](crate::scm).
    SourceControl,
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
/// Holds the state that must be preserved across a tab switch: the text and its
/// history, the cursor and scroll position, and the diagnostics a server has
/// published for it. The find bar is not preserved. It closes on a switch, as it
/// does when a file replaces the document, because its match list describes text
/// that is no longer on screen.
#[derive(Debug)]
struct Tab {
    document: Document,
    view: View,
    diagnostics: Vec<deco_lsp::Diagnostic>,
    semantic: Vec<deco_lsp::requests::SemanticSpan>,
    /// The find bar as this tab left it.
    ///
    /// Stored per tab rather than per session, so switching away keeps the bar
    /// with the document it describes instead of discarding it. A match list is
    /// stale only when it belongs to a *different* document. A single match list
    /// shared by all tabs had to be discarded on every switch.
    find: Find,
}

/// One editor group, as something drawing it sees it.
///
/// # Why the renderer is given this rather than the session
///
/// A renderer that reads `session.document` can only draw the focused group,
/// because that is the only group the session exposes directly. This type
/// describes one group: its document, its own view onto it, and its tabs. That
/// allows a second group to be drawn beside the first.
///
/// Borrowed rather than owned. It is built on demand from state the session keeps,
/// and must not be a second copy that can diverge.
pub struct Pane<'a> {
    /// The document showing in this group.
    pub document: &'a Document,
    /// This group's own view onto it. Scroll position and cursor are per group.
    pub view: &'a View,
    /// The server's classification of that document, if any.
    pub semantic: &'a [deco_lsp::requests::SemanticSpan],
    /// The problems published for it.
    pub diagnostics: &'a [deco_lsp::Diagnostic],
    /// The tabs open in this group, in display order.
    pub tabs: Vec<TabLabel>,
    /// Whether this is the group with the keyboard.
    ///
    /// The renderer draws the caret and the find bar's match highlighting only
    /// in the focused group.
    pub focused: bool,
    /// Diff-specific labels and line decoration, for a source-control comparison.
    pub comparison: Option<ComparisonPane<'a>>,
}

/// The source line represented by one aligned row of a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComparisonLine {
    /// One-based line number in the source, or `None` for an alignment gap.
    pub number: Option<usize>,
    /// What the row means on this side of the comparison.
    pub kind: ComparisonLineKind,
}

/// How one side of a comparison should decorate an aligned row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonLineKind {
    Context,
    Added,
    Removed,
    Modified,
    Empty,
}

/// Additional presentation data for a pane in a source-control comparison.
pub struct ComparisonPane<'a> {
    /// `HEAD`, `Index`, or `Working Tree`.
    pub label: &'a str,
    /// One entry per document line.
    pub lines: &'a [ComparisonLine],
}

#[derive(Debug)]
struct ComparisonSide {
    document: Document,
    view: View,
    label: &'static str,
    lines: Vec<ComparisonLine>,
}

#[derive(Debug)]
struct ComparisonView {
    left: ComparisonSide,
    right: ComparisonSide,
    focused_right: bool,
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
/// Large enough to hold every file used in a typical session. Files beyond the
/// limit keep the alphabetical order produced by the directory walk.
const MAX_RECENT: usize = 64;

/// The picker row that means "work it out from the file name" rather than naming
/// a language.
///
/// Uses deco's own namespace because VS Code has no identifier for this choice.
/// It is not a language ID, so a language-style ID would make
/// `[deco.language.auto]` look like a working settings key.
const AUTO_LANGUAGE: &str = "deco.language.auto";

/// A path with its `.` and `..` segments resolved, without touching the disk.
///
/// Treats `/w/src/main.rs` and `/w/./src/../src/main.rs` as the same path, which
/// decides whether two tabs are one file. This is **not** `fs::canonicalize`: the
/// core has no filesystem, and a path that does not exist yet (a file being
/// created) must also normalise.
///
/// Symlinks are not resolved. Two names for one file through a link open two
/// tabs, as in VS Code.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Pop only a normal segment. A leading `..` is part of the path,
                // and dropping it would change where the path points.
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

/// Aligns two texts so corresponding changed runs occupy the same screen rows.
fn comparison_rows(
    original: &str,
    modified: &str,
) -> ((String, Vec<ComparisonLine>), (String, Vec<ComparisonLine>)) {
    fn clean(line: &str) -> &str {
        line.strip_suffix('\n')
            .unwrap_or(line)
            .strip_suffix('\r')
            .unwrap_or_else(|| line.strip_suffix('\n').unwrap_or(line))
    }
    fn push_context(
        original: &[&str],
        modified: &[&str],
        at: &mut (usize, usize),
        end: (usize, usize),
        text: (&mut Vec<String>, &mut Vec<String>),
        lines: (&mut Vec<ComparisonLine>, &mut Vec<ComparisonLine>),
    ) {
        while at.0 < end.0 && at.1 < end.1 {
            text.0.push(clean(original[at.0]).to_owned());
            text.1.push(clean(modified[at.1]).to_owned());
            lines.0.push(ComparisonLine {
                number: Some(at.0 + 1),
                kind: ComparisonLineKind::Context,
            });
            lines.1.push(ComparisonLine {
                number: Some(at.1 + 1),
                kind: ComparisonLineKind::Context,
            });
            at.0 += 1;
            at.1 += 1;
        }
    }

    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let modified_lines: Vec<&str> = modified.split_inclusive('\n').collect();
    let diff = deco_scm::diff(original, modified);
    let mut left_text = Vec::new();
    let mut right_text = Vec::new();
    let mut left_lines = Vec::new();
    let mut right_lines = Vec::new();
    let mut at = (0usize, 0usize);

    for hunk in &diff.hunks {
        push_context(
            &original_lines,
            &modified_lines,
            &mut at,
            (hunk.head.start, hunk.working.start),
            (&mut left_text, &mut right_text),
            (&mut left_lines, &mut right_lines),
        );
        let rows = hunk.head.len().max(hunk.working.len());
        for offset in 0..rows {
            let left = hunk.head.start + offset;
            let right = hunk.working.start + offset;
            let has_left = left < hunk.head.end;
            let has_right = right < hunk.working.end;
            left_text.push(if has_left {
                clean(original_lines[left]).to_owned()
            } else {
                String::new()
            });
            right_text.push(if has_right {
                clean(modified_lines[right]).to_owned()
            } else {
                String::new()
            });
            left_lines.push(ComparisonLine {
                number: has_left.then_some(left + 1),
                kind: match (has_left, has_right) {
                    (true, true) => ComparisonLineKind::Modified,
                    (true, false) => ComparisonLineKind::Removed,
                    (false, _) => ComparisonLineKind::Empty,
                },
            });
            right_lines.push(ComparisonLine {
                number: has_right.then_some(right + 1),
                kind: match (has_left, has_right) {
                    (true, true) => ComparisonLineKind::Modified,
                    (false, true) => ComparisonLineKind::Added,
                    (_, false) => ComparisonLineKind::Empty,
                },
            });
        }
        at = (hunk.head.end, hunk.working.end);
    }
    push_context(
        &original_lines,
        &modified_lines,
        &mut at,
        (original_lines.len(), modified_lines.len()),
        (&mut left_text, &mut right_text),
        (&mut left_lines, &mut right_lines),
    );

    if left_lines.is_empty() {
        left_text.push(String::new());
        right_text.push(String::new());
        left_lines.push(ComparisonLine {
            number: None,
            kind: ComparisonLineKind::Empty,
        });
        right_lines.push(ComparisonLine {
            number: None,
            kind: ComparisonLineKind::Empty,
        });
    }
    (
        (left_text.join("\n"), left_lines),
        (right_text.join("\n"), right_lines),
    )
}

/// Exactly the bytes to write for `document`.
///
/// Uses the document's *own* settings, not the session's.
/// `files.insertFinalNewline` can differ per language, and saving all tabs must
/// apply each document's value.
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
    /// A second *view*, not a second document. `ctrl+\` in VS Code shows one file
    /// in two groups, which is one buffer with two views. Two documents would be
    /// two divergent copies of one file, which [`Session::open`] also prevents for
    /// tabs.
    ///
    /// [`Session::view`] is always the view of the focused group, and this is the
    /// other one. This is the same zipper layout the tabs use, so commands that
    /// read `session.view` work without knowing that groups exist.
    /// Whether the revert now in flight should close the tab when it lands.
    ///
    /// The frontend answers a `Revert` with only the file's text, so the session
    /// records which of the two revert commands made the request.
    pending_close: bool,
    /// How a project-wide search matches.
    ///
    /// Separate from the find bar's options, as in VS Code. Sharing them would
    /// let case sensitivity set for a workspace search change what the next
    /// `ctrl+f` matches.
    search_options: deco_core::search::SearchOptions,
    /// Whether the last command was a quit that was refused for unsaved work.
    ///
    /// Cleared by any other command, so "quit again" applies only to the next
    /// keystroke, not to a later quit.
    quit_refused: bool,
    split_view: Option<View>,
    /// Whether the group with the keyboard is the second one.
    ///
    /// Only meaningful while split. Used to list the panes in screen order while
    /// `view` remains the active one.
    split_focused: bool,
    /// A read-only, side-by-side source-control comparison over the active tab.
    comparison: Option<ComparisonView>,
    /// Paths that have been on screen, most recently first.
    ///
    /// Quick open (`ctrl+p`) lists these first, because the wanted file is
    /// usually one that was recently open. VS Code orders quick open the same way.
    ///
    /// **This session only.** VS Code keeps its history in workspace storage. deco
    /// [writes no files](../../../docs/configuration.md), so the list starts empty
    /// in each session and is not persisted.
    recent: Vec<PathBuf>,
    /// Tabs to the left of the active one, in display order.
    ///
    /// The active tab's state lives directly in [`Session::document`],
    /// [`Session::view`] and [`Session::diagnostics`]. This is a zipper, not an
    /// indexed list. Code that reads `session.document` always sees the active
    /// tab, and switching tabs moves whole structs instead of redirecting every
    /// reader through an index.
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
    /// command it forwards will be handled. For example, the terminal frontend
    /// can format a document because it has a language-server client, and the
    /// GPU frontend cannot. Listing a command without knowing that it is handled
    /// would make the palette offer actions the editor cannot perform.
    pub frontend_commands: Vec<crate::commands::PaletteEntry>,
    /// Whether the frontend can draw a line broken across several rows.
    ///
    /// Declared by the frontend for the same reason as
    /// [`Session::frontend_commands`]: the core cannot know the frontend's
    /// capabilities. The GPU frontend lays out one document line per row. If the
    /// session wrapped lines anyway, it would scroll and move the caret by rows
    /// that frontend never draws, and the caret would not match the text.
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
    /// Kept beside the diagnostics for the same reason: frontends read them from
    /// one place regardless of the source. Empty when no server is running, when
    /// the server does not provide them, or when the response has not arrived. In
    /// all three cases the lexer's colouring is used.
    pub semantic_tokens: Vec<deco_lsp::requests::SemanticSpan>,
    /// Diagnostics for the open document, newest publication wins.
    ///
    /// Owned by the session rather than by the LSP client, so frontends read
    /// diagnostics from one place regardless of the source: currently a language
    /// server, and later an extension or a linter.
    pub diagnostics: Vec<deco_lsp::Diagnostic>,
    /// Problems found while loading the user's configuration, kept so the
    /// frontend can show them rather than failing to start.
    pub problems: Vec<String>,
    /// Whether the query being typed is the first half of a replace.
    ///
    /// `ctrl+shift+f` and `ctrl+shift+h` open the same prompt and differ only in
    /// what accepting it does, so the session records which key was pressed.
    replacing_in_files: bool,
    /// The query a replace-in-files is waiting to be given a replacement for.
    replace_query: String,
    /// Set by a successful checkout until the frontend re-reads open files.
    checkout_completed: bool,
    /// Whether the side bar is showing.
    ///
    /// Whether it *fits* is decided separately by [`crate::layout::regions`]
    /// from the window size. A toggle on a narrow terminal is remembered and
    /// takes effect when the window grows.
    side_bar: bool,
    /// Whether the panel is showing.
    panel: bool,
    /// Which region has the keyboard.
    focus: Focus,
    /// File operations that can be taken back, most recent last.
    ///
    /// The explorer's own undo stack, separate from the text's, as in VS Code.
    /// `ctrl+z` in a buffer restores text and must not move files because the
    /// last action happened in the tree. Focus decides which stack a key press
    /// reaches.
    ///
    /// Holds the *inverse* of each operation, so undo runs the top entry. A
    /// delete has no inverse, so it clears the stack instead of leaving older
    /// entries that `ctrl+z` would skip to.
    explorer_undo: Vec<crate::files::Operation>,
    /// The undo the frontend is carrying out, if one is in flight.
    ///
    /// Popped off [`Session::explorer_undo`] and held here until the frontend
    /// reports the result, so a failure can restore it. Otherwise a transient
    /// failure, such as undoing `a → b` after another program has created `a`,
    /// would lose the entry and leave nothing to retry.
    pending_undo: Option<crate::files::Operation>,
    /// Files a delete took away that a language server still has open.
    closed_documents: Vec<PathBuf>,
    /// The workspace tree, once a frontend has said where the workspace is.
    ///
    /// `None` until then, because the session does not derive the root itself.
    /// That requires a working directory and the path deco was started with, and
    /// the core has neither. Making the root a full session concept is the first
    /// step of the roadmap's workspace-switching chapter. This field is only the
    /// tree's own copy of the root.
    explorer: Option<crate::Explorer>,
    /// The rectangle the frontend last handed over.
    ///
    /// Toggling a region re-divides the same window, and only the session knows
    /// that the division changed. Without this, a toggle would take effect only
    /// on the next resize.
    screen: (usize, usize),
    /// The number the next multi-document edit will be tagged with.
    ///
    /// Allocated here because this layer can see more than one document. A
    /// buffer's history only stores the number it was given.
    next_group: u64,
    /// The last `git status` result, if it has been run.
    ///
    /// Supplied by the frontend, like directory listings, because the core has
    /// no filesystem and cannot spawn processes. `None` covers three cases that
    /// the session cannot distinguish: not yet requested, git not available, or
    /// not a repository. Only the frontend that ran the command can tell them
    /// apart.
    scm: Option<deco_scm::Status>,
    /// Whether the status is stale.
    ///
    /// Works like [`Session::directory_wanted`]: the session records what it
    /// needs, and the frontend fetches it. Set when something happens that would
    /// change the `git status` output. Not set on keystrokes, because that would
    /// run one git process per character.
    scm_wanted: bool,
    /// Which view the side bar is showing.
    side_bar_view: SideBarView,
    /// Where the repository begins, once a frontend has said. See
    /// [`Session::set_repository_root`].
    repository_root: Option<PathBuf>,
    /// The source-control view, rebuilt whenever a status arrives.
    ///
    /// Stored rather than derived per render because it holds a *selection*. A
    /// selection recomputed from the status every frame would lose the selected
    /// file whenever the status changed.
    source_control: crate::scm::SourceControl,
    /// What `HEAD` had for each open file, and the marks derived from it.
    ///
    /// Split in two because the parts change at different rates. Fetching the
    /// committed text requires a process, and it changes only on commit. The
    /// buffer changes on every keystroke, and the comparison is a pure
    /// computation. The text is fetched once and cached, and the diff is
    /// recomputed here as the file is edited. The marks follow edits without
    /// running `git` per character.
    committed: std::collections::HashMap<PathBuf, Committed>,
}

/// The path an operation *emptied*, if it emptied one.
///
/// The counterpart to [`crate::files::Operation::arriving`]. A rename empties
/// its source path and a delete empties its path. Cached data about either path
/// now refers to a file that does not exist.
fn moved_from(operation: &crate::files::Operation) -> Option<&Path> {
    match operation {
        crate::files::Operation::Rename { from, .. } => Some(from),
        crate::files::Operation::Delete { path, .. }
        | crate::files::Operation::DeleteIfEmpty { path, .. } => Some(path),
        crate::files::Operation::CreateFile(_) | crate::files::Operation::CreateFolder(_) => None,
    }
}

/// One file's committed text, and the last diff taken against it.
#[derive(Debug, Clone)]
struct Committed {
    /// What `git show HEAD:<path>` said, or `None` when the file is not in
    /// `HEAD` at all — new, or on a branch with nothing committed.
    text: Option<String>,
    /// The marks, and the buffer version they were computed from.
    ///
    /// The version limits diffing to once per edit instead of once per render.
    marks: Option<(i32, deco_scm::Diff)>,
}

/// Applies `edits` to one document and its view.
///
/// A free function because the callers pass different pairs: the active document
/// and its view, or a background tab's. Everything it does (clamping, rejecting
/// overlaps, recording one undo step, marking dirty) applies to the document,
/// whether or not it is on screen.
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

/// Turns a server's edits into one transaction that performs them, or returns
/// why they cannot be applied.
///
/// `Ok(None)` means there was nothing to do. Separate from [`commit`] so that a
/// caller changing several documents can check that *all* of them can be changed
/// before changing any. Every rejection (a range that does not exist, two edits
/// over the same text) happens here, before any buffer is modified.
fn build_transaction(
    document: &Document,
    edits: &[deco_lsp::TextEdit],
) -> Result<Option<deco_core::Transaction>, EditError> {
    use deco_core::{Change, Transaction};

    // Servers often answer an already-formatted document with a no-op edit.
    // Applying it would mark the file dirty and add an empty undo step.
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

    // Overlapping edits have no well-defined result, and the specification
    // forbids them. Reject the batch with an error instead of choosing one.
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
    // Reveal the cursor even for a background tab, so that switching to it shows
    // the cursor where the edit left it.
    view.reveal_cursor(&document.buffer, &document.settings);
    applied
}

impl Session {
    /// Builds a session from the user's configuration.
    ///
    /// A broken `keybindings.json` or a missing theme does not stop the editor
    /// from opening. Both are reported through [`Session::problems`] and the
    /// defaults are used instead, so the editor can still be used to fix the
    /// configuration.
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
            comparison: None,
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
            // Replaced by the first `resize`, which every frontend calls before
            // drawing. The default matches `View`'s, so an unsized session in a
            // test still has a usable layout.
            screen: (80, 24),
            next_group: 0,
            scm: None,
            side_bar_view: SideBarView::default(),
            repository_root: None,
            source_control: crate::scm::SourceControl::default(),
            // Requested at startup so the branch is shown before the first save.
            scm_wanted: true,
            committed: std::collections::HashMap::new(),
            replacing_in_files: false,
            replace_query: String::new(),
            checkout_completed: false,
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
        // Switch to a file that is already open instead of opening it twice. Two
        // tabs for one file would be two divergent copies, and the last save
        // would overwrite the other without warning.
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

        // Open in a new tab, unless the active tab is an unmodified untitled
        // document, which is replaced. This is VS Code's rule, and it prevents
        // `deco file.rs` from starting with an empty tab beside the file.
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
        // The previous document's diagnostics refer to line numbers in another
        // file. Keeping them would show the old file's errors in the new one.
        self.diagnostics.clear();
        // The token list also describes the other file's text.
        self.semantic_tokens.clear();
        // The match list is stale for the same reason: this is a *different*
        // document. The query is kept so the next file can be searched for the
        // same text.
        self.find.close();
        if !carried_query.is_empty() {
            self.find.set_query(carried_query);
        }
        // A different file can have a different gutter width, which changes the
        // text width.
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
        // Compare normalised paths, not the paths as written. `src/main.rs` from
        // the command line and `/w/src/main.rs` from quick open are one file. An
        // exact comparison would open two tabs, with two buffers and two undo
        // histories, and the last save would overwrite the other.
        //
        // Callers are expected to pass absolute paths, and all current callers
        // do. Normalising here as well means new callers do not have to follow
        // that rule.
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
    /// One today. Renderers are written against a list of groups rather than
    /// against the single group the session exposes directly.
    pub fn panes(&self) -> Vec<Pane<'_>> {
        if let Some(comparison) = &self.comparison {
            return vec![
                Pane {
                    document: &comparison.left.document,
                    view: &comparison.left.view,
                    semantic: &[],
                    diagnostics: &[],
                    tabs: vec![TabLabel {
                        title: comparison.left.label.to_owned(),
                        dirty: false,
                        active: !comparison.focused_right,
                    }],
                    focused: !comparison.focused_right,
                    comparison: Some(ComparisonPane {
                        label: comparison.left.label,
                        lines: &comparison.left.lines,
                    }),
                },
                Pane {
                    document: &comparison.right.document,
                    view: &comparison.right.view,
                    semantic: &[],
                    diagnostics: &[],
                    tabs: vec![TabLabel {
                        title: comparison.right.label.to_owned(),
                        dirty: false,
                        active: comparison.focused_right,
                    }],
                    focused: comparison.focused_right,
                    comparison: Some(ComparisonPane {
                        label: comparison.right.label,
                        lines: &comparison.right.lines,
                    }),
                },
            ];
        }
        let mut panes = vec![self.pane(&self.view, true)];
        if let Some(other) = &self.split_view {
            let other = self.pane(other, false);
            if self.split_focused {
                // The active view belongs to the second group, so it is listed
                // second and the stored view first.
                panes.insert(0, other);
            } else {
                panes.push(other);
            }
        }
        panes
    }

    /// How many editor groups there are.
    pub fn group_count(&self) -> usize {
        if self.comparison.is_some() {
            return 2;
        }
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
            comparison: None,
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
    /// Collects all tabs into one list and splits it again around the new index.
    /// This is O(n) in the number of tabs, which is small, and avoids four
    /// separate rotation cases.
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
        // The search string is shared across tabs, but the bar is not, as in VS
        // Code. Opening find in another file shows the same query, and `F3` in a
        // tab that has not been searched uses the last query.
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
        // The text width is not restored. It depends on the gutter of the
        // document on screen, so `relayout` below recomputes it.

        self.right = all.split_off(index);
        self.left = all;
        self.document = chosen.document;
        self.view = chosen.view;
        self.diagnostics = chosen.diagnostics;
        self.semantic_tokens = chosen.semantic;
        // Restored rather than closed, because it describes *this* tab's text.
        // Switching away and back shows the bar as it was left, including
        // matches, as in VS Code.
        self.find = chosen.find;
        if self.find.query().is_empty() && !carried_query.is_empty() {
            self.find.set_query(carried_query);
        }
        // Recompute the layout for this document's gutter, then reveal its caret.
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
    /// A dirty document is not closed, because deco has no confirmation dialog
    /// and a keystroke must not discard edits. Closing the last tab leaves an
    /// untitled document, because the session always shows a document.
    /// Splits the editor, giving the same document a second view.
    ///
    /// The new group starts at the same position as the current one and takes
    /// the keyboard, as in VS Code. Scrolling it afterwards does not move the
    /// first group, so two places in one file are visible at once.
    fn split(&mut self) -> Outcome {
        if self.comparison.is_some() {
            return Outcome::Message("close the diff before splitting the editor".to_owned());
        }
        if self.split_view.is_some() {
            // A third group is not supported. Report it instead of ignoring the
            // key.
            return Outcome::Message("the editor is already split".to_owned());
        }
        self.split_view = Some(self.view.clone());
        self.split_focused = true;
        // Two columns replace one, so both are narrower and wrapped lines break
        // earlier.
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
        if let Some(comparison) = self.comparison.as_mut() {
            comparison.focused_right = index == 1;
            self.refresh_context();
            return Outcome::Handled;
        }
        let wanted = index == 1;
        if wanted != self.split_focused {
            // The active view is always `self.view`, so moving the keyboard swaps
            // views instead of changing an index, as the tabs do.
            if let Some(other) = self.split_view.as_mut() {
                std::mem::swap(other, &mut self.view);
            }
            self.split_focused = wanted;
            // Close the find bar. The find state belongs to the tab, and both
            // groups show the same tab, so its current match is at the *other*
            // group's cursor. A find bar per group requires a tab list per group,
            // which is not implemented.
            self.find.close();
            // The two columns can differ by one cell, so the newly focused view
            // needs the width of its column.
            self.relayout();
        }
        self.refresh_context();
        Outcome::Handled
    }

    /// Refuses to quit while anything is unsaved, and names what.
    ///
    /// `ctrl+w` does not close *one* unsaved document, so `ctrl+q` must not
    /// discard all of them. A second press quits anyway, so the user is never
    /// prevented from quitting. It must be the very next keystroke. A `ctrl+q`
    /// pressed later starts the check again.
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
    /// An untitled document reverts to empty, because there is no file to re-read
    /// and it started empty. This also makes it closable.
    ///
    /// The replacement is recorded in the undo history, so `ctrl+z` restores the
    /// discarded edits.
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
    /// This is VS Code's rule. After a split, the key first restores the single
    /// group.
    fn close_editor(&mut self) -> Outcome {
        if self.comparison.take().is_some() {
            self.relayout();
            self.refresh_context();
            return Outcome::Message("Closed the diff".to_owned());
        }
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
        // A new tab has no matches. The previous find state stays with the
        // previous tab.
        self.find = Find::new();
        self.relayout();
        self.refresh_context();
        Outcome::Handled
    }

    /// Replaces the diagnostics for the open document.
    ///
    /// Replaces rather than appends, as the protocol requires. A server publishes
    /// the complete set for a document each time, and an empty set means there
    /// are no problems.
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

    /// Resolves the open document's settings again, keeping the overrides from the
    /// file and from the keyboard.
    ///
    /// Three events re-resolve: a workspace layer arriving, a rename, and a
    /// language change. Each replaces the whole `EditorSettings`, but two values do
    /// not come from `settings.json`: the indentation detected from the file, and
    /// the `alt+z` toggle. They are re-applied here rather than at each call site,
    /// so a new caller cannot lose them.
    fn resolve_document_settings(&mut self) {
        self.document.settings = EditorSettings::resolve(&self.settings, self.document.language());
        self.document.apply_overrides();
        self.report_unsupported();
    }

    /// Adds anything the resolved settings ask for that deco does not do.
    ///
    /// Each problem is added once. The frontend shows every entry, so re-resolving
    /// the same settings must not add duplicates.
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
        if self
            .document
            .snippet
            .as_ref()
            .is_some_and(|snippet| !snippet.contains(&self.view.selections))
        {
            self.document.snippet = None;
        }
        self.context
            .set("inSnippetMode", self.document.snippet.is_some());
        self.note_active_document();
        let selections = self
            .comparison
            .as_ref()
            .map(|comparison| {
                if comparison.focused_right {
                    &comparison.right.view.selections
                } else {
                    &comparison.left.view.selections
                }
            })
            .unwrap_or(&self.view.selections);
        // VS Code's distinction, which the find bar depends on: `editorTextFocus`
        // is the text area, `textInputFocus` is any text input including the find
        // box, and `editorFocus` covers the whole editor. With the find bar open,
        // `tab` and `ctrl+space` stop resolving, while `left` and `backspace`
        // still resolve and `Find::consume` handles them.
        let find_focus = self.find.visible();
        let on_replace = find_focus && self.find.field() == crate::find::Field::Replace;
        // VS Code's key for "a quick-open widget has the keyboard". It clears
        // `editorTextFocus` for the same reason the find bar does.
        let in_quick_open = self.prompt.is_some();
        self.context.set("inQuickOpen", in_quick_open);
        // VS Code's key for the search view being open. deco uses a prompt rather
        // than a viewlet, but a `when` clause copied from a VS Code
        // keybindings.json should behave the same.
        self.context
            .set("searchViewletVisible", self.searching_project());
        // VS Code's keys for the chrome. Visible and focused are separate:
        // `ctrl+b` shows the side bar without moving the keyboard into it, so
        // `sideBarVisible` and `sideBarFocus` can differ.
        let regions = self.regions();
        self.context
            .set("sideBarVisible", regions.side_bar.is_some());
        self.context.set("panelVisible", regions.panel.is_some());
        self.context
            .set("sideBarFocus", self.focus == Focus::SideBar);
        self.context.set("panelFocus", self.focus == Focus::Panel);
        // The tree's keys. `filesExplorerFocus` is VS Code's key for the explorer
        // having the keyboard, and `listFocus` is for any list having it. The
        // explorer is the only list here, so both currently have the same value,
        // and a `when` clause copied from VS Code that uses either one resolves.
        let in_side_bar = self.focus == Focus::SideBar;
        let explorer_focus =
            in_side_bar && self.explorer.is_some() && self.side_bar_view == SideBarView::Explorer;
        self.context.set("filesExplorerFocus", explorer_focus);
        // Every list, not just the tree, as in VS Code. This is why the
        // source-control view responds to the same `list.*` keys.
        let scm_focus = in_side_bar && self.side_bar_view == SideBarView::SourceControl;
        self.context.set("listFocus", explorer_focus || scm_focus);
        // A deco key, because VS Code has no `when` key for "the source-control
        // view has the keyboard". VS Code's bindings there are contributed by the
        // view rather than gated on a context key. The `deco.` prefix is used for
        // names that VS Code does not have.
        self.context.set("deco.sourceControlFocus", scm_focus);
        // VS Code sets `scmProvider` to the active provider's id. deco has only
        // git, so the value depends only on whether a repository was found. A
        // `when` clause with `scmProvider == 'git'` resolves when git returned a
        // status.
        self.context.set(
            "scmProvider",
            match self.scm.is_some() {
                true => serde_json::json!("git"),
                false => serde_json::json!(""),
            },
        );
        self.context
            .set("explorerViewletVisible", regions.side_bar.is_some());
        // The keys below describe the text. While another region has the
        // keyboard the text does not, so a binding gated on `editorTextFocus`
        // does not resolve in a tree.
        let in_editor = self.focus == Focus::Editor;
        self.context.set(
            "editorTextFocus",
            in_editor && !find_focus && !in_quick_open,
        );
        self.context.set("editorFocus", in_editor);
        self.context.set("textInputFocus", in_editor);
        self.context.set("findWidgetVisible", find_focus);
        // Exactly one of the two inputs has the keyboard, so `enter` can mean
        // "next match" in one and "replace" in the other.
        self.context
            .set("findInputFocussed", find_focus && !on_replace);
        self.context.set("replaceInputFocussed", on_replace);
        self.context
            .set("editorReadonly", self.comparison.is_some());
        self.context.set(
            "editorHasSelection",
            selections.iter().any(|s| !s.is_empty()),
        );
        self.context
            .set("editorHasMultipleSelections", selections.is_multi());
        self.context.set("dirty", self.document.dirty);
        // VS Code's key, so a `when` clause copied from an existing
        // keybindings.json behaves the same here. `gotoNextError` is bound
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
    /// `now_ms` is a monotonic timestamp used for undo grouping. The frontend
    /// owns the clock so that this path stays testable.
    pub fn handle_chord(&mut self, chord: Chord, now_ms: u64) -> Outcome {
        let resolution = self
            .keymap
            .resolve(&mut self.view.chord, chord, &self.context);

        let outcome = match resolution {
            Resolution::Pending { .. } => Outcome::Handled,
            Resolution::Match { command, args } => self.dispatch(&command, args.as_ref(), now_ms),
            Resolution::NoMatch => {
                // An unbound printable key inserts its character. Modifiers other
                // than Shift indicate a command, so those keys insert nothing.
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

    /// Runs a command, letting the find input handle it first.
    ///
    /// The find input is a text input, so while it has the keyboard it handles
    /// the text-editing commands. The [`crate::find`] module explains why this
    /// cannot be expressed as a `when` clause. All other commands, and all
    /// commands while the bar is closed, go to [`Session::run`].
    fn dispatch(
        &mut self,
        command: &str,
        args: Option<&serde_json::Value>,
        now_ms: u64,
    ) -> Outcome {
        // "ctrl+q again" means the very next keystroke. Any other command in
        // between resets the confirmation.
        if !matches!(
            command,
            "workbench.action.quit" | "workbench.action.closeWindow"
        ) {
            self.quit_refused = false;
        }

        // The prompt first. It is drawn over the find bar, so it has the keyboard
        // when both are open.
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
        if self.comparison.is_some() {
            return self.run_in_comparison(command, args, now_ms);
        }
        // Diagnostic navigation is handled here rather than in `commands`
        // because it needs the diagnostic list, which belongs to the session. A
        // command sees only the document, the view and the clipboard.
        let outcome = match command {
            "jumpToNextSnippetPlaceholder" => self.move_snippet(false),
            "jumpToPrevSnippetPlaceholder" => self.move_snippet(true),
            "leaveSnippet" => {
                self.document.snippet = None;
                Outcome::Handled
            }
            "editor.action.marker.next" | "editor.action.marker.nextInFiles" => {
                self.goto_marker(Direction::Next)
            }
            "editor.action.marker.prev" | "editor.action.marker.prevInFiles" => {
                self.goto_marker(Direction::Prev)
            }
            // Undoing a change made to several documents must reach all of
            // them. Handled here rather than in `commands`, because a command
            // there sees one document and this layer sees all of them.
            //
            // Only when the top step is a shared one. Ordinary editing falls
            // through to the single-document undo.
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
            // The chrome. Handled at session level because a region changes how
            // much of the window the text has, which affects every group and the
            // wrap width.
            "workbench.action.toggleSidebarVisibility" => self.toggle_side_bar(),
            "workbench.action.togglePanel" => self.toggle_panel(),
            "workbench.action.closeSidebar" => self.show_side_bar(false),
            "workbench.action.closePanel" => self.show_panel(false),
            "workbench.files.action.focusFilesExplorer" | "workbench.view.explorer" => {
                self.show_side_bar_view(SideBarView::Explorer)
            }
            "workbench.view.scm" => self.show_side_bar_view(SideBarView::SourceControl),
            // Repository commands. Like the tree's `list.*`, they match
            // regardless of focus and check the state inside. An arm that did not
            // match would report a working binding as unknown.
            "git.stage" => self.stage_selected(),
            "git.stageAll" => self.stage_all(),
            "git.unstage" => self.unstage_selected(),
            "git.commit" => self.ask_commit_message(),
            "git.checkout" => self.ask_checkout(),
            "git.refresh" => {
                self.scm_changed();
                Outcome::Handled
            }
            "revealInExplorer" => self.reveal_active_file(),
            // The tree's keys. Routed before the focus guard below, because
            // unlike the editor's commands they act on whatever has the keyboard.
            // Whether the tree has focus is checked inside, not by a guard here.
            // These commands exist regardless of focus, and an arm that did not
            // match would report them as unknown, which the frontend reports for
            // a mistyped binding.
            "list.focusDown" | "list.focusUp" | "list.focusFirst" | "list.focusLast"
            | "list.expand" | "list.collapse" | "list.select" => self.explorer_key(command),
            // The tree's undo uses the same key as the text's, and focus decides
            // which one runs. The workspace-edit arms above are gated on editor
            // focus for the same reason: after a project-wide replace the
            // document has a shared undo step, and `ctrl+z` in the tree must
            // still run the tree's undo, not the text's.
            "undo" if self.focus == Focus::SideBar => self.undo_file_operation(),
            // Prompts for the tree's file operations. They act on the tree's
            // *selection*, which is model state and exists whether or not the
            // tree has the keyboard. There is no focus guard, so they also work
            // from the palette. The keymap separates the keys: `F2` and `delete`
            // are bound to these only under `sideBarFocus`, so in the text they
            // still rename a symbol and delete a character.
            "explorer.newFile" => self.open_tree_prompt(PromptKind::NewFile),
            "explorer.newFolder" => self.open_tree_prompt(PromptKind::NewFolder),
            "renameFile" => self.open_rename_file(),
            "deleteFile" => self.open_tree_prompt(PromptKind::ConfirmDelete),
            "workbench.action.focusSideBar" => self.focus_region(Focus::SideBar),
            "workbench.action.focusPanel" => self.focus_region(Focus::Panel),
            // VS Code's way back to the text from anywhere in the chrome.
            "workbench.action.focusActiveEditorGroup" => self.focus_region(Focus::Editor),
            // Needs the view as well as the document, because wrapping depends on
            // the window width, which the view stores.
            "editor.action.toggleWordWrap" => self.toggle_word_wrap(),
            // The find bar is handled here for the same reason. It needs the
            // whole document and its own state, and a command in `commands` can
            // see neither.
            "actions.find" => self.open_find(false),
            "closeFindWidget" => {
                self.find.close();
                Outcome::Handled
            }
            "editor.action.nextMatchFindAction" => self.step_find(Direction::Next),
            "editor.action.previousMatchFindAction" => self.step_find(Direction::Prev),
            // Applies to the search currently being typed. The two option sets
            // are separate: the find bar's belong to the document on screen, and
            // the project search has its own.
            "toggleFindCaseSensitive" => {
                if self.searching_project() {
                    self.search_options.case_sensitive = !self.search_options.case_sensitive;
                    // Not an early `return`. The end of this function shows an
                    // `Outcome::Message` on the status bar, so the toggle is
                    // visible to the user.
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
            "toggleFindRegex" => {
                if self.searching_project() {
                    self.search_options.regex = !self.search_options.regex;
                    self.report_search_options()
                } else {
                    self.find.toggle_regex();
                    self.find_query_changed()
                }
            }
            "editor.action.startFindReplaceAction" => self.open_find(true),
            // The quick-open prompt. Like the find bar, it needs the whole
            // session, not only the document and view that a command in
            // `commands` sees.
            "workbench.action.gotoLine" => {
                self.prompt = Some(Prompt::plain(PromptKind::GoToLine));
                Outcome::Handled
            }
            "workbench.action.showCommands" => {
                self.prompt = Some(Prompt::list(PromptKind::Commands, self.palette()));
                Outcome::Handled
            }
            // The file list must be read from disk, which only a frontend can do.
            // It calls `offer_files` with the result.
            "workbench.action.quickOpen" => Outcome::Frontend(command.to_owned()),
            // Prompts for the query first, seeded from the cursor, so the search
            // is not limited to the text at the cursor.
            "workbench.action.findInFiles" => {
                self.replacing_in_files = false;
                self.prompt = Some(Prompt::seeded(
                    PromptKind::SearchQuery,
                    self.search_seed().unwrap_or_default(),
                ));
                self.refresh_context();
                Outcome::Handled
            }
            // The same query prompt, recorded as the first step of a replace. VS
            // Code opens its search view with the replace box visible. deco shows
            // one prompt at a time, so it asks for the query and then the
            // replacement.
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
            // Tabs. Handled at session level because they move whole documents,
            // and a command in `commands` sees only one.
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
            // Checked before `commands::execute` returns `Outcome::Save`. A
            // document with no path cannot be written, so `ctrl+s` opens Save As,
            // as in VS Code. Otherwise an untitled tab could not be saved, and so
            // could not be closed.
            "workbench.action.files.save" if self.document.path.is_none() => self.offer_save_as(),
            "workbench.action.files.saveAs" => self.offer_save_as(),
            "workbench.action.quit" | "workbench.action.closeWindow" => self.quit(),
            "workbench.action.files.revert" => self.revert(false),
            "workbench.action.revertAndCloseActiveEditor" => self.revert(true),
            "workbench.action.files.openFile" => self.offer_open_path(),
            "editor.action.replaceOne" => self.replace_one(now_ms),
            "editor.action.replaceAll" => self.replace_all(now_ms),
            // Commands that need something the core does not have. They are
            // listed by name rather than forwarding every unknown command, so a
            // typo in a keybinding is still reported as unknown.
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
            // Editor commands apply only to the editor. While another region has
            // the keyboard they are ignored. `commands` implements typing,
            // motion, undo and the clipboard, and all of them act on the
            // document, which does not have focus.
            //
            // A guard here rather than a `when` clause on each binding, because
            // the fallback that types an unbound printable key does not go
            // through the keymap, so a clause cannot apply to it.
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
        // A bound command that nothing handles would otherwise do nothing, which
        // looks like an unresponsive editor. Report it instead, with different
        // messages for a planned feature and for an identifier that does not
        // exist.
        let outcome = match outcome {
            // A command the frontend declared is routed to the frontend,
            // regardless of its name. The identifiers above are deco's own and
            // fixed. Extension commands depend on what is installed, so this is
            // the only way to route them, and it is the purpose of
            // `frontend_commands`.
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
        // Shared by all commands, so a command handled above reports to the
        // status bar like any other. Otherwise F8 would move to an error without
        // showing its message.
        if let Outcome::Message(message) = &outcome {
            self.status = Some(message.clone());
        }
        self.refresh_context();
        outcome
    }

    /// Runs only navigation and copying against the focused read-only diff side.
    fn run_in_comparison(
        &mut self,
        command: &str,
        args: Option<&serde_json::Value>,
        now_ms: u64,
    ) -> Outcome {
        match command {
            "workbench.action.closeActiveEditor" => return self.close_editor(),
            "workbench.action.focusFirstEditorGroup" => return self.focus_group(0),
            "workbench.action.focusSecondEditorGroup" => return self.focus_group(1),
            "workbench.action.focusThirdEditorGroup" => return self.focus_group(2),
            "workbench.action.splitEditor" | "workbench.action.splitEditorRight" => {
                return self.split()
            }
            _ => {}
        }
        let safe = command.starts_with("cursor")
            || command.starts_with("scroll")
            || matches!(
                command,
                "editor.action.clipboardCopyAction"
                    | "editor.action.selectAll"
                    | "expandLineSelection"
                    | "removeSecondaryCursors"
                    | "cancelSelection"
            );
        if !safe {
            return Outcome::Message("the diff view is read only".to_owned());
        }

        let mut comparison = self.comparison.take().expect("checked above");
        let (outcome, scroll_top, scroll_row) = {
            let side = if comparison.focused_right {
                &mut comparison.right
            } else {
                &mut comparison.left
            };
            std::mem::swap(&mut self.document, &mut side.document);
            std::mem::swap(&mut self.view, &mut side.view);
            let outcome = self.run(command, args, now_ms);
            std::mem::swap(&mut self.view, &mut side.view);
            std::mem::swap(&mut self.document, &mut side.document);
            (outcome, side.view.scroll_top, side.view.scroll_row)
        };
        let other = if comparison.focused_right {
            &mut comparison.left
        } else {
            &mut comparison.right
        };
        other.view.scroll_top = scroll_top;
        other.view.scroll_row = scroll_row;
        self.comparison = Some(comparison);
        self.refresh_context();
        outcome
    }

    /// Opens the quick-open prompt over `files`.
    ///
    /// Called by the frontend once it has walked the workspace. Each entry's `id`
    /// is the path to open and its `title` is the displayed text, so typing matches
    /// the file name first and the rest of the path second.
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
    /// The rest keep the order the frontend supplied, which is alphabetical. The
    /// sort is stable, so files not in the recency list keep their relative order
    /// after the recent files.
    fn order_by_recency(&mut self, files: &mut [crate::commands::PaletteEntry]) {
        if self.recent.is_empty() {
            return;
        }
        // Compare normalised paths rather than strings, because the walk and
        // `ctrl+o` can write a path differently (`src/main.rs` and
        // `./src/main.rs`). An unrecognised recent file would lose its position.
        let recent: Vec<PathBuf> = self.recent.iter().map(|path| normalise(path)).collect();
        // Cached, so each path is normalised once per row, not once per comparison.
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
    /// Differs from [`Session::offer_files`] only in the message for an empty
    /// list. An empty file list means the workspace is empty, and an empty result
    /// list means the term was not found. The messages must not be confused.
    /// Offers the decisions already made, so one can be revoked.
    ///
    /// When there are none, shows a message instead of opening an empty list.
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
    /// `what` describes the request as shown to the user: the extension's name
    /// and the capability it requests. The user cannot decide without knowing
    /// which extension is asking.
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
    /// Owned by the core rather than a frontend, because every language deco
    /// supports is compiled in and nothing needs to be read from disk.
    fn offer_languages(&mut self) -> Outcome {
        let mut entries = vec![
            // Automatic detection rather than a fixed language. VS Code lists it
            // first, and it is the only way to undo a manual choice. The detail
            // shows the language detection would select.
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
    /// Save As is usually used to save next to the current file under another
    /// name, so editing the current path is faster than typing a full one.
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
    /// Seeded with the directory, not the file, because the user wants to open a
    /// *different* file and would otherwise have to delete the file name.
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
    /// Everything derived from the path is recomputed: the language, and with it
    /// the lexer and the `[language]` settings. `notes.txt` saved as `notes.toml`
    /// is now a TOML file. The document is clean, because the disk contents match
    /// the buffer.
    ///
    /// A language chosen manually is **kept**. Saving under a new name must not
    /// override the user's explicit choice.
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
    /// Called by the frontend after it has found the installed themes. The
    /// built-in themes are compiled in, but a marketplace theme is a file in an
    /// extension directory, and the core has no filesystem.
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
    /// Colours are read from the theme at render time, so nothing needs to be
    /// invalidated. The next frame uses the new colours.
    ///
    /// The choice lasts for the session. To keep it, set `workbench.colorTheme` in
    /// the settings. deco reads that setting but never writes it, so the message
    /// tells the user which setting to change.
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
    /// Everything that depends on the identifier is rebuilt: the lexer and the
    /// settings, which can be overridden per language. The `editorLangId` context
    /// key is also updated.
    ///
    /// The text is not changed. The language affects only how the text is
    /// interpreted.
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
        // `Some` is a manual choice. `None` means "detect from the file name
        // again", which unpins the language.
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
    /// Each entry's `id` is the document's path, so accepting one uses the same
    /// open-file-at-position path as a search result. The cursor lands in the
    /// correct tab even if the user switched tabs before the server responded.
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
    /// Per document, because the resolved settings are stored there. It is
    /// therefore also per tab: enabling it for one Markdown file does not affect
    /// the code in the next tab. The change is not saved: deco
    /// [does not write settings files](../../../docs/configuration.md).
    ///
    /// Turning it back on restores the `editor.wordWrap` value, including a
    /// `[language]` override, rather than assuming `"on"`. A user who configured
    /// `"bounded"` and pressed the key twice gets `"bounded"` back, not wrapping at
    /// the viewport width.
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
        // Recorded on the document as well as applied, so that a language change,
        // which resolves these settings from scratch, does not undo the toggle.
        self.document.wrap_override = Some(wanted);
        self.document.settings.word_wrap = wanted;

        // The rows in the window have changed, so the scroll anchor and the caret
        // must be revealed again.
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

    /// Reports which options a project search will use, because the prompt has no
    /// room to show them.
    fn report_search_options(&mut self) -> Outcome {
        let describe = |on: bool| if on { "on" } else { "off" };
        Outcome::Message(format!(
            "Search: case {}, whole word {}, regex {}",
            describe(self.search_options.case_sensitive),
            describe(self.search_options.whole_word),
            describe(self.search_options.regex)
        ))
    }

    /// What a project-wide search should look for.
    ///
    /// The selection, the word under the cursor, or the find bar's last query, in
    /// that order, from most to least recently indicated by the user.
    ///
    /// Text from the document is escaped when project search is in regex mode,
    /// so it matches itself. The find bar's query is already a query and is
    /// used as typed.
    pub fn search_seed(&self) -> Option<String> {
        if let Some((text, _)) = self.seed_from_document() {
            if self.search_options.regex {
                return Some(deco_core::search::escape(&text));
            }
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
        // Show the identifier as the detail, because `keybindings.json` refers to
        // it and the title does not show it.
        //
        // Keep a detail the frontend already set. For extension commands it names
        // the extension, which is more useful than the identifier.
        for entry in &mut entries {
            if entry.detail.is_none() {
                entry.detail = Some(entry.id.clone());
            }
        }
        entries
    }

    /// Runs whatever the open prompt was asking for.
    fn accept_prompt(&mut self, now_ms: u64) -> Outcome {
        // Taken rather than borrowed. Running a command needs `&mut self`, and a
        // command may open its own prompt, as `Go to Line` chosen from the
        // palette does.
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
            PromptKind::CommitMessage => {
                let message = prompt.text().trim().to_owned();
                self.commit(message)
            }
            PromptKind::Branches => match prompt.selected() {
                Some(entry) => Outcome::GitCheckoutPreview(entry.id.clone()),
                None => Outcome::Message(format!("no branch matches `{}`", prompt.text())),
            },
            PromptKind::ConfirmCheckout => match prompt.selected() {
                Some(entry) if entry.id == CHECKOUT_CANCEL => {
                    Outcome::Message("stayed on the current branch".to_owned())
                }
                Some(entry) => {
                    Outcome::GitOperation(deco_scm::Operation::Checkout(entry.id.clone()))
                }
                None => Outcome::Message("branch switch cancelled".to_owned()),
            },
            PromptKind::ConfirmDelete => {
                // Only a typed `y` deletes. Enter on an empty input is a common
                // way to dismiss an unread prompt, so it must not delete the
                // file.
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
                // Nothing matched the typed text. Report it, because closing
                // silently would look as if the command had run.
                None => Outcome::Message(format!("no command matches `{}`", prompt.text())),
            },
            PromptKind::SearchQuery => {
                // Not trimmed. A query of spaces is a valid search, and only an
                // *empty* query is rejected.
                let typed = prompt.text();
                if typed.is_empty() {
                    self.replacing_in_files = false;
                    return Outcome::Message("nothing to search for".to_owned());
                }
                // Reported here, before any file is read, so that an invalid
                // regular expression is not shown as "no matches".
                if let Err(error) = deco_core::search::Pattern::new(typed, self.search_options) {
                    self.replacing_in_files = false;
                    return Outcome::Message(error.to_string());
                }
                if self.replacing_in_files {
                    // The second step. The query is stored in the session, not in
                    // the prompt, because the prompt is replaced by the
                    // replacement prompt.
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
                // The replacement may be empty, which deletes every occurrence.
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
                // The prompt opens with the current name, so pressing F2 and
                // then enter submits it unchanged. A rename to the same name
                // would return an empty diff, or, from a server that does not
                // check, an edit per occurrence that marks every file using the
                // name as dirty without changing anything.
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
                // Nothing matched the typed text, so the filter hid both choices.
                // This is treated as no decision rather than as a denial. The
                // extension is still waiting, and the prompt can be opened again.
                None => Outcome::Message("no answer chosen".to_owned()),
            },
            PromptKind::Themes => match prompt.selected() {
                // The identifier is the file to read, or empty for a built-in theme.
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
                    // The frontend reads the file, because the core has no
                    // filesystem.
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
    /// Accepts `12` and VS Code's `line:column` form `12:5`, because the status bar
    /// shows both.
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
            // Include the line count so the message states the valid range.
            return Outcome::Message(format!("line {line} is outside 1-{lines}"));
        }

        // Clamped rather than rejected. A column past the end of the line moves
        // to the end of the line.
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
    /// Matches VS Code, where `editor.find.seedSearchStringFromSelection` is on by
    /// default, because the find bar is usually opened by selecting a word and
    /// pressing `ctrl+f`.
    fn open_find(&mut self, replacing: bool) -> Outcome {
        let primary = *self.view.selections.primary();
        let seed =
            (!primary.is_empty()).then(|| self.document.buffer.text_in_range(primary.range()));
        // The start of the selection, not the cursor, so the selected occurrence
        // becomes the current match instead of the next one.
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
    /// Called after anything that changes the matches, such as a keystroke in the
    /// query or a toggled option. Searching from the origin rather than from the
    /// cursor prevents typing `f`, `o`, `o` from moving down the file one match per
    /// keystroke.
    fn find_query_changed(&mut self) -> Outcome {
        self.find.refresh(&self.document.buffer);
        if let Some(range) = self.find.first_at_or_after(self.find.origin()) {
            self.select_match(range);
        }
        Outcome::Handled
    }

    /// What to report when the find bar's query found nothing: the regex error
    /// when the query did not compile, otherwise that there are no results.
    fn no_find_results(&self) -> Outcome {
        match self.find.error() {
            Some(error) => Outcome::Message(error.to_string()),
            None => Outcome::Message(format!("no results for `{}`", self.find.query())),
        }
    }

    /// `F3` and `shift+F3`: the next or previous match, wrapping.
    fn step_find(&mut self, direction: Direction) -> Outcome {
        // `F3` with an empty query searches for the selection, or for the word
        // under the cursor, so it works without opening find first.
        if self.find.query().is_empty() {
            let Some((seed, range)) = self.seed_from_document() else {
                return Outcome::Message("nothing to search for".to_owned());
            };
            let seed = self.find.literal_query(seed);
            self.find.set_query(seed);
            // Select the seed so that the step below moves past it. Otherwise the
            // search starts from a caret inside the seed word, finds that word,
            // and appears to do nothing.
            self.select_match(range);
        }
        self.find.refresh(&self.document.buffer);
        if self.find.matches().is_empty() {
            return self.no_find_results();
        }

        let primary = *self.view.selections.primary();
        // Search from the end of the selection in the direction of travel, so
        // pressing the key on a match moves to another match instead of finding
        // the same one.
        let found = match direction {
            Direction::Next => self.find.first_at_or_after(primary.end()),
            Direction::Prev => self.find.last_at_or_before(primary.start()),
        };
        let Some(range) = found else {
            return Outcome::Handled;
        };
        self.select_match(range);
        // The bar shows the count when it is open. When it is closed, this
        // message is the only indication of whether the search wrapped.
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
    /// If the selection is not on a match, the press moves to the next match
    /// without replacing anything, as in VS Code. This avoids replacing text the
    /// user has not seen.
    fn replace_one(&mut self, now_ms: u64) -> Outcome {
        if self.find.query().is_empty() {
            return Outcome::Message("nothing to replace".to_owned());
        }
        self.find.refresh(&self.document.buffer);
        if self.find.matches().is_empty() {
            return self.no_find_results();
        }

        let primary = *self.view.selections.primary();
        let current = deco_core::position::Range::new(primary.start(), primary.end());
        if self.find.ordinal(current).is_none() {
            return self.step_find(Direction::Next);
        }

        // In regex mode the replacement depends on the match, so it is taken from
        // the expansion for the selected match.
        let Some(replacement) = self
            .find
            .replacements(&self.document.buffer)
            .into_iter()
            .find(|(range, _)| *range == current)
            .map(|(_, replacement)| replacement)
        else {
            return self.step_find(Direction::Next);
        };
        let after = self.replace_range(current, &replacement, now_ms);
        // The document changed, so rebuild the match list before using it.
        self.find.refresh(&self.document.buffer);
        if let Some(range) = self.find.first_at_or_after(after) {
            self.select_match(range);
        }
        Outcome::Handled
    }

    /// `ctrl+alt+enter`: replaces every match, in one undo step.
    ///
    /// One undo step for one user action, so a single `ctrl+z` reverts all
    /// replacements.
    fn replace_all(&mut self, now_ms: u64) -> Outcome {
        if self.find.query().is_empty() {
            return Outcome::Message("nothing to replace".to_owned());
        }
        self.find.refresh(&self.document.buffer);
        if self.find.matches().is_empty() {
            return self.no_find_results();
        }

        // Skip matches that already equal their replacement, so replacing `foo`
        // with `foo` does not dirty the file or add an undo step. A match can
        // differ from the query, because a case-insensitive search for `foo` also
        // finds `FOO`.
        let edits: Vec<deco_lsp::TextEdit> = self
            .find
            .replacements(&self.document.buffer)
            .into_iter()
            .filter(|(range, replacement)| {
                self.document.buffer.text_in_range(*range) != *replacement
            })
            .map(|(range, new_text)| deco_lsp::TextEdit { range, new_text })
            .collect();
        if edits.is_empty() {
            return Outcome::Message(if self.find.options().regex {
                "every match already reads as its replacement".to_owned()
            } else {
                format!("every match already reads `{}`", self.find.replace())
            });
        }

        // `TextEdit` is a range and a string, and `apply_edits` already turns a
        // batch of them into one transaction with one undo step. Reuse it instead
        // of duplicating that logic.
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
    /// The range is needed as well as the text. It is the match the cursor is
    /// already on, and `F3` must move past it.
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
    /// Wraps around, as in VS Code. Pressing F8 on the last error returns to the
    /// first.
    fn goto_marker(&mut self, direction: Direction) -> Outcome {
        if self.diagnostics.is_empty() {
            return Outcome::Message("no problems in this file".into());
        }

        // Sorted by position rather than kept in publication order. Servers
        // publish in the order analysis finished, and "next" must mean next in
        // the file.
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

        // Clamped because a diagnostic can refer to text that no longer exists.
        // The user may have deleted the lines before the server updated, and an
        // unclamped position would panic or scroll past the end.
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
    /// Lets a frontend apply an edit it computed itself, such as an accepted
    /// completion or a formatting result, without a command. It uses the same
    /// transaction and history code as every other edit, so the result is one
    /// undo step and the dirty flag is correct.
    ///
    /// `Discrete` rather than typed. Accepting a completion is one action. Merging
    /// it with the characters typed just before would make one undo remove the
    /// typed word as well as the completion.
    pub fn replace_range(
        &mut self,
        range: deco_core::position::Range,
        text: &str,
        now_ms: u64,
    ) -> deco_core::position::Position {
        use deco_core::{Change, EditKind, Selection, SelectionSet, Transaction};

        self.document.snippet = None;

        let before = self.view.selections.clone();
        // Clamped because the range may have been computed against text the user
        // has since changed, such as a completion returned while typing continued.
        let range = deco_core::position::Range::new(
            self.document.buffer.clamp_position(range.start),
            self.document.buffer.clamp_position(range.end),
        );

        let transaction = Transaction::single(Change::replace(range, text.to_owned()));
        let inverse = self.document.apply(&transaction);

        // The end of the inserted text, where the caret goes after an insertion.
        // Computed from the text rather than by searching the buffer, so it is
        // correct when the text contains newlines.
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

    /// Expands supported variables using the active document before insertion.
    /// No filesystem, environment or clipboard access is performed.
    pub fn expand_snippet(&self, source: &str) -> Option<deco_lsp::snippet::Snippet> {
        let selection = self.view.selections.primary();
        let cursor = self.document.buffer.clamp_position(selection.active);
        deco_lsp::snippet::Snippet::parse_with_variables(source, |name| {
            let path = self.document.path.as_deref();
            Some(match name {
                "TM_FILENAME" => path
                    .and_then(std::path::Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "TM_FILENAME_BASE" => path
                    .and_then(std::path::Path::file_stem)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "TM_LINE_INDEX" => cursor.line.to_string(),
                "TM_LINE_NUMBER" => (u64::from(cursor.line) + 1).to_string(),
                "TM_CURRENT_LINE" => self
                    .document
                    .buffer
                    .line_content(cursor.line as usize)
                    .map(|line| line.to_string())
                    .unwrap_or_default(),
                "TM_CURRENT_WORD" => deco_core::search::word_at(&self.document.buffer, cursor)
                    .map(|range| self.document.buffer.text_in_range(range))
                    .unwrap_or_default(),
                "TM_SELECTED_TEXT" => self.document.buffer.text_in_range(selection.range()),
                _ => return None,
            })
        })
    }

    /// Inserts a parsed completion and selects the first numeric placeholder.
    /// Uses the ordinary replacement transaction, including its undo boundary.
    pub fn insert_snippet(
        &mut self,
        range: deco_core::Range,
        snippet: &deco_lsp::snippet::Snippet,
        now_ms: u64,
    ) {
        use deco_core::{Position, Range};
        if self.comparison.is_some() || self.focus != Focus::Editor {
            return;
        }
        let start = self.document.buffer.clamp_position(range.start);
        self.replace_range(range, &snippet.text, now_ms);
        let offset = |position: Position| {
            Position::new(
                start.line + position.line,
                position.character
                    + if position.line == 0 {
                        start.character
                    } else {
                        0
                    },
            )
        };
        if snippet.stops.is_empty() {
            return;
        }
        self.document.snippet = Some(crate::snippet::ActiveSnippet {
            stops: snippet
                .stops
                .iter()
                .map(|r| Range::new(offset(r.start), offset(r.end)))
                .collect(),
            current: 0,
        });
        self.select_snippet_stop();
    }

    fn move_snippet(&mut self, previous: bool) -> Outcome {
        if self.focus != Focus::Editor {
            return Outcome::Handled;
        }
        if let Some(snippet) = self.document.snippet.as_mut() {
            if previous {
                snippet.current = snippet.current.saturating_sub(1);
            } else {
                snippet.current = (snippet.current + 1).min(snippet.stops.len() - 1);
            }
            self.select_snippet_stop();
        }
        Outcome::Handled
    }

    fn select_snippet_stop(&mut self) {
        use deco_core::{Selection, SelectionSet};
        if let Some(snippet) = &self.document.snippet {
            let range = snippet.stops[snippet.current];
            self.view.selections = SelectionSet::single(Selection::new(range.start, range.end));
            if snippet.current + 1 == snippet.stops.len() {
                self.document.snippet = None;
            }
            self.view
                .reveal_cursor(&self.document.buffer, &self.document.settings);
        }
        self.refresh_context();
    }

    /// Applies a batch of server-computed replacements as one undo step.
    ///
    /// Every range refers to the document as the server saw it, and the protocol
    /// does not define the order of the edits. Applying them front to back would
    /// corrupt the file, because each edit shifts the positions after it.
    /// [`deco_core::Transaction`] sorts them and applies them back to front, so
    /// they are passed as one batch rather than applied in a loop.
    ///
    /// Returns how many edits were applied, or an error with the reason when none
    /// could be applied.
    ///
    /// The cursor stays where it was, clamped into the new text, so formatting
    /// does not move the caret to the end of the file.
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
    /// `None` when no tab holds it, which tells the caller to change the file on
    /// disk instead. Edits to an open document must go to its *buffer*, not its
    /// file. Otherwise saving the document would overwrite the edit on disk and
    /// the edit would be lost.
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
        // and this document is in the background.
        Some(applied)
    }

    /// Offers the code actions a language server returned for the selection.
    ///
    /// Called by the frontend when the response arrives, like
    /// [`Session::offer_symbols`]. Each entry's `id` is the frontend's own handle
    /// for the action, because the frontend stores the actions.
    ///
    /// An empty list is reported as a message rather than opening an empty
    /// prompt. `ctrl+.` on a line without problems is a normal action.
    pub fn offer_code_actions(&mut self, actions: Vec<crate::commands::PaletteEntry>) {
        if actions.is_empty() {
            self.status = Some("no code actions here".to_owned());
            return;
        }
        self.prompt = Some(Prompt::list(PromptKind::CodeActions, actions));
        self.refresh_context();
    }

    /// Prompts for a new name for the symbol under the cursor.
    ///
    /// Opened by the frontend rather than by `editor.action.rename` in the core,
    /// because whether a rename is possible depends on the language server, which
    /// the frontend owns. The prompt is not shown if the server cannot rename.
    ///
    /// Seeded with the current name, fully selected, as VS Code's rename box
    /// opens. Typing replaces the name, and `end` keeps it to add a suffix.
    pub fn offer_rename(&mut self) -> Outcome {
        let Some((name, _)) = self.seed_from_document() else {
            return Outcome::Message("put the cursor on a name to rename it".to_owned());
        };
        self.prompt = Some(Prompt::seeded(PromptKind::Rename, name));
        self.refresh_context();
        Outcome::Handled
    }

    /// Resolves a server's [`deco_lsp::WorkspaceEdit`] against the open documents.
    ///
    /// Nothing is changed. The returned plan lists the files that still need to
    /// be read (see [`crate::workspace::Plan::missing`]), and
    /// [`Session::apply_workspace_edit`] applies it.
    ///
    /// `resolve` converts a server URI into a path on the machine holding the
    /// files, and `version_of` returns the version last sent to that server for a
    /// path. Both are callbacks because they belong to the LSP client, which is in
    /// a frontend. The rules for an unresolvable URI and a mismatched version are
    /// defined here, so every frontend uses the same rules.
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
    /// The result goes to [`Session::apply_workspace_edit`] like any other plan,
    /// so a workspace-wide replace is one undoable action, and files not open in
    /// a tab are opened rather than written. These are the same rules as for a
    /// rename.
    ///
    /// # The matches are found again here, not carried over
    ///
    /// The caller already searched these files, but its positions cannot be
    /// reused, for two reasons. First, a search result gives where a match
    /// *started*, and replacing needs where it ended. Deriving the end from the
    /// needle's length assumes that case folding preserves length, which
    /// case-insensitive matching does not guarantee. Second, a file the search
    /// read from disk may be open with unsaved changes. The buffer is then the
    /// relevant text, and positions from disk do not match it.
    ///
    /// Each file is therefore searched again, against the buffer when a tab holds
    /// one, with the same [`deco_core::search::Pattern`] the find bar uses. The
    /// count reported afterwards is therefore the number of replacements, not
    /// the number of earlier search results.
    ///
    /// `read` supplies the text of a file not open in a tab. It is provided by
    /// the caller because reading is I/O, which happens on another machine in a
    /// remote session.
    pub fn plan_replacements(
        &self,
        paths: &[PathBuf],
        needle: &str,
        replacement: &str,
        options: deco_core::search::SearchOptions,
        mut read: impl FnMut(&Path) -> Result<String, String>,
    ) -> Result<crate::workspace::Plan, crate::workspace::WorkspaceError> {
        let pattern = deco_core::search::Pattern::new(needle, options)?;
        let mut documents = Vec::with_capacity(paths.len());
        for path in paths {
            let open = self.document_at_path(path);
            // Borrowed from the tab, or built from the caller's text. An open
            // document already has a buffer, so one is built only for files that
            // are not open.
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

            // In regex mode each match gets its own replacement, with capture
            // references expanded against that match.
            let edits: Vec<deco_lsp::TextEdit> = pattern
                .replacements(&buffer, replacement)
                .into_iter()
                .map(|(range, new_text)| deco_lsp::TextEdit { range, new_text })
                .collect();

            // Skip a file with no remaining matches (for example, because the
            // matched text has since changed) instead of opening it.
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
    /// Every transaction is built before any is applied. Building is the only
    /// step that can reject an edit (for example, overlapping ranges), so a
    /// rejection leaves every buffer unchanged. Changes are applied only after
    /// all transactions are built, and that step cannot fail.
    ///
    /// Files not open in a tab are opened as background tabs from the text
    /// supplied by [`crate::workspace::Plan::with_contents`]. They are opened
    /// *after* the same check, so a rejection does not leave new tabs either.
    ///
    /// Every document records its step under one shared group, which
    /// [`Session::run`] uses to undo the whole change at once.
    pub fn apply_workspace_edit(
        &mut self,
        mut plan: crate::workspace::Plan,
        now_ms: u64,
    ) -> Result<crate::workspace::Applied, crate::workspace::WorkspaceError> {
        use crate::workspace::WorkspaceError;

        // Documents this edit adds. They are built here but not yet opened, so a
        // rejection below leaves the session's tabs unchanged.
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

        // Nothing after this point can fail.
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
                // Opened even when it has nothing to change. The server named the
                // file, and a tab that always appears is more predictable than one
                // that appears only sometimes. `edits` below counts the actual
                // changes.
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

    /// Returns the next group number and advances the counter.
    fn take_group(&mut self) -> deco_core::Group {
        let group = deco_core::Group(self.next_group);
        // Saturating rather than wrapping, because reusing a number would join two
        // unrelated changes into one undo step. Exhausting a `u64` at one group
        // per refactor does not happen in practice.
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
    /// Called instead of the ordinary undo when the top step of the active
    /// document's history is tagged. Every other document whose *next* step has
    /// the same tag is undone with it. A file edited manually since the rename
    /// keeps that edit. Its part of the rename stays in its own history and is
    /// undone after the later edits are undone.
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
            // `Document::apply`, so all caches must be invalidated.
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
    /// The user's settings, resolved for the open document's language, so a
    /// server formats with the project's indentation rather than its own
    /// defaults.
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
    /// The loop and its reporting are here so both frontends behave identically,
    /// and so the behaviour can be tested with an in-memory `write`. The core
    /// still performs no I/O: it passes a path and the bytes to `write`, and the
    /// caller performs the write and returns the result.
    ///
    /// Each write result is handled individually. A failed write leaves that
    /// document dirty instead of marking the whole batch saved, so a tab never
    /// appears saved when it is not. A failure does not stop the remaining
    /// writes.
    ///
    /// A dirty *untitled* document is counted and skipped. It has no filename,
    /// and deco does not choose one for the user.
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
            // The reasons go to the problem list, because the status bar has one
            // line and cannot show several failures.
            self.problems.extend(failures);
        }
        Outcome::Message(report)
    }

    /// Every document with unsaved changes and a filename, in tab order.
    ///
    /// For `workbench.action.files.saveAll`. Each pair is the path to write and
    /// the exact bytes to write, resolved through that document's own settings.
    /// A tab holding a `.md` file uses its own `files.insertFinalNewline`, not
    /// the active document's.
    ///
    /// A dirty *untitled* document is left out, because it has no filename and
    /// deco does not choose one. [`Session::unsaved_untitled`] counts those so the
    /// frontend can report them.
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

    /// Every path held by an open tab, in tab order.
    ///
    /// Used after a branch switch: Git changed files outside the editor, and
    /// every clean buffer must be brought to the same revision before it can be
    /// edited or saved again.
    pub fn open_paths(&self) -> Vec<PathBuf> {
        self.documents()
            .filter_map(|document| document.path.clone())
            .collect()
    }

    /// Replaces the clean open buffer at `path` with its post-checkout text.
    ///
    /// The old undo history belongs to another branch and is dropped. Explicit
    /// language and wrapping choices survive because they belong to the tab,
    /// not to either revision of the file.
    pub fn reload_open(&mut self, path: &Path, text: &str) -> bool {
        let Some(index) = self.tab_of(path) else {
            return false;
        };
        let settings = self.settings.clone();
        let wanted = normalise(path);
        for (document, view) in self.documents_and_views() {
            if document.path.as_deref().map(normalise) != Some(wanted.clone()) {
                continue;
            }
            let pinned = document.language_pinned;
            let language = document.language_id.clone();
            let wrap = document.wrap_override;
            let resolved = EditorSettings::resolve(&settings, language.as_deref());
            let mut replacement = Document::from_file(path.to_path_buf(), text, resolved);
            if pinned {
                replacement.language_id = language;
                replacement.language_pinned = true;
                replacement.syntax = deco_syntax::Syntax::new(replacement.language());
            }
            replacement.wrap_override = wrap;
            replacement.apply_overrides();
            let cursor = replacement
                .buffer
                .clamp_position(view.selections.primary().active);
            view.selections = deco_core::SelectionSet::caret(cursor);
            view.reveal_cursor(&replacement.buffer, &replacement.settings);
            *document = replacement;
            break;
        }
        self.clear_analysis_of_tab(index);
        self.committed.clear();
        self.relayout();
        self.refresh_context();
        true
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
    /// Per path rather than for all documents, so a failed write leaves that
    /// document dirty and it does not appear saved.
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
        // A write is the most common reason for `git status` to change. This
        // handles the *active* document. `mark_saved_at` calls this for the
        // active document and sets the flag itself for the others.
        self.scm_changed();
        self.refresh_context();
    }

    /// Tells the session how large the text area is.
    ///
    /// `width` is the whole area, including gutters and separators. The session
    /// uses [`crate::layout`] to compute how many columns each group has for text,
    /// which determines where wrapped lines break. Computing this here rather than
    /// in the frontend keeps the wrap width and the drawn width consistent.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.lay_out(width, height);
        // After a size change the caret may be outside the window.
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
        // Move focus to the editor when hiding the focused region.
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

    /// Re-divides the window and reports the result when needed.
    ///
    /// The only reported case is showing a region that does not fit. Without a
    /// message, the key would appear to be ignored. The state is still kept, so
    /// widening the window shows the region.
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

    /// Opens one of the tree's prompts, or reports why it cannot be opened.
    fn open_tree_prompt(&mut self, kind: PromptKind) -> Outcome {
        if self.explorer.is_none() {
            return Outcome::Message("there is no workspace open".to_owned());
        }
        if kind == PromptKind::ConfirmDelete {
            let Some(row) = self.explorer.as_ref().and_then(crate::Explorer::selection) else {
                return Outcome::Message(crate::files::FileError::NoSelection.to_string());
            };
            // Include the name in the question so the user can see what will be
            // deleted.
            let what = if row.is_dir {
                format!("{} and everything in it", row.name)
            } else {
                row.name
            };
            // The input opens empty and the name is shown in the status line.
            // The input holds only the answer.
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
    /// A new file goes in the selected row when it is a directory, and in the
    /// selected row's parent when it is a file, as in VS Code. "New file"
    /// therefore creates the file next to the selected one.
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
    /// Separate from opening the prompt. This step validates the typed name.
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
        // Existence is checked against the tree's listing, which is sufficient
        // to reject a name without querying the filesystem. A race with another
        // program is detected by the frontend, which reports the failure and
        // removes the undo entry.
        if explorer.rows().iter().any(|row| row.path == path) {
            return Outcome::Message(crate::files::FileError::Exists(name.to_owned()).to_string());
        }
        // The same check as for renaming. A tab can hold a path the tree does not
        // show, when another program deleted the file and the session has not
        // detected it. Creating the file would succeed on disk, but
        // `Session::open` would then switch to the *old* buffer instead of the
        // empty file, and saving would write the old contents back.
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
        // Checked before trimming. The prompt opens with the current name, so
        // accepting it unchanged must do nothing. On a filesystem that allows a
        // name like `" report "`, trimming first would turn enter into an
        // unintended rename.
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
            // Not an error, but skipped. Renaming a file to its current name
            // would still access the disk and invalidate the listing.
            return Outcome::Handled;
        }
        if let Err(error) = crate::files::check_inside(&root, &to) {
            return Outcome::Message(error.to_string());
        }
        if explorer.rows().iter().any(|other| other.path == to) {
            return Outcome::Message(crate::files::FileError::Exists(name.to_owned()).to_string());
        }
        // A tab can hold a path the tree does not show: a file deleted by
        // another program stays open until the session detects it. Renaming onto
        // it would leave two buffers for one path, and the last save would
        // overwrite the other. `Session::open` prevents the same situation.
        //
        // Checks paths at *or under* the target, because renaming a directory
        // moves its whole subtree. With `/w/b/x` open and `/w/a` renamed to
        // `/w/b`, an exact comparison against `/w/b` passes, and `/w/a/x` would
        // then be retargeted onto `/w/b/x`, which another tab already holds.
        // `open_paths_under` handles both cases, because a file path matches only
        // itself.
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
        // The root is not a row, so this should not delete the workspace. It is
        // checked anyway, because an error would delete the whole project.
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

    /// Detaches every tab holding a path under `gone`, and returns how many.
    ///
    /// The buffers are kept and their paths are cleared. The text still belongs
    /// to the user, who decides where to save it. A tab still pointing at a
    /// deleted path would recreate the file on the next save, or fail if its
    /// directory was also deleted.
    ///
    /// Public because a *failed* recursive delete also needs it.
    /// `remove_dir_all` can remove part of a tree and then stop, and the removed
    /// part is gone as if the delete had succeeded.
    pub fn detach_tabs_under(&mut self, gone: &Path) -> usize {
        let gone = normalise(gone);
        let affected = self.tabs_under(&gone);
        let held: Vec<PathBuf> = affected
            .iter()
            .filter_map(|index| self.path_of_tab(*index))
            .collect();
        // Record the paths first, while the tabs still have them. The server has
        // these documents open under URIs that no longer exist.
        self.closed_documents.extend(held);

        let mut detached = 0usize;
        for index in affected {
            let Some(document) = self.document_at_index_mut(index) else {
                continue;
            };
            document.path = None;
            document.dirty = true;
            // Diagnostics and semantic tokens describe a file that no longer
            // exists. Every code path that refreshes them returns early when the
            // path is `None`, so they would otherwise stay on screen for the
            // lifetime of the buffer.
            self.clear_analysis_of_tab(index);
            detached += 1;
        }
        detached
    }

    /// The paths of every open tab holding something under `path`.
    ///
    /// For a caller with filesystem access that needs to check each path. A
    /// recursive delete that failed part way has removed some of these paths,
    /// and only the filesystem can tell which.
    pub fn open_paths_under(&self, path: &Path) -> Vec<PathBuf> {
        let under = normalise(path);
        self.tabs_under(&under)
            .into_iter()
            .filter_map(|index| self.path_of_tab(index))
            .collect()
    }

    /// Every path the tree knows about at or under `path`.
    ///
    /// For a caller with filesystem access that needs to check whether a delete
    /// that reported failure removed anything.
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
    /// For a path that no longer refers to the same directory. Invalidating would
    /// keep it in the map to be read again, and it would then appear as an empty
    /// folder.
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

    /// Clears the tree's undo history.
    ///
    /// For a recursive delete that may have partly completed. An irreversible
    /// change happened, so the older inverses no longer describe a valid state.
    /// A completed delete clears the history in the same way. This is the
    /// equivalent for a delete that reported failure.
    pub fn clear_file_undo(&mut self) {
        self.explorer_undo.clear();
        self.pending_undo = None;
    }

    /// Clears the analysis attached to one tab.
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

    /// Returns and clears the files the language server should be told are
    /// closed.
    ///
    /// Filled when a delete detaches a tab. The server has the file open under a
    /// URI that no longer exists, and only a frontend can notify it. The list is
    /// drained, so one delete produces one `didClose` per file.
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
    /// A file matches itself, and a directory matches its whole subtree. Both
    /// sides are normalised for the reason given in [`Session::tab_of`]: the same
    /// file can be written two ways, and a missed tab would keep pointing at a
    /// path that no longer exists.
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
    /// The buffer, its history and its unsaved changes are kept. Only the path
    /// changes. Settings are re-resolved, so renaming `notes.txt` to `notes.md`
    /// highlights it as Markdown. A manually chosen language takes precedence
    /// over the extension, the same rule [`Session::rename_to`] follows for Save
    /// As.
    fn retarget_tab(&mut self, index: usize, to: PathBuf) {
        let Some(document) = self.document_at_index_mut(index) else {
            // No tab has this index, so there is nothing to retarget.
            return;
        };

        // Set when the inferred language changed, to whether there was a
        // language before. A rename *away* from a recognised extension must
        // discard the server's results and close the document on the server.
        let mut language_changed: Option<bool> = None;

        // A manually chosen language takes precedence over the new extension, as
        // in Save As.
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
                // Rebuild the lexer as `set_language` does. Otherwise a file
                // renamed to another language would report the new language but
                // keep the old highlighting.
                document.syntax = deco_syntax::Syntax::new(document.language());
                language_changed = Some(had_language);
            }
        }
        let language = document.language().map(str::to_owned);

        // Re-resolve for every tab, not only the active one. Switching to a tab
        // lays it out again but does not re-resolve its settings, so a background
        // tab renamed from `notes.txt` to `notes.md` would otherwise stay plain
        // text for the rest of the session.
        let settings = EditorSettings::resolve(&self.settings, language.as_deref());
        if let Some(document) = self.document_at_index_mut(index) {
            document.settings = settings;
            document.apply_overrides();
        }
        // Semantic tokens and diagnostics came from a server that knew the old
        // path and language. When the rename removes the language, no server
        // replaces them, because `attach` returns early for a document with no
        // language. The old tokens would then override the rebuilt lexer
        // permanently.
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
    /// An operation with no inverse (a delete) does *not* clear the stack here.
    /// Recording happens before the frontend attempts the operation, so a delete
    /// the filesystem rejects would otherwise discard all earlier undo entries.
    /// The stack is cleared in [`Session::file_operation_done`], after the delete
    /// has succeeded.
    fn record_file_operation(&mut self, operation: &crate::files::Operation) {
        if let Some(inverse) = operation.inverse() {
            self.explorer_undo.push(inverse);
        }
    }

    /// Attaches the moved file's stamp to its pending undo entry.
    ///
    /// Called by the frontend after it performs a rename, because only the
    /// frontend can inspect the file. Without a stamp, undoing a rename relies
    /// on the path alone, and another program may have replaced the renamed
    /// file with a different one at that path.
    pub fn stamp_last_undo(&mut self, stamp: crate::files::Stamp) {
        // Skip while an undo is in progress. That operation was *popped* into
        // `pending_undo`, so the top of the stack is the previous entry.
        // Stamping it would describe the wrong file, and the next `ctrl+z` would
        // reject a valid rename undo.
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

    /// `undo` while the tree has the keyboard: reverts the last operation.
    ///
    /// The undone operation's inverse is **not** pushed back. Pushing it would
    /// make `ctrl+z` a toggle: a second press would redo the rename instead of
    /// undoing the operation before it, so older entries would be unreachable.
    /// Repeated presses move back through the history. The tree has no redo.
    fn undo_file_operation(&mut self) -> Outcome {
        let Some(operation) = self.explorer_undo.last().cloned() else {
            return Outcome::Message("nothing in the tree to undo".to_owned());
        };
        // The same collision check as `rename_in_tree`. The entry was recorded
        // when the destination was free, but it may no longer be. For example:
        // rename `a` to `b`, open a new `a`, and let another program remove it.
        // Undoing would move `b` back onto the path that tab still holds, giving
        // two buffers for one file.
        //
        // Checked before popping, so a rejected undo stays on the stack and can
        // be retried after the tab is closed.
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

    /// Removes an operation's undo entry after the filesystem rejected it.
    ///
    /// The core records the entry before the frontend attempts the operation. If
    /// the operation fails, the entry is removed, so `ctrl+z` does not undo an
    /// operation that never happened.
    pub fn file_operation_failed(&mut self, operation: &crate::files::Operation, reason: &str) {
        match self.pending_undo.take() {
            // A failed undo goes back on the stack, so it can be retried after
            // the cause is resolved.
            Some(pending) if &pending == operation => self.explorer_undo.push(pending),
            _ => {
                if let Some(inverse) = operation.inverse() {
                    if self.explorer_undo.last() == Some(&inverse) {
                        self.explorer_undo.pop();
                    }
                }
            }
        }
        // The tree's state for the destination path is wrong. The operation was
        // attempted because the tree showed the name as free, so anything the
        // tree remembers at that path is a directory that no longer exists. It
        // is forgotten rather than invalidated, because the expansion state is
        // also stale: the path now refers to a different entry.
        //
        // The success path does the same for the same reason. The most common
        // case is a create that failed because another program took the name,
        // so it must also be handled here.
        if let (Some(explorer), Some(path)) = (self.explorer.as_mut(), operation.arriving()) {
            explorer.forget_under(path);
        }
        self.status = Some(format!("could not {}: {reason}", operation.describe()));
    }

    /// Retargets an open tab after its file moved, and re-reads the tree.
    ///
    /// Called by the frontend after the rename has succeeded. Renaming goes
    /// through the session, not only the tree, so that the tab and the file move
    /// together.
    pub fn file_operation_done(&mut self, operation: &crate::files::Operation) {
        self.pending_undo = None;
        // Focus moves to the editor for a created file, because
        // `Outcome::FileOperation` specifies that a created file is opened, and
        // keys typed into the tree would be ignored. Done here rather than when
        // the create was requested, so a failed create leaves focus in the tree
        // for a retry.
        if matches!(operation, crate::files::Operation::CreateFile(_)) {
            self.focus = Focus::Editor;
        }
        // A tab pointing at a deleted path would recreate the file on the next
        // save, or fail if its directory was also deleted. The buffer is kept
        // and its path cleared, so the user decides where to save the text.
        let mut detached_tabs = 0usize;
        if let crate::files::Operation::Delete { path, .. }
        | crate::files::Operation::DeleteIfEmpty { path, .. } = operation
        {
            detached_tabs = self.detach_tabs_under(path);
        }

        // Entries older than a delete cannot be undone either. Running an older
        // inverse would restore a file without the deleted one, a state that
        // never existed. Cleared here rather than when the delete was recorded,
        // so a failed delete keeps the history.
        if matches!(operation, crate::files::Operation::Delete { .. }) {
            self.explorer_undo.clear();
        }
        if let crate::files::Operation::Rename { from, to, .. } = operation {
            // Every tab *under* `from`, not only one whose path equals it.
            // Renaming a directory moves its whole subtree on disk, and a tab
            // still pointing into the old tree would save to a path that no
            // longer exists, recreating the old directory or failing.
            let from = normalise(from);
            for index in self.tabs_under(&from) {
                let path = self
                    .path_of_tab(index)
                    .expect("tabs_under only returns tabs with paths");
                // Strip from the *normalised* path, which `tabs_under` matched.
                // With the raw path, a tab written as `/w/./src/a.rs` would be
                // selected but fail to strip, and would keep pointing into the
                // moved directory.
                let moved = match normalise(&path).strip_prefix(&from) {
                    Ok(rest) if rest.as_os_str().is_empty() => to.clone(),
                    Ok(rest) => to.join(rest),
                    Err(_) => continue,
                };
                self.retarget_tab(index, moved);
            }
        }
        // A created or removed file changes the `git status` output.
        self.scm_changed();
        // Cached data for the affected paths now describes a file that no
        // longer exists, or, at the destination, a different file. Both sides
        // are cleared, because a rename empties one path and fills the other.
        for path in [operation.arriving(), moved_from(operation)]
            .into_iter()
            .flatten()
        {
            self.forget_committed_under(path);
        }
        if let (Some(explorer), Some(parent)) = (self.explorer.as_mut(), operation.parent()) {
            explorer.invalidate(parent);
            // Forget the tree's state for the moved or deleted entry. The parent
            // alone is not enough: a deleted directory keeps its cached listing
            // and expansion, so a new directory with the same name would show
            // the old rows, already expanded.
            //
            // This handles only the path that was *left*. The path that
            // receives an entry is handled below, once for all operations.
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
            // The selection moves to the created or moved entry. Otherwise the
            // selection stays on the previously highlighted row, and the next
            // key (`F2`, `delete`) would act on the wrong file.
            //
            // `reveal` is used rather than a direct selection because the row
            // does not exist yet: the directory has just been invalidated and is
            // read again on the next turn. `reveal` sets the selection when the
            // listing arrives.
            // Wherever an entry arrives at a path, first forget what the tree
            // remembered there.
            //
            // A directory removed outside deco keeps its listing and expansion
            // when a later operation refreshes only the parent: the row is
            // removed, but the cached state is not. Whatever later takes that
            // name inherits the state: an empty folder shows the old children, a
            // renamed directory shows another directory's children, or a file
            // shows a subtree. In each case the tree does not request a listing,
            // because it has a cached one.
            //
            // The rule is defined once, on the operation, because it applies to
            // a created folder, a rename destination, a created file, and the
            // same three cases when the operation fails. See
            // [`crate::files::Operation::arriving`], which both this and
            // [`Session::file_operation_failed`] use.
            if let Some(path) = operation.arriving() {
                explorer.forget_under(path);
            }

            match operation {
                crate::files::Operation::CreateFile(path)
                | crate::files::Operation::CreateFolder(path) => explorer.reveal(path),
                // After the forget above, so the source's listings and
                // expansion move to a path with no remaining state.
                crate::files::Operation::Rename { from, to, .. } => {
                    explorer.rekey_under(from, to);
                    explorer.reveal(to);
                }
                // Nothing to select, because the entry is gone. The clamp in
                // `fill` moves the selection to a row that still exists.
                crate::files::Operation::Delete { .. }
                | crate::files::Operation::DeleteIfEmpty { .. } => {}
            }
        }
        // If the selected file was deleted or renamed, re-reading the directory
        // updates the rows, and the clamp in `fill` keeps the selection on a row
        // that exists.
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
    /// The frontend determines the root from the working directory and the path
    /// deco was started with. When called again with a different root, the tree
    /// is recreated rather than merged, because expansion state from one
    /// workspace does not apply to another.
    pub fn set_workspace_root(&mut self, root: impl Into<std::path::PathBuf>) {
        self.explorer = Some(crate::Explorer::new(root));
        // The stack holds absolute paths in the previous workspace. Undoing one
        // in another workspace would move or delete a file that is not shown.
        self.explorer_undo.clear();
        self.refresh_context();
    }

    /// A `list.*` key, with the source-control view showing.
    ///
    /// Expanding and collapsing do nothing: the list is flat, and its headings
    /// are drawn from the rows rather than being rows. They return `Handled`
    /// rather than an error, because the key is bound to `list.*` for every list
    /// and an error would report a working binding as unknown.
    fn source_control_key(&mut self, command: &str) -> Outcome {
        match command {
            "list.focusDown" => self.source_control.select_next(),
            "list.focusUp" => self.source_control.select_previous(),
            "list.focusFirst" => self.source_control.select_first(),
            "list.focusLast" => self.source_control.select_last(),
            "list.expand" | "list.collapse" => {}
            "list.select" => {
                let Some(row) = self.source_control.selection() else {
                    return Outcome::Handled;
                };
                if row.group == crate::scm::Group::Conflicts {
                    return Outcome::Message(
                        "resolve the merge conflict before opening its diff".to_owned(),
                    );
                }
                let kind = match row.group {
                    crate::scm::Group::Staged => deco_scm::ComparisonKind::Staged,
                    crate::scm::Group::Changes | crate::scm::Group::Untracked => {
                        deco_scm::ComparisonKind::WorkingTree
                    }
                    crate::scm::Group::Conflicts => unreachable!("refused above"),
                };
                let request = deco_scm::ComparisonRequest {
                    path: row.path.clone(),
                    original: (row.group == crate::scm::Group::Staged)
                        .then(|| row.original.clone())
                        .flatten(),
                    kind,
                };
                self.focus = Focus::Editor;
                self.refresh_context();
                return Outcome::GitComparison(request);
            }
            _ => {}
        }
        // Keep the selection visible, as the tree does. Done here because
        // `explorer_key` delegates to this function before its own call.
        let height = self.side_bar_rows();
        self.source_control.scroll_into_view(height);
        self.refresh_context();
        Outcome::Handled
    }

    /// `git.stage`: add the selected file's working-tree state to the index.
    ///
    /// Rejects a row that is already staged instead of running `git add`. The
    /// command would succeed without effect, and the status message would
    /// incorrectly report that something was staged.
    fn stage_selected(&mut self) -> Outcome {
        let Some(row) = self.source_control.selection() else {
            return Outcome::Message("nothing is selected in source control".to_owned());
        };
        if row.group == crate::scm::Group::Staged {
            return Outcome::Message(format!("{} is already staged", row.name()));
        }
        Outcome::GitOperation(deco_scm::Operation::Stage(row.path.clone()))
    }

    /// `git.stageAll`: everything git reported.
    fn stage_all(&mut self) -> Outcome {
        // Report when there is nothing left to stage.
        let unstaged = self
            .source_control
            .rows()
            .iter()
            .any(|row| row.group != crate::scm::Group::Staged);
        if !unstaged {
            return Outcome::Message("there is nothing left to stage".to_owned());
        }
        Outcome::GitOperation(deco_scm::Operation::StageAll)
    }

    /// `git.unstage`: take the selected file back out of the index.
    fn unstage_selected(&mut self) -> Outcome {
        let Some(row) = self.source_control.selection() else {
            return Outcome::Message("nothing is selected in source control".to_owned());
        };
        if row.group != crate::scm::Group::Staged {
            return Outcome::Message(format!("{} is not staged", row.name()));
        }
        Outcome::GitOperation(deco_scm::Operation::Unstage {
            path: row.path.clone(),
            // A staged rename is two entries in the index, and unstaging one
            // of them leaves a commit that still deletes the other. A copy is
            // different: its source remains an independent index entry, and
            // resetting it here would unstage changes the user did not select.
            original: (row.change == deco_scm::Change::Renamed)
                .then(|| row.original.clone())
                .flatten(),
        })
    }

    /// `git.commit`: ask for a message.
    ///
    /// Rejected before the prompt opens when nothing is staged, rather than after
    /// the message has been typed, so a typed message is not lost.
    fn ask_commit_message(&mut self) -> Outcome {
        if self.scm.is_none() {
            return Outcome::Message("this is not a git repository".to_owned());
        }
        if !self
            .source_control
            .rows()
            .iter()
            .any(|row| row.group == crate::scm::Group::Staged)
        {
            return Outcome::Message("nothing is staged to commit".to_owned());
        }
        self.prompt = Some(Prompt::plain(PromptKind::CommitMessage));
        self.refresh_context();
        Outcome::Handled
    }

    /// The commit itself, once a message has been typed.
    fn commit(&mut self, message: String) -> Outcome {
        if message.is_empty() {
            // Enter on an empty input usually dismisses a prompt opened by
            // mistake. Git would also reject this, but checking here avoids
            // starting a process.
            return Outcome::Message("a commit needs a message".to_owned());
        }
        Outcome::GitOperation(deco_scm::Operation::Commit(message))
    }

    /// `git.checkout`: ask the frontend for local branches.
    ///
    /// Git cannot see editor buffers. Rejecting unsaved documents before the
    /// picker opens prevents a save after a successful checkout from writing the
    /// old branch's buffer over the new branch.
    fn ask_checkout(&mut self) -> Outcome {
        if self.scm.is_none() {
            return Outcome::Message("this is not a git repository".to_owned());
        }
        let unsaved = self.unsaved().len() + self.unsaved_untitled();
        if unsaved > 0 {
            return Outcome::Message(format!(
                "save or close {unsaved} unsaved document{} before switching branches",
                if unsaved == 1 { "" } else { "s" }
            ));
        }
        Outcome::GitBranches
    }

    /// Opens the local-branch picker once the frontend has asked Git.
    pub fn offer_branches(&mut self, branches: Vec<deco_scm::Branch>) {
        let current = branches
            .iter()
            .find(|branch| branch.current)
            .map(|branch| branch.name.clone());
        let choices = branches
            .into_iter()
            .filter(|branch| !branch.current)
            .map(|branch| {
                crate::commands::PaletteEntry::new(&branch.name, &branch.name)
                    .with_detail("local branch")
            })
            .collect::<Vec<_>>();
        if choices.is_empty() {
            self.status = Some("there is no other local branch to switch to".to_owned());
            self.refresh_context();
            return;
        }
        self.prompt = Some(Prompt::list(PromptKind::Branches, choices));
        self.status = current.map(|name| format!("currently on {name}"));
        self.refresh_context();
    }

    /// Shows the checkout cost and asks for an explicit second decision.
    pub fn confirm_checkout(&mut self, plan: deco_scm::CheckoutPlan) {
        let unsaved = self.unsaved().len() + self.unsaved_untitled();
        if unsaved > 0 {
            self.git_checkout_failed(&format!(
                "save or close {unsaved} unsaved document{} first",
                if unsaved == 1 { "" } else { "s" }
            ));
            return;
        }
        let detail = plan.summary();
        self.prompt = Some(Prompt::list(
            PromptKind::ConfirmCheckout,
            vec![
                crate::commands::PaletteEntry::new(CHECKOUT_CANCEL, "Cancel")
                    .with_detail("keep the current branch"),
                crate::commands::PaletteEntry::new(
                    &plan.target,
                    &format!("Switch to {}", plan.target),
                )
                .with_detail(&detail),
            ],
        ));
        self.status = Some(format!(
            "{} → {} — no local work will be discarded; Git will refuse an overwrite",
            plan.current, plan.target
        ));
        self.refresh_context();
    }

    /// Reports why branches or a checkout preview could not be obtained.
    pub fn git_checkout_failed(&mut self, reason: &str) {
        self.status = Some(format!("could not switch branches: {reason}"));
        self.refresh_context();
    }

    /// Reports that a repository change happened, so the display is updated.
    ///
    /// The status is requested again rather than adjusted here. Only git knows
    /// the current index, and a predicted state would diverge as soon as a hook
    /// changed something.
    pub fn git_operation_done(&mut self, operation: &deco_scm::Operation) {
        self.status = Some(operation.describe());
        if matches!(operation, deco_scm::Operation::Checkout(_)) {
            self.checkout_completed = true;
            self.comparison = None;
        }
        self.scm_changed();
        self.refresh_context();
    }

    /// Whether a completed checkout requires open files to be re-read.
    pub fn take_checkout_completed(&mut self) -> bool {
        std::mem::take(&mut self.checkout_completed)
    }

    /// Reports that one could not be carried out.
    pub fn git_operation_failed(&mut self, operation: &deco_scm::Operation, reason: &str) {
        self.status = Some(format!("could not {}: {reason}", operation.describe()));
        // Request the status again anyway. A failure usually means the view's
        // state of the index was out of date.
        self.scm_changed();
        self.refresh_context();
    }

    /// Opens a read-only, side-by-side comparison over the current editor tab.
    pub fn open_comparison(
        &mut self,
        request: deco_scm::ComparisonRequest,
        comparison: deco_scm::Comparison,
    ) {
        let original = comparison.original.unwrap_or_default();
        let modified = comparison.modified.unwrap_or_default();
        let ((left_text, left_lines), (right_text, right_lines)) =
            comparison_rows(&original, &modified);
        let language = crate::document::language_for_path(&request.path);
        let mut left_settings = EditorSettings::resolve(&self.settings, language);
        let mut right_settings = left_settings.clone();
        left_settings.word_wrap = deco_config::WordWrap::Off;
        right_settings.word_wrap = deco_config::WordWrap::Off;
        let (left_label, right_label) = match request.kind {
            deco_scm::ComparisonKind::Staged => ("HEAD", "Index"),
            deco_scm::ComparisonKind::WorkingTree => ("Index", "Working Tree"),
        };
        let view = View {
            width: self.view.width,
            height: self.view.height,
            ..Default::default()
        };
        self.comparison = Some(ComparisonView {
            left: ComparisonSide {
                document: Document::from_file(request.path.clone(), &left_text, left_settings),
                view: view.clone(),
                label: left_label,
                lines: left_lines,
            },
            right: ComparisonSide {
                document: Document::from_file(request.path, &right_text, right_settings),
                view,
                label: right_label,
                lines: right_lines,
            },
            focused_right: true,
        });
        self.focus = Focus::Editor;
        self.status = Some(format!("{left_label} ↔ {right_label} — read only"));
        self.relayout();
        self.refresh_context();
    }

    /// Reports why a requested comparison could not be opened.
    pub fn comparison_failed(&mut self, reason: &str) {
        self.status = Some(format!("could not open diff: {reason}"));
        self.refresh_context();
    }

    /// Whether a source-control comparison is covering the active tab.
    pub fn comparison_active(&self) -> bool {
        self.comparison.is_some()
    }

    /// Says where the repository begins.
    ///
    /// Not the same as the workspace root. A subdirectory of a repository can be
    /// opened, and every path in the source-control view is relative to the
    /// *repository*. Supplied by the frontend, because finding it requires
    /// running `git rev-parse`, which the core cannot do.
    pub fn set_repository_root(&mut self, root: Option<PathBuf>) {
        self.repository_root = root;
    }

    /// Where the repository begins, if a frontend has said.
    pub fn repository_root(&self) -> Option<&Path> {
        self.repository_root.as_deref()
    }

    /// The workspace tree, if a root has been set.
    pub fn explorer(&self) -> Option<&crate::Explorer> {
        self.explorer.as_ref()
    }

    /// A directory the tree needs read, if any.
    ///
    /// The frontend calls this after anything that could have expanded a
    /// directory, and repeats until it returns `None`. One listing is read per
    /// turn, so a deep reveal loads one level at a time rather than in one
    /// blocking walk.
    pub fn directory_wanted(&self) -> Option<std::path::PathBuf> {
        self.explorer.as_ref().and_then(crate::Explorer::wanted)
    }

    /// Whether `git.enabled` leaves the feature on.
    ///
    /// VS Code's setting, with VS Code's default of `true`. Read here rather
    /// than in a frontend so that all frontends interpret it the same way.
    pub fn git_enabled(&self) -> bool {
        self.settings.get_bool("git.enabled", None).unwrap_or(true)
    }

    /// Whether a fresh `git status` would be worth running.
    ///
    /// Always `false` when `git.enabled` is off, so no git process is spawned,
    /// not only hidden.
    pub fn scm_wanted(&self) -> bool {
        self.scm_wanted && self.git_enabled()
    }

    /// What `git status` last said, if anything.
    ///
    /// `None` when `git.enabled` is off, even if a status was found before. The
    /// setting hides existing results as well as stopping new runs.
    pub fn scm_status(&self) -> Option<&deco_scm::Status> {
        self.git_enabled().then_some(self.scm.as_ref()).flatten()
    }

    /// Records that a run has started, so another run is not requested.
    ///
    /// Separate from [`Session::fill_scm`] and called *first*, so a change during
    /// a run is not lost. A save while git is running sets the flag again, the
    /// result is stored without clearing it, and the next poll starts a new run.
    /// Clearing the flag when the result arrives would lose that save, and the
    /// status bar would stay out of date until the next change.
    pub fn scm_started(&mut self) {
        self.scm_wanted = false;
    }

    /// Stores the `git status` result.
    ///
    /// `None` replaces the previous status rather than keeping it. It means git
    /// is not available, this is not a repository, or git failed. A stale branch
    /// name must not stay on screen after the repository is gone.
    pub fn fill_scm(&mut self, status: Option<deco_scm::Status>) {
        // A different commit means every file's committed text may have
        // changed, so the cache is cleared. A `git commit` in another terminal
        // therefore clears the gutter instead of leaving open files compared
        // against the previous commit.
        let was = self.scm.as_ref().and_then(|status| status.commit.clone());
        let now = status.as_ref().and_then(|status| status.commit.clone());
        if was != now {
            self.committed.clear();
        }
        match &status {
            Some(status) => self.source_control.refresh(status),
            // No repository, or git is unavailable. Show an empty view rather
            // than the previous one, which would list files to stage in a folder
            // that is no longer a working tree.
            None => self.source_control = crate::scm::SourceControl::default(),
        }
        self.scm = status;
        self.refresh_context();
    }

    /// Which view the side bar is showing.
    pub fn side_bar_view(&self) -> SideBarView {
        self.side_bar_view
    }

    /// The source-control view.
    pub fn source_control(&self) -> &crate::scm::SourceControl {
        &self.source_control
    }

    /// Shows the side bar with `view` in it, and gives it the keyboard.
    ///
    /// VS Code's `workbench.view.*` commands do all three: open the container if
    /// it is closed, switch to that view, and focus it.
    fn show_side_bar_view(&mut self, view: SideBarView) -> Outcome {
        self.side_bar_view = view;
        if !self.side_bar {
            self.show_side_bar(true);
        }
        self.focus_region(Focus::SideBar)
    }

    /// A file whose committed text has not been fetched yet.
    ///
    /// Works like [`Session::directory_wanted`]: one file at a time, and the
    /// frontend responds with [`Session::fill_committed`]. Only open documents
    /// are returned, because only they have a gutter.
    pub fn committed_wanted(&self) -> Option<PathBuf> {
        if !self.git_enabled() || !self.gutter_marks_enabled() {
            return None;
        }
        self.documents()
            .filter_map(|document| document.path.clone())
            .find(|path| !self.committed.contains_key(path))
    }

    /// Stores a file's text from `HEAD`.
    ///
    /// `None` means the file is not in `HEAD`: it was added since the last
    /// commit, or the branch has no commits. Every line of the file is then new.
    /// The result is stored in both cases, so it is not requested again on
    /// every poll.
    pub fn fill_committed(&mut self, path: PathBuf, text: Option<String>) {
        self.committed.insert(
            path,
            Committed {
                text,
                // Computed on first use rather than here, because the buffer
                // may change before then.
                marks: None,
            },
        );
    }

    /// Whether `git.decorations.enabled` leaves the gutter marks on.
    ///
    /// VS Code's setting, with VS Code's default of `true`. Separate from
    /// `git.enabled`, which turns off the whole feature. This setting hides the
    /// gutter marks while keeping the branch in the status bar.
    pub fn gutter_marks_enabled(&self) -> bool {
        self.settings
            .get_bool("git.decorations.enabled", None)
            .unwrap_or(true)
    }

    /// Brings every open file's marks up to date with its buffer.
    ///
    /// Called by a frontend before drawing. It is separate from
    /// [`Session::diff_marks`] because a renderer holds the session by shared
    /// reference, and computing on demand there would require either a diff per
    /// frame or interior mutability. When nothing has changed, the cost is one
    /// version comparison per open document.
    pub fn refresh_diffs(&mut self) {
        if !self.git_enabled() || !self.gutter_marks_enabled() {
            return;
        }
        let open: Vec<(PathBuf, i32, String)> = self
            .documents()
            .filter_map(|document| {
                let path = document.path.clone()?;
                Some((path, document.buffer.version(), document.buffer.text()))
            })
            .collect();
        for (path, version, text) in open {
            let Some(entry) = self.committed.get(&path) else {
                continue;
            };
            if entry.marks.as_ref().map(|(at, _)| *at) == Some(version) {
                continue;
            }
            // A file with no committed text has every line marked as added,
            // which a diff against empty text produces.
            let head = entry.text.clone().unwrap_or_default();
            let diff = deco_scm::diff(&head, &text);
            if let Some(entry) = self.committed.get_mut(&path) {
                entry.marks = Some((version, diff));
            }
        }
    }

    /// What changed in `path` since it was committed, as of the last
    /// [`Session::refresh_diffs`].
    ///
    /// `None` when the committed text has not arrived, when nothing has
    /// computed the marks yet, or when they are switched off.
    pub fn diff_marks(&self, path: &Path) -> Option<&deco_scm::Diff> {
        if !self.git_enabled() || !self.gutter_marks_enabled() {
            return None;
        }
        self.committed
            .get(path)
            .and_then(|entry| entry.marks.as_ref())
            .map(|(_, diff)| diff)
    }

    /// Forgets what was cached for everything at or under `path`.
    ///
    /// A moved or deleted file's committed text is removed. The entry is keyed by
    /// path, so keeping it would make the *next* file with that name diff against
    /// another file's history.
    fn forget_committed_under(&mut self, path: &Path) {
        self.committed.retain(|held, _| !held.starts_with(path));
    }

    /// Marks the status as stale.
    ///
    /// Called for events that change the git status: a save, a file created,
    /// renamed or deleted, and returning to the window, since a commit may have
    /// been made in a terminal.
    pub fn scm_changed(&mut self) {
        self.scm_wanted = true;
    }

    /// Supplies the tree with a directory's contents.
    pub fn fill_directory(&mut self, dir: &std::path::Path, entries: Vec<crate::explorer::Entry>) {
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.fill(dir, entries);
        }
        self.refresh_context();
    }

    /// `revealInExplorer`: opens the tree onto the file being edited.
    ///
    /// An untitled document has no path to reveal, so a message is shown instead
    /// of ignoring the key.
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
    /// these keys on `sideBarFocus`, but a user binding without a `when` clause
    /// is allowed. Such a key must not move a hidden selection while the user
    /// types in the editor.
    fn explorer_key(&mut self, command: &str) -> Outcome {
        if self.focus != Focus::SideBar {
            return Outcome::Handled;
        }
        // Route to the view that is showing. VS Code also uses `list.*`: one
        // set of keys for every list in the workbench, with focus deciding which
        // list receives them, rather than a separate binding per view.
        if self.side_bar_view == SideBarView::SourceControl {
            return self.source_control_key(command);
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
                // Enter opens a file and toggles a directory, as in the VS Code
                // explorer.
                let Some(row) = explorer.selection() else {
                    return Outcome::Handled;
                };
                if row.is_dir {
                    explorer.toggle();
                } else {
                    // Focus moves to the editor with the opened file, so the
                    // user can type in it without another keystroke.
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
        // Pass the side bar's height so the tree can keep the selection on
        // screen. The model does not know its drawn height.
        let height = self.side_bar_rows();
        if let Some(explorer) = self.explorer.as_mut() {
            explorer.scroll_into_view(height);
        }
        self.refresh_context();
        Outcome::Handled
    }

    /// How many rows the side bar has for a list, once its chrome is off.
    ///
    /// The models do not know their drawn height, so it must be passed to them.
    /// It is computed here rather than in a frontend, because the session owns
    /// the division of the window.
    fn side_bar_rows(&self) -> usize {
        self.regions()
            .side_bar
            .map(|rect| rect.height.saturating_sub(EXPLORER_CHROME_ROWS))
            .unwrap_or(0)
    }

    /// Moves the keyboard to a region, showing it first if it is hidden.
    ///
    /// It must never focus a hidden region. Toggling does *not* focus: VS Code's
    /// `ctrl+b` leaves the caret in the text, so showing the tree does not move
    /// the user's position.
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

        // A region that does not fit in the window cannot take focus.
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
    /// Recomputed rather than stored. It is a pure function of the window size
    /// and two booleans, and a cached copy could become stale.
    pub fn regions(&self) -> crate::layout::Regions {
        self.regions_for(self.screen.0, self.screen.1)
    }

    /// The same division, of a rectangle the caller names.
    ///
    /// For a renderer, which knows the area it draws into and should not assume
    /// that it matches the session's last resize. The two are equal in the
    /// editor, because the frontend computes one from the other, but the
    /// renderer should not depend on that.
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
        // Subtract the regions first. The remaining rectangle belongs to the
        // editor, and the groups divide it and wrap text inside it.
        let editor = self.regions().editor;
        let (width, height) = (editor.width, editor.height);

        let columns = crate::layout::column_widths(width, self.group_count());
        if let Some(comparison) = self.comparison.as_mut() {
            for (index, side) in [&mut comparison.left, &mut comparison.right]
                .into_iter()
                .enumerate()
            {
                let pane_width = columns.get(index).copied().unwrap_or(width);
                let gutter = crate::layout::gutter_width(&side.document);
                side.view.width = pane_width;
                side.view.height = height;
                side.view.text_width = if self.frontend_wraps {
                    pane_width.saturating_sub(gutter)
                } else {
                    0
                };
            }
            return;
        }
        let gutter = crate::layout::gutter_width(&self.document);
        // The active group is the second one on screen while the split has the
        // keyboard, matching the order `panes` reports.
        let active = usize::from(self.split_focused);
        // Zero for a frontend that does not wrap, which tells the view not to
        // break lines.
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
            // Both groups currently show the same document, so one gutter width
            // applies to both. If they can differ, this must use each pane's own.
            other.text_width = other_width;
        }
    }

    /// Moves the open document to the front of the recency list.
    ///
    /// Called from [`Session::refresh_context`], which runs after every change to
    /// what is on screen, so no separate call sites need to be maintained. The
    /// guard reduces the common case to one comparison: on an ordinary keystroke
    /// the document is already at the front.
    fn note_active_document(&mut self) {
        let Some(path) = self.document.path.as_deref() else {
            // An untitled document has no path to record.
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
    /// The text width depends on the document's gutter and on the number of
    /// groups on screen, so anything that changes either (a tab switch, a split,
    /// closing a group) must call this. A stale width would wrap the current file
    /// at the previous file's width.
    ///
    /// Not a `resize`: nothing here scrolls. Focusing a group that was scrolled
    /// away from its caret must not move it, and the size has not changed.
    fn relayout(&mut self) {
        let (width, height) = (self.view.width, self.view.height);
        self.lay_out(width, height);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn snippet_variables_read_the_active_document_and_selection() {
        use deco_core::{Position, Selection, SelectionSet};
        let mut s = session();
        s.open(
            PathBuf::from("/w/sample.test.rs"),
            "header\nlet value = 1;\n",
        );
        s.view.selections =
            SelectionSet::single(Selection::new(Position::new(1, 4), Position::new(1, 9)));
        let snippet = s
            .expand_snippet("$TM_FILENAME|$TM_FILENAME_BASE|$TM_LINE_INDEX|$TM_LINE_NUMBER|$TM_CURRENT_LINE|$TM_CURRENT_WORD|$TM_SELECTED_TEXT")
            .unwrap();
        assert_eq!(
            snippet.text,
            "sample.test.rs|sample.test|1|2|let value = 1;|value|value"
        );
        assert_eq!(s.document.buffer.text(), "header\nlet value = 1;\n");
        assert!(!s.document.dirty);
    }

    #[test]
    fn snippet_variables_in_an_untitled_document_use_defaults() {
        let s = session();
        let snippet = s
            .expand_snippet("${TM_FILENAME:untitled}|${TM_SELECTED_TEXT:empty}|$TM_LINE_NUMBER")
            .unwrap();
        assert_eq!(snippet.text, "untitled|empty|1");
        assert!(s.expand_snippet("$CLIPBOARD").is_none());
        assert!(s.expand_snippet("${UNKNOWN:default}").is_none());
    }

    #[test]
    fn snippet_navigation_tracks_edits_and_finishes_at_zero() {
        use deco_core::{Position, Range};
        let mut s = session();
        let snippet = deco_lsp::snippet::Snippet::parse("call(${1:arg}, ${2:value})$0").unwrap();
        s.insert_snippet(Range::empty(Position::ZERO), &snippet, 0);
        assert_eq!(
            s.view.selections.primary().range(),
            Range::new(Position::new(0, 5), Position::new(0, 8))
        );
        s.run(
            "type",
            Some(&serde_json::json!({"text":"日本😀\n語"})),
            1000,
        );
        press(&mut s, "tab");
        assert_eq!(
            s.view.selections.primary().range(),
            Range::new(Position::new(1, 3), Position::new(1, 8))
        );
        press(&mut s, "shift+tab");
        assert_eq!(
            s.view.selections.primary().range(),
            Range::new(Position::new(0, 5), Position::new(1, 1))
        );
        press(&mut s, "tab");
        press(&mut s, "tab");
        assert!(s.document.snippet.is_none());
        assert_eq!(s.view.selections.primary().active, Position::new(1, 9));
    }

    #[test]
    fn snippet_escape_and_undo_clear_navigation() {
        use deco_core::{Position, Range};
        let mut s = session();
        let snippet = deco_lsp::snippet::Snippet::parse("${1:arg} end$0").unwrap();
        s.insert_snippet(Range::empty(Position::ZERO), &snippet, 0);
        press(&mut s, "escape");
        assert!(s.document.snippet.is_none());
        assert_eq!(s.document.buffer.text(), "arg end");
        s.run("undo", None, 1000);
        assert_eq!(s.document.buffer.text(), "");
        assert!(s.document.snippet.is_none());
        s.run("redo", None, 2000);
        assert_eq!(s.document.buffer.text(), "arg end");
        assert!(s.document.snippet.is_none());
    }

    #[test]
    fn snippet_leaving_field_or_external_edit_cancels() {
        use deco_core::{Position, Range, Selection, SelectionSet};
        let mut s = session();
        let snippet = deco_lsp::snippet::Snippet::parse("a ${1:arg} end$0").unwrap();
        s.insert_snippet(Range::empty(Position::ZERO), &snippet, 0);
        s.view.selections = SelectionSet::single(Selection::caret(Position::ZERO));
        s.refresh_context();
        assert!(s.document.snippet.is_none());
        s.insert_snippet(Range::empty(Position::ZERO), &snippet, 0);
        s.document
            .apply(&deco_core::Transaction::single(deco_core::Change::insert(
                Position::ZERO,
                "x".into(),
            )));
        assert!(s.document.snippet.is_none());
    }

    #[test]
    fn snippet_special_line_breaks_cancel_tracking_and_keep_text() {
        use deco_core::{Position, Range};
        for c in ['\r', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}'] {
            let mut s = session();
            let snippet = deco_lsp::snippet::Snippet::parse("${1:arg} end$0").unwrap();
            s.insert_snippet(Range::empty(Position::ZERO), &snippet, 0);
            let text = format!("a{c}b");
            s.run("type", Some(&serde_json::json!({"text":text})), 1);
            assert!(s.document.snippet.is_none());
            assert!(s.document.buffer.text().contains('b'));
        }
    }

    #[test]
    fn snippet_undo_while_active_does_not_restore_stale_stops() {
        use deco_core::{Position, Range};
        let mut s = session();
        let snippet = deco_lsp::snippet::Snippet::parse("${1:arg} end$0").unwrap();
        s.insert_snippet(Range::empty(Position::ZERO), &snippet, 0);
        s.run("type", Some(&serde_json::json!({"text":"value"})), 1000);
        s.run("undo", None, 2000);
        assert!(s.document.snippet.is_none());
        assert_eq!(s.document.buffer.text(), "arg end");
        s.run("redo", None, 3000);
        assert!(s.document.snippet.is_none());
        assert_eq!(s.document.buffer.text(), "value end");
    }

    #[test]
    fn snippet_state_does_not_move_to_another_tab() {
        use deco_core::{Position, Range};
        let mut s = session();
        s.open(std::path::PathBuf::from("first.rs"), "");
        let snippet = deco_lsp::snippet::Snippet::parse("${1:arg} end$0").unwrap();
        s.insert_snippet(Range::empty(Position::ZERO), &snippet, 0);
        s.open(std::path::PathBuf::from("second.rs"), "");
        assert!(s.document.snippet.is_none());
        press(&mut s, "tab");
        assert!(!s.document.buffer.text().is_empty());
        assert_eq!(
            s.document.path.as_deref(),
            Some(std::path::Path::new("second.rs"))
        );
    }

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
    /// start of `line`, which is enough to check that the correct text in the
    /// correct file changed.
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

    /// Answers both prompts of a replace-in-files and returns the outcome.
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
        // An empty replacement deletes every occurrence and must be accepted.
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
        // The two commands open the same prompt, so the session must remember
        // which one was used until the prompt is accepted.
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
        // The search read the file from disk, and this tab has since changed. A
        // replace must act on the buffer. Otherwise it would edit positions in
        // outdated text and a save would overwrite the buffer's changes.
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
        // The file is opened unsaved, so nothing is written to disk without the
        // user saving, and in a tab, so the user can see it needs saving.
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
        // The rejection must happen before the *other* document is changed. This
        // is why all transactions are built first.
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
        // The keystroke belongs only to this document. After it is undone, the
        // shared step is next, and only then does undo reach the other files.
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
        // A document with a name. An untitled one opens the save-as prompt
        // instead, which has its own test.
        let mut s = searchable("x\n");
        assert_eq!(press(&mut s, "ctrl+s"), Outcome::Save);
        assert_eq!(press(&mut s, "ctrl+q"), Outcome::Quit);
    }

    #[test]
    fn ctrl_s_on_an_untitled_document_asks_where_to_put_it() {
        // Regression test. Previously `ctrl+s` reported "This document has no
        // filename yet" after targeting the file deco was started with, and
        // `ctrl+w` reported "save it first", so an untitled tab could be neither
        // saved nor closed.
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
        // Save As followed by close, end to end.
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
        // The frontend writes the file and reports back, which clears `dirty`.
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
        // So a `when` clause copied from an existing keybindings.json behaves the
        // same here.
        let mut s = with_diagnostics(&[]);
        assert_eq!(s.context.get("editorHasDiagnostics"), Some(&json!(false)));
        s.set_diagnostics(vec![diagnostic(1, deco_lsp::Severity::Error, "x")]);
        assert_eq!(s.context.get("editorHasDiagnostics"), Some(&json!(true)));
    }

    #[test]
    fn publishing_replaces_rather_than_appends() {
        // The protocol replaces per document. Appending would duplicate every
        // error each time the file is analysed.
        let mut s = with_diagnostics(&[1, 2, 3]);
        s.set_diagnostics(vec![diagnostic(9, deco_lsp::Severity::Error, "only")]);
        assert_eq!(s.diagnostics.len(), 1);
    }

    #[test]
    fn opening_another_file_drops_the_previous_ones_diagnostics() {
        // They refer to line numbers in a file that is no longer on screen.
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
        // Pressing again after the last error returns to the first.
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
        // Servers publish in the order analysis finished.
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
        // A server can lag behind edits. If the user deletes the lines before it
        // recomputes, its ranges refer to text that no longer exists.
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
        // Where the caret goes after accepting a completion.
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
        // Accepting a completion is one action. Merging it with the word typed
        // before would make one undo remove both.
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
        // changed, such as a completion returned while typing continued.
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
        // Every range refers to the document the server saw, and applying the
        // edits front to back would shift every position after the first edit.
        // The edits are intentionally given out of order.
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
        // Formatting is one action. Undoing it one edit at a time would be
        // impractical on a file the server reformatted.
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
        // Servers answer with a no-op edit here. Applying it would mark the file
        // dirty and add an empty undo step.
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
        // Reject the entire overlapping batch and preserve the original text.
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
        // Formatting must not move the caret to the end of the file.
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
        // The server's response may refer to text the user has since deleted.
        let mut s = session();
        s.open(PathBuf::from("/w/a.rs"), "ab\n");
        s.apply_edits(&[edit(0, 1, 99, "Z")], 0).unwrap();
        assert_eq!(s.document.buffer.line_content(0).unwrap(), "aZ");
    }

    #[test]
    fn formatting_options_come_from_the_users_own_settings() {
        // Without these options a server uses its own defaults. In a project
        // with different settings, formatting would then change every line.
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
        // The core has no language server. Listing the commands by name keeps a
        // mistyped binding reported as unknown.
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
        // The first match, not the third: narrowing the query must not move the
        // cursor down the file on each keystroke.
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
        // The bar is closed, so the count is shown in the status bar.
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

    /// Types `text` into whichever input has the keyboard.
    fn type_into(s: &mut Session, text: &str) {
        s.dispatch("type", Some(&serde_json::json!({ "text": text })), 0);
    }

    #[test]
    fn alt_r_toggles_regex_and_re_searches() {
        let mut s = searchable("foo fooo f.o\n");
        press(&mut s, "ctrl+f");
        type_into(&mut s, "fo+");
        assert!(
            s.find.matches().is_empty(),
            "literal `fo+` is not in the text"
        );
        assert_eq!(press(&mut s, "alt+r"), Outcome::Handled);
        assert!(s.find.options().regex);
        assert_eq!(s.find.matches().len(), 2);
        assert_eq!(selected(&s), ((0, 0), (0, 3)));
        press(&mut s, "alt+r");
        assert!(!s.find.options().regex);
        assert!(s.find.matches().is_empty());
    }

    #[test]
    fn an_invalid_regex_is_reported_rather_than_shown_as_no_results() {
        let mut s = searchable("(foo\n");
        press(&mut s, "ctrl+f");
        press(&mut s, "alt+r");
        type_into(&mut s, "(foo");
        assert!(s.find.matches().is_empty());
        assert!(s.find.error().is_some());
        let Outcome::Message(message) = press(&mut s, "enter") else {
            panic!("stepping should report the error");
        };
        assert!(
            message.starts_with("invalid regular expression"),
            "{message}"
        );
    }

    #[test]
    fn a_seed_is_escaped_in_regex_mode() {
        let mut s = searchable("a.b axb a.b\n");
        press(&mut s, "ctrl+f");
        press(&mut s, "alt+r");
        press(&mut s, "escape");
        s.view.selections = deco_core::SelectionSet::single(deco_core::Selection::new(
            Position::new(0, 0),
            Position::new(0, 3),
        ));
        press(&mut s, "ctrl+f");
        assert_eq!(s.find.query(), r"a\.b");
        assert_eq!(s.find.matches().len(), 2);
    }

    #[test]
    fn replace_all_expands_capture_groups_per_match_in_one_step() {
        let mut s = searchable("a=1\nb=2\n");
        press(&mut s, "ctrl+h");
        press(&mut s, "alt+r");
        type_into(&mut s, r"(\w)=(\d)");
        press(&mut s, "tab");
        type_into(&mut s, "$2=$1");
        assert_eq!(
            s.run("editor.action.replaceAll", None, 0),
            Outcome::Message("replaced 2 occurrences".to_owned())
        );
        assert_eq!(s.document.buffer.text(), "1=a\n2=b\n");
        s.run("undo", None, 0);
        assert_eq!(s.document.buffer.text(), "a=1\nb=2\n");
    }

    #[test]
    fn replace_one_expands_capture_groups_for_the_selected_match() {
        let mut s = searchable("a=1\nb=2\n");
        press(&mut s, "ctrl+h");
        press(&mut s, "alt+r");
        type_into(&mut s, r"(\w)=(\d)");
        press(&mut s, "tab");
        type_into(&mut s, "$2");
        s.run("editor.action.replaceOne", None, 0);
        assert_eq!(s.document.buffer.text(), "1\nb=2\n");
        assert_eq!(selected(&s), ((1, 0), (1, 3)), "the next match is selected");
    }

    #[test]
    fn an_invalid_regex_in_a_project_search_is_reported_before_searching() {
        let mut s = searchable("x\n");
        s.run("workbench.action.findInFiles", None, 0);
        press(&mut s, "alt+r");
        assert_eq!(
            s.status.as_deref(),
            Some("Search: case off, whole word off, regex on")
        );
        press(&mut s, "ctrl+x");
        type_into(&mut s, "(x");
        let Outcome::Message(message) = press(&mut s, "enter") else {
            panic!("an invalid pattern should not reach the frontend");
        };
        assert!(
            message.starts_with("invalid regular expression"),
            "{message}"
        );
    }

    #[test]
    fn the_context_keys_follow_the_find_bar() {
        let mut s = searchable("foo\n");
        assert_eq!(s.context.get("findWidgetVisible"), Some(&json!(false)));
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(true)));

        press(&mut s, "ctrl+f");
        // VS Code's spelling for both keys, so a `when` clause copied from a
        // keybindings.json behaves the same here.
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
        // The second press replaces the match, which is now visible.
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
        // Tests back-to-front application: applied in document order, every edit
        // after the first would be misplaced by the shifted positions.
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
        // The registry is a list of strings separate from two `match`es on
        // strings, so they can get out of sync. A palette entry that resolves to
        // `NotFound` would be offered to the user and then reported as unknown
        // when chosen.
        for (id, title) in commands::PALETTE {
            // A fresh session per entry, because several entries change state,
            // including `quit`.
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
        // The wanted file is usually one that was recently open, so recent files
        // are listed before the alphabetical list.
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
        // VS Code also keeps closed files in the list, because they are likely to
        // be reopened.
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
        // With no recent files, the order is alphabetical.
        let mut s = session();
        s.offer_files(file_entries(&["aaa.rs", "bbb.rs", "ccc.rs"]));
        assert_eq!(titles(&s), ["aaa.rs", "bbb.rs", "ccc.rs"]);
    }

    #[test]
    fn a_path_spelled_differently_is_still_the_same_file() {
        // `ctrl+o` resolves the typed path, and the walk joins onto the workspace
        // root. They can differ in `./`, and a string comparison would not
        // recognise the file as recent.
        let mut s = session();
        s.open(PathBuf::from("/w/./src/../src/main.rs"), "m\n");
        s.offer_files(file_entries(&["aaa.rs", "src/main.rs"]));
        assert_eq!(titles(&s), ["src/main.rs", "aaa.rs"]);
    }

    #[test]
    fn recency_orders_equal_matches_and_no_more_than_that() {
        // Two rows match `main` equally well, so recency decides the order.
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
        // Recency orders equal matches only. It does not override match quality.
        // Here `main.rs` matches `main` as a prefix, `domain.rs` only contains it,
        // and `domain.rs` is the file that was open.
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
        // `editor.detectIndentation` is on by default, so the first `tab` in a
        // project with different indentation uses the file's indentation.
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
        // The status bar then shows no override, which is the case for most files.
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "editor.tabSize", json!(2));
        let mut s = Session::new(settings, None, Platform::Linux);
        s.open(PathBuf::from("/w/a.ts"), "a\n  b\n    c\n");
        assert_eq!(s.document.settings.tab_size, 2);
        assert!(!s.document.indentation_overridden);
    }

    #[test]
    fn a_tab_indented_file_switches_to_tabs_and_keeps_the_settings_width() {
        // `editor.tabSize` sets the drawn tab width. The file only indicates that
        // it uses tabs.
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
        // `ctrl+k m` resolves the settings from scratch, but changing the
        // language does not change the file's indentation.
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
        // The flag is read from the newly resolved settings, so a workspace layer
        // takes effect without reopening the file.
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
        // The frontend shows every entry, so the user learns that the setting has
        // no effect.
        let mut settings = Settings::with_defaults();
        settings.set(Scope::User, "files.autoSave", json!("onFocusChange"));
        let s = Session::new(settings, None, Platform::Linux);
        assert_eq!(s.problems.len(), 1, "{:?}", s.problems);
        assert!(s.problems[0].contains("onFocusChange"), "{:?}", s.problems);
    }

    #[test]
    fn the_same_complaint_is_not_made_twice() {
        // The settings are re-resolved on a language change, a rename and a new
        // workspace layer. Each must not add a duplicate problem.
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
        // The message includes the column, because it is not visible from the
        // text and it is what the setting controls.
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
        // A user who configured `bounded` and pressed the key twice gets
        // `bounded` back, not wrapping at the window width.
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
        // The settings are per document, so enabling wrap for one file does not
        // affect the code in the next tab.
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
        // Two groups share the width, so the same line wraps earlier in each. A
        // group that kept the full-width wrap column would draw past its column.
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
        // The GPU frontend lays out one line per row. If the session wrapped
        // anyway, it would scroll and move the caret by rows that are not drawn,
        // and the caret would not match the text.
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
        // Regression test. Previously it searched for the seed immediately, so a
        // project search could only find the text at the cursor.
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
        // With one shared set of options, case sensitivity set for a workspace
        // search changed what the next ctrl+f matched. VS Code keeps them separate.
        let mut s = searchable("x\n");
        s.run("workbench.action.findInFiles", None, 0);
        press(&mut s, "alt+c");
        assert_eq!(
            s.status.as_deref(),
            Some("Search: case on, whole word off, regex off"),
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

        // With neither, the find bar's last query is used.
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
        // These are different conditions and need different messages.
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
        // Editing the current path is faster than typing a full one.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/notes.txt"), "text\n");
        assert_eq!(press(&mut s, "ctrl+shift+s"), Outcome::Handled);
        let prompt = s.prompt.as_ref().expect("a prompt should be open");
        assert_eq!(prompt.kind(), crate::prompt::PromptKind::SaveAs);
        assert_eq!(prompt.text(), "/w/notes.txt");
    }

    #[test]
    fn open_file_seeds_the_prompt_with_the_directory_only() {
        // The user wants to open a different file, so the seed omits the file
        // name.
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
        // `ctrl+x` clears a one-line input, so the seed is replaced rather than
        // appended to. `ctrl+a` is ignored, because the field has no selection.
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
        // Saving under a new name must not override a manually chosen language.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/notes.txt"), "name = 1\n");
        s.set_language(Some("toml"));

        s.rename_to(PathBuf::from("/w/other.txt"));
        assert_eq!(s.document.language(), Some("toml"));
    }

    #[test]
    fn renaming_a_detected_language_still_follows_the_name() {
        // The opposite of the case above: no language was chosen manually, so the
        // file name decides.
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
        // A second copy of the document could diverge from the one being edited.
        let mut s = searchable("x\n");
        press(&mut s, "y");
        let panes = s.panes();
        assert_eq!(panes[0].document.buffer.text(), "yx\n");
    }

    // ---- Picker selection --------------------------------------------------

    #[test]
    fn typing_in_a_picker_selects_the_best_match() {
        // `enter` runs the selected entry, so it must be the best match for the
        // typed text. Previously the selection stayed on the previously selected
        // entry (initially row 0, the registry's first entry) regardless of its
        // rank.
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
        // A shorter query is a new query, so its best match is selected, not the
        // best match for the longer query.
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
        // Regression test. `deco src/main.rs` followed by picking the same file
        // from `ctrl+p` opened it twice, with two buffers and two undo histories,
        // and the last save overwrote the other.
        let mut s = session();
        s.resize(80, 10);
        s.open(PathBuf::from("/w/src/main.rs"), "fn main() {}\n");
        s.open(PathBuf::from("/w/./src/../src/main.rs"), "fn main() {}\n");
        assert_eq!(s.tab_count(), 1);
    }

    #[test]
    fn switching_to_it_keeps_the_edits_rather_than_rereading() {
        // With one tab per file, the second open switches to the tab, so unsaved
        // changes are kept.
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
        // Dropping the `..` from `../a.rs` would change where it points.
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
        // There is no file to re-read, and the document started empty. This lets
        // the user close a scratch buffer without saving it.
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

        // The frontend then calls `revert_to` with the file's text.
        assert_eq!(
            s.revert_to("saved\n"),
            Outcome::Message("Reverted a.txt".to_owned())
        );
        assert_eq!(s.document.buffer.text(), "saved\n");
        assert!(!s.document.dirty);
    }

    #[test]
    fn a_revert_can_be_undone() {
        // Revert discards edits, so it must be undoable.
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
        // ctrl+w does not close one unsaved document, so ctrl+q must not discard
        // all of them.
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
        // Any other key resets the confirmation, so a later ctrl+q asks again.
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
        // one file, which `open` also prevents for tabs.
        let mut s = searchable("one\ntwo\nthree\n");
        assert_eq!(s.group_count(), 1);
        press(&mut s, "ctrl+\\");
        assert_eq!(s.group_count(), 2);

        let panes = s.panes();
        assert_eq!(panes.len(), 2);
        assert!(std::ptr::eq(panes[0].document, panes[1].document));
        // The new group starts at the same position as the old one and takes
        // the keyboard.
        assert!(!panes[0].focused);
        assert!(panes[1].focused);
    }

    #[test]
    fn each_group_scrolls_and_moves_on_its_own() {
        // Two places in one file are visible at once.
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
        // Both groups show the edit, because there is one document, but only the
        // focused view's cursor moves.
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
        // After a split, the key first closes the second group.
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
        // Its matches were found in the other view, and its current match is at
        // that group's cursor.
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
        // Handled by the frontend, because a marketplace theme is a file in an
        // extension directory and the core has no filesystem.
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
        // The built-in themes always work, so they stay at the top above the
        // installed ones.
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
        // deco reads `workbench.colorTheme` but never writes it, so the message
        // tells the user which setting to change.
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
        // Detection is first, because it is the only way to undo a manual
        // language choice.
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
        // A `.txt` file that contains TOML. The name does not indicate TOML, so
        // the lexer is inactive until the language is set.
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
        // Changing the language must not alter text or create an undo entry.
        let mut s = searchable("x\n");
        s.open(PathBuf::from("/w/notes.txt"), "name = 1\n");
        s.set_language(Some("toml"));
        assert_eq!(s.document.buffer.text(), "name = 1\n");
        assert!(!s.document.dirty, "and it is not an edit");
    }

    #[test]
    fn a_language_deco_has_no_name_for_is_shown_as_its_identifier() {
        // Such an identifier can come from a settings file or a server. Showing
        // the identifier is more useful than showing nothing.
        assert_eq!(crate::document::language_title("rust"), "Rust");
        assert_eq!(crate::document::language_title("brainfuck"), "brainfuck");
    }

    #[test]
    fn every_language_the_file_name_can_detect_is_offerable() {
        // Otherwise a document could be in a language the picker cannot select.
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
        // Byte order sorts uppercase before lowercase, so `JSON` would come
        // before `Java`. Tested through the real picker rather than a copy of the
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
        // A tab must not appear saved when its write failed.
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
        // The reason goes to the problem list, because the status bar has one line.
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
        // `files.insertFinalNewline` can differ per language, and a batch save
        // must use each tab's value rather than the active tab's.
        //
        // `files.eol` is fixed rather than left at `auto`. A document with no
        // existing line ending uses the platform's, so the appended newline would
        // be CRLF on Windows and the test would depend on the host.
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
        // Prevents adding a key binding that does nothing. A command that nothing
        // handles and that is not in `commands::PENDING` returns `NotFound`,
        // which the frontend does not display, so the key would appear to do
        // nothing, like a hung editor.
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
        // The session owns the window division because the wrap width must
        // follow it. Otherwise lines would break at a width the renderer does not
        // use.
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

        // `src` was selected, so the file was created inside it. Revealing the
        // file expanded `src`, so the tree now requests its listing.
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

        // Creating a file moved focus to the editor. The tree's undo requires
        // tree focus, which the user restores with `ctrl+shift+e`.
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
        // The full user sequence, including prompts, as in the demonstration.
        // A focus bug would appear here.
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
        // The demonstration's sequence: create through the prompt, then type. If
        // focus stayed in the tree, the typed keys would be ignored.
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
        // The frontend opens a created file.
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
        // The demonstration's full sequence. Typing into the new file gives the
        // *document* an undo history, and `ctrl+z` in the tree must still run the
        // tree's undo. Focus selects the stack, not which was used last.
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
        // The frontend re-reads the changed directory, so the reveal can select
        // the new file.
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

        // A failed delete must not remove the create's undo entry.
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
        // After a project-wide replace the document has a shared undo step.
        // `ctrl+z` in the tree must still run the tree's undo.
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

        // Two presses must undo two operations, not undo one and redo it.
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

        // `.txt` has no language, so no server will replace these tokens.
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
        // Needed for a partly completed recursive delete: some of a directory's
        // files are gone and the rest remain, and only the filesystem can tell
        // which.
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

        // A recursive delete that removed part of a tree and then stopped. An
        // irreversible change happened, so the older inverses no longer describe
        // a valid state.
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
        // A new, empty directory with the same name exists.
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
        // Choosing Rust for a file already detected as Rust. The language value
        // alone cannot distinguish this from no manual choice.
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
        // A file another program deleted, still open here. The tree no longer
        // lists it, so only the tab check prevents the rename.
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
        // Deleted by another program and still open here. The tree does not
        // list it, so only the tab check prevents the create.
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
        // `/w/b/x.rs` is open, and another program removed `b`, so the tree does
        // not list it and the name appears free.
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

        // Undo the second rename. The frontend stamps the file it just moved,
        // and the stamp must not be applied to the first rename's entry, which
        // is now on top.
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

        // A new `Cargo.toml` is opened and then removed by another program, so
        // the tree does not list it and the path appears free.
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

        // `src` is removed outside deco. A refresh of the *parent* (for example,
        // from a sibling operation) removes the row, but `src`'s listing and
        // expansion stay cached.
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

    /// What `git status --porcelain=v2 --branch -z` writes for a branch at
    /// `commit`.
    fn scm_at(commit: &str) -> deco_scm::Status {
        deco_scm::parse(&format!("# branch.oid {commit}\0# branch.head main\0"))
            .expect("git's own format")
    }

    #[test]
    fn the_marks_follow_the_buffer_rather_than_the_file_on_disk() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/a.rs"), "one\ntwo\n");
        s.fill_committed(PathBuf::from("/w/a.rs"), Some("one\ntwo\n".to_owned()));
        s.refresh_diffs();
        assert!(
            s.diff_marks(Path::new("/w/a.rs"))
                .expect("the committed text has arrived")
                .is_empty(),
            "nothing typed yet, so nothing differs"
        );

        // Typed, not saved. The gutter must describe the buffer, not the file on
        // disk.
        s.run("cursorEnd", None, 0);
        s.run("type", Some(&json!({ "text": "!" })), 0);
        s.refresh_diffs();
        let diff = s.diff_marks(Path::new("/w/a.rs")).expect("still open");
        assert_eq!(diff.mark_at(0), Some(deco_scm::Mark::Modified));
        assert_eq!(diff.mark_at(1), None, "and only the line that was touched");
    }

    #[test]
    fn a_file_with_nothing_committed_is_all_addition() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/new.rs"), "one\ntwo\n");
        // `None` means the file is not in HEAD: it was added since the last
        // commit. Every line is marked as added, which differs from no marks.
        s.fill_committed(PathBuf::from("/w/new.rs"), None);

        s.refresh_diffs();
        let diff = s.diff_marks(Path::new("/w/new.rs")).expect("an answer");
        assert_eq!(diff.mark_at(0), Some(deco_scm::Mark::Added));
        assert_eq!(diff.mark_at(1), Some(deco_scm::Mark::Added));
    }

    #[test]
    fn a_commit_somewhere_else_throws_the_committed_text_away() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/a.rs"), "one\n");
        s.fill_scm(Some(scm_at("1111111")));
        s.fill_committed(PathBuf::from("/w/a.rs"), Some("old\n".to_owned()));
        assert_eq!(s.committed_wanted(), None, "it has been answered");

        // A commit is made in a terminal. Every file's committed text may have
        // changed, and a gutter drawn against the old text would be wrong.
        s.fill_scm(Some(scm_at("2222222")));
        assert_eq!(
            s.committed_wanted(),
            Some(PathBuf::from("/w/a.rs")),
            "the file is asked about again rather than kept"
        );
    }

    #[test]
    fn the_same_commit_reported_again_keeps_what_was_fetched() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/a.rs"), "one\n");
        s.fill_scm(Some(scm_at("1111111")));
        s.fill_committed(PathBuf::from("/w/a.rs"), Some("old\n".to_owned()));

        // A save refreshes the status without moving HEAD. Re-fetching every
        // open file's committed text on every save would start one process per
        // file each time, for data that has not changed.
        s.fill_scm(Some(scm_at("1111111")));
        assert_eq!(s.committed_wanted(), None);
    }

    #[test]
    fn turning_the_decorations_off_stops_the_fetch_as_well_as_the_marks() {
        let mut s = with_tree();
        s.settings.set(
            deco_config::Scope::User,
            "git.decorations.enabled",
            serde_json::Value::Bool(false),
        );
        s.open(PathBuf::from("/w/a.rs"), "one\n");
        assert_eq!(
            s.committed_wanted(),
            None,
            "a setting that only hid the marks would still pay for them"
        );
        s.fill_committed(PathBuf::from("/w/a.rs"), Some("other\n".to_owned()));
        s.refresh_diffs();
        assert!(s.diff_marks(Path::new("/w/a.rs")).is_none());
    }

    #[test]
    fn a_renamed_file_does_not_keep_the_old_names_history() {
        let mut s = with_tree();
        s.open(PathBuf::from("/w/a.rs"), "one\n");
        s.fill_committed(PathBuf::from("/w/a.rs"), Some("one\n".to_owned()));

        let renamed = crate::files::Operation::Rename {
            from: PathBuf::from("/w/a.rs"),
            to: PathBuf::from("/w/b.rs"),
            expect: None,
            directory: false,
        };
        s.file_operation_done(&renamed);

        // The entry is keyed by path. If it were kept, the next file named
        // `a.rs` would be diffed against another file's history.
        assert!(
            s.committed_wanted().is_some(),
            "the moved file is asked about under its new name"
        );
        assert!(s.diff_marks(Path::new("/w/a.rs")).is_none());
    }

    /// A status with one modified and one untracked file.
    fn dirty_status() -> deco_scm::Status {
        deco_scm::parse(
            "# branch.oid 1c9d4e5\0# branch.head main\0\
             1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb work.rs\0? new.rs\0",
        )
        .expect("git's own format")
    }

    #[test]
    fn the_side_bar_opens_on_the_tree_and_switches_to_source_control() {
        let mut s = with_tree();
        assert_eq!(s.side_bar_view(), SideBarView::Explorer);

        // One key opens the container, switches the view, and takes focus.
        assert!(matches!(
            s.run("workbench.view.scm", None, 0),
            Outcome::Handled
        ));
        assert_eq!(s.side_bar_view(), SideBarView::SourceControl);
        assert_eq!(s.focus(), Focus::SideBar);
        assert!(
            s.regions().side_bar.is_some(),
            "and the side bar is showing"
        );

        s.run("workbench.view.explorer", None, 0);
        assert_eq!(s.side_bar_view(), SideBarView::Explorer);
    }

    #[test]
    fn the_list_keys_reach_whichever_tenant_is_showing() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        s.run("workbench.view.scm", None, 0);

        s.run("list.focusDown", None, 0);
        assert_eq!(
            s.source_control().selection().unwrap().path,
            PathBuf::from("new.rs"),
            "the same `list.*` keys, and what has the keyboard decides"
        );

        // Back to the tree, and the same key moves the tree instead.
        s.run("workbench.view.explorer", None, 0);
        s.run("list.focusFirst", None, 0);
        s.run("list.focusDown", None, 0);
        assert_eq!(
            s.explorer().unwrap().selection().unwrap().name,
            "Cargo.toml"
        );
        assert_eq!(
            s.source_control().selection().unwrap().path,
            PathBuf::from("new.rs"),
            "and the view that does not have the keyboard is left alone"
        );
    }

    #[test]
    fn the_explorers_context_key_is_false_while_source_control_has_the_keyboard() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        s.run("workbench.view.scm", None, 0);

        // `explorer.newFile` and the tree's `ctrl+z` are gated on
        // `filesExplorerFocus`. If it stayed true here, `ctrl+n` in the
        // source-control view would create a file in the hidden tree.
        assert_eq!(s.context.get("filesExplorerFocus"), Some(&json!(false)));
        assert_eq!(
            s.context.get("listFocus"),
            Some(&json!(true)),
            "but it is still a list, and `list.*` is bound on that"
        );
        assert_eq!(s.context.get("scmProvider"), Some(&json!("git")));
    }

    #[test]
    fn the_trees_keys_do_not_reach_it_from_the_source_control_view() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        s.run("workbench.view.scm", None, 0);

        // `sideBarFocus` is true for *both* views, so a binding gated only on it
        // would act on the hidden tree. For example, `ctrl+z` would undo a file
        // operation that is not shown on screen.
        for key in ["ctrl+z", "ctrl+n", "ctrl+shift+n", "f2", "delete"] {
            let chord = Chord::parse(key).expect("a key");
            let outcome = s.handle_chord(chord, 0);
            assert!(
                matches!(outcome, Outcome::NotFound | Outcome::Handled),
                "{key} resolved to something in the source-control view: {outcome:?}"
            );
            assert!(
                s.prompt.is_none(),
                "{key} opened one of the tree's prompts from the wrong view"
            );
        }

        // And they still work where they belong.
        s.run("workbench.view.explorer", None, 0);
        s.handle_chord(Chord::parse("ctrl+n").expect("a key"), 0);
        assert_eq!(
            s.prompt.as_ref().map(|p| p.kind()),
            Some(PromptKind::NewFile)
        );
    }

    #[test]
    fn the_source_control_list_scrolls_with_its_selection() {
        let mut s = with_tree();
        let mut entries = String::from("# branch.oid 1c9d4e5\0# branch.head main\0");
        for n in 0..30 {
            entries.push_str(&format!("? file{n:02}.rs\0"));
        }
        s.fill_scm(Some(deco_scm::parse(&entries).expect("git's own format")));
        s.resize(80, 14);
        s.run("workbench.view.scm", None, 0);

        assert_eq!(s.source_control().scroll(), 0);
        for _ in 0..29 {
            s.run("list.focusDown", None, 0);
        }
        // Otherwise the selection moves below the visible area, and the next
        // stage acts on a file that is not shown.
        assert!(
            s.source_control().scroll() > 0,
            "the list never scrolled: selection {} of {}",
            s.source_control().selected_index(),
            s.source_control().rows().len()
        );
    }

    #[test]
    fn a_folder_that_is_not_a_repository_empties_the_view() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        assert!(!s.source_control().is_empty());

        // git is unavailable, or the folder is no longer a working tree. The
        // previous list of files to stage must not remain.
        s.fill_scm(None);
        assert!(s.source_control().is_empty());
        assert_eq!(s.context.get("scmProvider"), Some(&json!("")));
    }

    #[test]
    fn enter_in_the_source_control_view_requests_the_selected_comparison() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        s.run("workbench.view.scm", None, 0);

        let Outcome::GitComparison(request) = s.run("list.select", None, 0) else {
            panic!("expected a comparison request");
        };
        assert_eq!(request.path, PathBuf::from("work.rs"));
        assert_eq!(request.kind, deco_scm::ComparisonKind::WorkingTree);
        assert_eq!(
            s.focus(),
            Focus::Editor,
            "and the keyboard follows the diff, as it does out of the tree"
        );
    }

    #[test]
    fn a_comparison_is_aligned_read_only_and_closes_back_to_the_tab() {
        let mut s = Session::with_defaults();
        s.open(PathBuf::from("work.rs"), "the live tab\n");
        s.open_comparison(
            deco_scm::ComparisonRequest {
                path: PathBuf::from("work.rs"),
                original: None,
                kind: deco_scm::ComparisonKind::WorkingTree,
            },
            deco_scm::Comparison {
                original: Some("one\ntwo\nthree\n".to_owned()),
                modified: Some("one\ninserted\ntwo changed\nthree\n".to_owned()),
            },
        );

        let panes = s.panes();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].comparison.as_ref().unwrap().label, "Index");
        assert_eq!(panes[1].comparison.as_ref().unwrap().label, "Working Tree");
        assert_eq!(
            panes[0].document.buffer.line_count(),
            panes[1].document.buffer.line_count(),
            "alignment gaps keep both sides on the same row"
        );
        assert_eq!(s.context.get("editorReadonly"), Some(&json!(true)));

        assert!(matches!(
            s.handle_chord(Chord::char('x'), 0),
            Outcome::Message(_)
        ));
        assert_eq!(s.document.buffer.text(), "the live tab\n");
        s.run("workbench.action.closeActiveEditor", None, 0);
        assert!(!s.comparison_active());
        assert_eq!(s.document.buffer.text(), "the live tab\n");
    }

    #[test]
    fn comparison_navigation_keeps_both_vertical_viewports_together() {
        let mut s = Session::with_defaults();
        let original: String = (0..40).map(|line| format!("line {line}\n")).collect();
        let modified: String = (0..40)
            .map(|line| format!("line {line} changed\n"))
            .collect();
        s.open_comparison(
            deco_scm::ComparisonRequest {
                path: PathBuf::from("work.rs"),
                original: None,
                kind: deco_scm::ComparisonKind::WorkingTree,
            },
            deco_scm::Comparison {
                original: Some(original),
                modified: Some(modified),
            },
        );
        s.resize(40, 5);
        s.run("cursorBottom", None, 0);

        let panes = s.panes();
        assert!(panes[1].view.scroll_top > 0);
        assert_eq!(panes[0].view.scroll_top, panes[1].view.scroll_top);
        assert_eq!(panes[0].view.scroll_row, panes[1].view.scroll_row);
    }

    #[test]
    fn staging_names_the_selected_file_relative_to_the_repository() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        s.run("workbench.view.scm", None, 0);

        let Outcome::GitOperation(op) = s.run("git.stage", None, 0) else {
            panic!("expected a stage");
        };
        assert_eq!(
            op,
            deco_scm::Operation::Stage(PathBuf::from("work.rs")),
            "git answers about repository-relative paths, and the view holds them"
        );
    }

    #[test]
    fn checkout_requires_clean_buffers_and_an_explicit_confirmation() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        s.open(PathBuf::from("/w/a.rs"), "main\n");
        s.handle_chord(Chord::char('x'), 0);
        assert!(
            matches!(s.run("git.checkout", None, 0), Outcome::Message(ref message) if message.contains("unsaved document")),
            "Git cannot protect text that only exists in the editor"
        );

        s.mark_saved();
        assert!(matches!(
            s.run("git.checkout", None, 0),
            Outcome::GitBranches
        ));
        s.offer_branches(vec![
            deco_scm::Branch {
                name: "main".to_owned(),
                current: true,
            },
            deco_scm::Branch {
                name: "feature".to_owned(),
                current: false,
            },
        ]);
        assert_eq!(
            s.prompt.as_ref().map(Prompt::kind),
            Some(PromptKind::Branches)
        );
        assert!(matches!(
            press(&mut s, "enter"),
            Outcome::GitCheckoutPreview(ref target) if target == "feature"
        ));

        let plan = deco_scm::CheckoutPlan {
            current: "main".to_owned(),
            target: "feature".to_owned(),
            branch_changes: 3,
            staged: 1,
            unstaged: 0,
            untracked: 1,
        };
        s.confirm_checkout(plan.clone());
        assert_eq!(
            s.prompt
                .as_ref()
                .and_then(Prompt::selected)
                .map(|row| row.id.as_str()),
            Some(CHECKOUT_CANCEL),
            "enter alone must be the safe answer"
        );
        assert!(matches!(press(&mut s, "enter"), Outcome::Message(_)));

        s.confirm_checkout(plan);
        s.prompt.as_mut().expect("confirmation").next();
        assert!(matches!(
            press(&mut s, "enter"),
            Outcome::GitOperation(deco_scm::Operation::Checkout(ref target)) if target == "feature"
        ));
    }

    #[test]
    fn a_completed_checkout_reloads_clean_tabs_and_drops_old_history() {
        let mut s = Session::with_defaults();
        let path = PathBuf::from("/w/a.rs");
        s.open(path.clone(), "main\n");
        s.handle_chord(Chord::char('x'), 0);
        s.mark_saved();
        s.run("undo", None, 0);
        assert_eq!(
            s.document.buffer.text(),
            "main\n",
            "the old history existed"
        );

        s.git_operation_done(&deco_scm::Operation::Checkout("feature".to_owned()));
        assert!(s.take_checkout_completed());
        assert!(!s.take_checkout_completed(), "the signal is consumed once");
        assert!(s.reload_open(&path, "feature\n"));
        assert_eq!(s.document.buffer.text(), "feature\n");
        assert!(!s.document.dirty);
        s.run("undo", None, 0);
        assert_eq!(
            s.document.buffer.text(),
            "feature\n",
            "undo from the old branch cannot cross the checkout"
        );
    }

    #[test]
    fn staging_something_already_staged_is_refused_rather_than_run() {
        let mut s = with_tree();
        s.fill_scm(Some(
            deco_scm::parse(
                "# branch.oid 1c9d4e5\0# branch.head main\0\
                 1 M. N... 100644 100644 100644 aaaaaaa bbbbbbb work.rs\0",
            )
            .expect("git's own format"),
        ));
        s.run("workbench.view.scm", None, 0);

        // `git add` would succeed without effect, and the message would
        // incorrectly report that something was staged.
        assert!(matches!(s.run("git.stage", None, 0), Outcome::Message(_)));
        assert!(
            matches!(s.run("git.stageAll", None, 0), Outcome::Message(_)),
            "and there is nothing left to stage either"
        );
    }

    #[test]
    fn unstaging_something_that_is_not_staged_is_refused() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        s.run("workbench.view.scm", None, 0);
        assert!(matches!(s.run("git.unstage", None, 0), Outcome::Message(_)));
    }

    #[test]
    fn unstaging_a_copy_does_not_include_its_source() {
        let mut s = with_tree();
        s.fill_scm(Some(
            deco_scm::parse(
                "# branch.oid 1c9d4e5\0# branch.head main\0\
                 2 C. N... 100644 100644 100644 aaaaaaa bbbbbbb C100 copy.rs\0\
                 source.rs\0",
            )
            .expect("git's own format"),
        ));
        s.run("workbench.view.scm", None, 0);

        let Outcome::GitOperation(deco_scm::Operation::Unstage { path, original }) =
            s.run("git.unstage", None, 0)
        else {
            panic!("expected an unstage");
        };
        assert_eq!(path, PathBuf::from("copy.rs"));
        assert_eq!(
            original, None,
            "the copy source may have staged changes of its own"
        );
    }

    #[test]
    fn committing_with_nothing_staged_never_asks_for_a_message() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        s.run("workbench.view.scm", None, 0);

        // Rejected before the prompt opens, so a typed message is not lost.
        assert!(matches!(s.run("git.commit", None, 0), Outcome::Message(_)));
        assert!(s.prompt.is_none());
    }

    #[test]
    fn committing_asks_for_a_message_and_refuses_an_empty_one() {
        let mut s = with_tree();
        s.fill_scm(Some(
            deco_scm::parse(
                "# branch.oid 1c9d4e5\0# branch.head main\0\
                 1 M. N... 100644 100644 100644 aaaaaaa bbbbbbb work.rs\0",
            )
            .expect("git's own format"),
        ));
        s.run("workbench.view.scm", None, 0);
        s.run("git.commit", None, 0);
        assert_eq!(
            s.prompt.as_ref().map(|p| p.kind()),
            Some(PromptKind::CommitMessage)
        );

        // Enter on an empty input usually dismisses the prompt.
        let enter = Chord::parse("enter").expect("a key");
        assert!(matches!(s.handle_chord(enter, 0), Outcome::Message(_)));

        s.run("git.commit", None, 0);
        for c in "a real message".chars() {
            s.handle_chord(Chord::char(c), 0);
        }
        let Outcome::GitOperation(op) = s.handle_chord(enter, 0) else {
            panic!("expected a commit");
        };
        assert_eq!(op, deco_scm::Operation::Commit("a real message".to_owned()));
    }

    #[test]
    fn a_repository_change_asks_git_again_rather_than_guessing() {
        let mut s = with_tree();
        // As the frontend does: mark the run as started, then supply the result.
        s.scm_started();
        s.fill_scm(Some(dirty_status()));
        assert!(!s.scm_wanted(), "the status has been answered");

        // The view is not adjusted here. Only git knows the current index, and
        // a predicted state would diverge as soon as a hook ran.
        s.git_operation_done(&deco_scm::Operation::Stage(PathBuf::from("work.rs")));
        assert!(s.scm_wanted());
        assert_eq!(s.status.as_deref(), Some("staged work.rs"));
    }

    #[test]
    fn a_refused_change_still_asks_git_again() {
        let mut s = with_tree();
        s.fill_scm(Some(dirty_status()));
        // A failure usually means the view's state of the index was out of date.
        s.git_operation_failed(&deco_scm::Operation::StageAll, "index.lock exists");
        assert!(s.scm_wanted());
        assert!(s.status.as_deref().unwrap().contains("index.lock"));
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

        // `src` is removed outside deco. A refresh of the parent removes its row,
        // but its listing and expansion stay cached.
        s.fill_directory(
            Path::new("/w"),
            vec![crate::explorer::Entry::file("Cargo.toml")],
        );

        // deco tries to create a folder named `src`, because the tree shows the
        // name as free. It fails, because another program recreated `src` in
        // the meantime.
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

        // `b` is removed outside deco, and only the parent is refreshed.
        s.fill_directory(Path::new("/w"), vec![crate::explorer::Entry::dir("a")]);

        // Renaming `a` to the free name `b` fails, because another program
        // recreated `b` after the tree was read.
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
        // Selects the row for a path, regardless of the tree's shape. The test
        // must rename the intended row, and the clamp after a refill can move
        // the selection.
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

        // `a` contains a directory that is not expanded. After the rename its
        // listing must be requested rather than taken from the cache.
        focus(&mut s, "/w/a");
        s.run("list.expand", None, 0);
        s.fill_directory(
            Path::new("/w/a"),
            vec![
                crate::explorer::Entry::dir("sub"),
                crate::explorer::Entry::file("mine.rs"),
            ],
        );

        // `b` contains a directory with the same name, and *that* one is
        // expanded. The tree therefore caches state one level below `b` that
        // re-keying `a` cannot overwrite, because `a` has no listing that deep.
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

        // `b` is removed outside deco, and a later refresh of the parent removes
        // its row, but its listings and expansions stay cached.
        s.fill_directory(Path::new("/w"), vec![crate::explorer::Entry::dir("a")]);

        // Rename `a` to the free name `b`.
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

        // Delete something. `ctrl+z` must not then undo the older create.
        s.run("list.focusDown", None, 0);
        let Outcome::FileOperation(deleted) = s.delete_in_tree() else {
            panic!("expected an operation");
        };
        // The stack is cleared after the delete succeeds, not when it is
        // requested, so a failed delete keeps the earlier undo entries.
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

        // In the text, `ctrl+z` runs the document's undo.
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
        // Without a message, the key would appear to be ignored. The *state* is
        // kept, so widening the window shows the region without another press.
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
        // As in VS Code, showing the tree does not move focus out of the file.
        let mut s = session();
        s.resize(80, 24);
        press(&mut s, "ctrl+b");

        assert_eq!(s.focus(), Focus::Editor);
        assert_eq!(s.context.get("sideBarFocus"), Some(&json!(false)));
        assert_eq!(s.context.get("editorTextFocus"), Some(&json!(true)));
    }

    #[test]
    fn focusing_a_region_shows_it_first() {
        // A hidden region must never receive focus.
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
        // Every editing command acts on the document, which does not have focus.
        // The fallback for unbound printable keys is also blocked, which is why
        // the guard is on the command rather than on the binding.
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

        // Typing works again as soon as the editor has focus.
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
        // The panel exists, but the terminal that this key opens is not
        // implemented.
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
        // Different from "not implemented yet". This is usually a typo in a
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
        // A palette entry must work when chosen, so unimplemented commands are
        // not listed.
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
        // Uses the same path as a search result. For a document that is already
        // open, this switches to its own tab, so unsaved changes are kept.
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
        // `keybindings.json` refers to the identifier, and the title does not
        // show it.
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
        // The bar belongs to the tab, so switching away keeps it with the tab
        // instead of discarding it, and switching back restores it as it was.
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
        // With one match list per session, every switch had to discard it.
        let mut s = session();
        s.resize(80, 10);
        s.open(PathBuf::from("/w/a.txt"), "aaa\n");
        press(&mut s, "ctrl+f");
        press(&mut s, "a");
        s.open(PathBuf::from("/w/b.txt"), "bbbb\n");
        press(&mut s, "ctrl+f");
        // The query was carried over, so replace it.
        press(&mut s, "ctrl+x");
        press(&mut s, "b");
        assert_eq!(s.find.matches().len(), 4);

        press(&mut s, "ctrl+shift+tab");
        assert_eq!(s.find.query(), "a", "a.txt kept its own");
        assert_eq!(s.find.matches().len(), 3);
    }

    #[test]
    fn a_new_document_in_the_same_tab_still_drops_the_matches() {
        // `open` on an unmodified untitled tab replaces the document rather than
        // adding a tab, so the matches are stale.
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
        // This works because `accept_prompt` takes the prompt rather than
        // borrowing it.
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
