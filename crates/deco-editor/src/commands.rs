//! The command set, implemented once for every frontend.
//!
//! Commands are addressed by VS Code's identifiers, so a user's
//! `keybindings.json` reaches the same command deco runs by default. They
//! operate on a [`Document`] and a [`View`] and touch nothing else — no
//! terminal, no window — which is what lets the whole editable surface be
//! tested headlessly.

use deco_core::movement::{self, VerticalDirection};
use deco_core::{Buffer, Change, EditKind, Position, Range, Selection, SelectionSet, Transaction};
use serde_json::Value;

use crate::document::{block_comment_tokens, line_comment_token, Document, View};

/// Somewhere to put cut and copied text.
///
/// A trait rather than a concrete type because the terminal frontend, the GPU
/// frontend and the tests all have different ideas of what the clipboard is.
pub trait Clipboard {
    /// Reads the clipboard.
    fn read(&self) -> String;
    /// Writes the clipboard.
    fn write(&mut self, text: &str);
}

/// A clipboard that only exists in memory, used by tests and by frontends with
/// no system clipboard available.
#[derive(Debug, Default)]
pub struct MemoryClipboard {
    text: String,
}

impl Clipboard for MemoryClipboard {
    fn read(&self) -> String {
        self.text.clone()
    }

    fn write(&mut self, text: &str) {
        self.text = text.to_owned();
    }
}

/// What the frontend should do after a command ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The command ran; redraw.
    Handled,
    /// No command with that identifier exists here.
    ///
    /// Not necessarily an error: a frontend owns commands the core cannot
    /// implement, because they need something the core has no concept of — a
    /// language server, a window, a file dialog. The identifier is carried so it
    /// can be offered to whoever does own it.
    NotFound,
    /// The command exists but belongs to the frontend, which is given its name.
    ///
    /// Distinct from [`Outcome::NotFound`] so that a typo in a keybinding still
    /// reports as unknown rather than being silently handed onward and dropped.
    Frontend(String),
    /// The document should be written to disk.
    Save,
    /// The user chose a remembered decision to take back.
    ///
    /// The identifier is whatever the frontend put on the choice when it built
    /// the list: only the frontend knows which extension and capability it stands
    /// for, and the core has no reason to learn.
    ForgetExtensionPermission(String),
    /// The user answered an extension's permission request.
    ///
    /// Which extension and which capability are not carried: whoever opened the
    /// prompt is holding the request that is waiting on it, and putting a copy
    /// here would create two answers to keep in step.
    ExtensionConsent {
        /// Whether the extension may proceed.
        allow: bool,
    },
    /// The frontend should load this colour theme and hand it back with
    /// [`Session::set_theme`](crate::Session::set_theme).
    ///
    /// Named rather than loaded here for the usual reason: a theme that lives in
    /// an extension directory is a file, and the core has no filesystem.
    LoadTheme {
        /// What it is called, for reporting and for the `workbench.colorTheme`
        /// setting that would make the choice stick.
        label: String,
        /// The file to read, or `None` for a theme compiled in.
        path: Option<std::path::PathBuf>,
    },
    /// The document should be written to this path, which then becomes its own.
    ///
    /// The path is **exactly what was typed**. Resolving `~` and a relative path
    /// needs a home directory and a working directory, neither of which the core
    /// has — so the frontend resolves it, writes, and reports the path it settled
    /// on with [`Session::rename_to`](crate::Session::rename_to).
    SaveAs(std::path::PathBuf),
    /// The workspace should be searched for `query`.
    ///
    /// Carries the options rather than leaving the frontend to read the find
    /// bar's: a project search and the find bar are different searches, and
    /// "case-insensitive while I skim this file" and "case-sensitive across the
    /// project" are ordinary things to want at once.
    SearchInFiles {
        /// What to look for. Never empty — the session refuses before this.
        query: String,
        /// How to match, this search's own.
        options: deco_core::search::SearchOptions,
    },
    /// The symbol under the cursor should be renamed to `new_name`, everywhere.
    ///
    /// Only a frontend can carry this out: it takes a language server to find
    /// out what "everywhere" is, and a filesystem to read the files that answer
    /// names. The frontend asks the server, hands the reply to
    /// [`Session::plan_workspace_edit`](crate::Session::plan_workspace_edit),
    /// reads whatever files the plan asks for, and applies it with
    /// [`Session::apply_workspace_edit`](crate::Session::apply_workspace_edit).
    ///
    /// Carries no position: the cursor has not moved since the prompt opened —
    /// the prompt has the keyboard — and a copy kept here would be a second
    /// answer to keep in step with the first.
    Rename {
        /// What the user typed. Never empty, and never the current name: the
        /// session refuses both before this is produced.
        new_name: String,
    },
    /// Every occurrence of `query` in the workspace should become `replacement`.
    ///
    /// The frontend searches — only it knows where the files are — and then
    /// hands what it found to
    /// [`Session::plan_replacements`](crate::Session::plan_replacements), which
    /// decides what the edit is, and to
    /// [`Session::apply_workspace_edit`](crate::Session::apply_workspace_edit),
    /// which makes it one undoable action.
    ReplaceInFiles {
        /// What to look for. Never empty — the session refuses before this.
        query: String,
        /// What to put there. **May be empty**, which deletes every occurrence
        /// and is a thing people mean to do.
        replacement: String,
        /// How to match, the project search's own.
        options: deco_core::search::SearchOptions,
    },
    /// The user chose one of the code actions the frontend offered.
    ///
    /// The identifier is whatever the frontend put on the choice, as with
    /// [`Outcome::ForgetExtensionPermission`]: only the frontend is holding the
    /// list the server sent, and a copy kept here would be a second answer to
    /// keep in step with it.
    CodeAction(String),
    /// The document should be re-read from disk, throwing away the edits.
    ///
    /// The frontend reads `session.document.path` and hands the text back with
    /// [`Session::revert_to`](crate::Session::revert_to). Re-read rather than
    /// remembered: keeping a second copy of every open file to revert to would
    /// double what a large one costs, and re-reading is also what "revert" means
    /// when the file has changed underneath you.
    ///
    /// An untitled document never produces this — there is nothing to read, so
    /// the session reverts it to empty itself.
    Revert,
    /// Every unsaved document should be written to disk.
    ///
    /// Names no paths and no bytes, for the same reason [`Outcome::Save`] does not:
    /// the core has no filesystem. The frontend asks
    /// [`Session::unsaved`](crate::Session::unsaved) what to write and reports each
    /// success back with
    /// [`Session::mark_saved_at`](crate::Session::mark_saved_at), so a write that
    /// fails leaves that document dirty rather than looking saved.
    SaveAll,
    /// The editor should exit.
    Quit,
    /// Something worth telling the user.
    Message(String),
    /// The frontend should read this path, open it, and put the cursor at `at`.
    ///
    /// The core has no filesystem — `Document::from_file` is handed text, never a
    /// path to read — which is what keeps the whole editable surface testable
    /// without one. Quick open therefore names the file and lets the frontend
    /// fetch it, exactly as [`Outcome::Save`] names no bytes.
    OpenFile {
        /// What to open.
        path: std::path::PathBuf,
        /// Where to put the cursor, or `None` to leave it at the start.
        at: Option<deco_core::position::Position>,
    },
    /// The frontend should list this directory and hand the entries back with
    /// [`Session::fill_directory`](crate::Session::fill_directory).
    ///
    /// The file tree's half of the same bargain [`Outcome::OpenFile`] makes: the
    /// core decides *which* directory needs reading — that is a question about
    /// what is expanded and on screen — and whoever has a filesystem answers it.
    /// On a remote workspace that is the connection rather than `std::fs`, and
    /// the tree does not know the difference.
    ListDirectory(std::path::PathBuf),
    /// The frontend should carry out this change to the files themselves.
    ///
    /// The core has decided it is allowed — the name is a name, the path is
    /// inside the workspace, nothing is in the way — and has already recorded it
    /// on the explorer's undo stack. What is left is the part that needs a
    /// filesystem. The frontend reports back with
    /// [`Session::file_operation_failed`](crate::Session::file_operation_failed)
    /// if the disk disagrees, which takes the entry back off the stack.
    FileOperation(crate::files::Operation),
}

/// The title of a command deco binds but has not built, if `id` is one.
///
/// Used to turn an unhandled binding into a sentence rather than silence — see
/// [`PENDING`].
pub fn pending_title(id: &str) -> Option<&'static str> {
    PENDING
        .iter()
        .find(|(pending, _)| *pending == id)
        .map(|(_, title)| *title)
}

/// One entry in the command palette: what to run, and what to call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    /// The command identifier, VS Code's — or, for a quick-open entry, the path
    /// to open.
    pub id: String,
    /// The title to show, VS Code's wording where it has one.
    pub title: String,
    /// Where in the file to land, for an entry that names a place rather than a
    /// whole file.
    ///
    /// `None` for a command and for quick open, which opens a file at wherever
    /// the cursor last was; `Some` for a search result, which is a position.
    pub at: Option<deco_core::position::Position>,
    /// A second column, drawn right-aligned when there is room.
    ///
    /// For what the title does not say and the reader needs: a command's
    /// identifier, which is what a `keybindings.json` refers to, or a symbol's
    /// kind, which is what tells a field from a method of the same name. `None`
    /// for a file or a search result, whose title already is the path.
    pub detail: Option<String>,
}

impl PaletteEntry {
    /// Builds an entry from a borrowed pair.
    pub fn new(id: &str, title: &str) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
            at: None,
            detail: None,
        }
    }

    /// The same entry with a second column.
    pub fn with_detail(mut self, detail: &str) -> Self {
        self.detail = Some(detail.to_owned());
        self
    }

    /// Builds an entry that names a position within a file.
    pub fn at(id: &str, title: &str, at: deco_core::position::Position) -> Self {
        Self {
            at: Some(at),
            ..Self::new(id, title)
        }
    }
}

/// The commands the palette offers from this module.
///
/// Deliberately not every arm of [`execute`]: a palette entry has to *work* when
/// chosen, and the entries here are the ones that need nothing but a document, a
/// view and a clipboard. Commands that need something only a frontend has — a
/// language server, a window — are contributed by the frontend instead, through
/// `Session::frontend_commands`. A palette that offers something the editor cannot
/// do is worse than a short one.
///
/// Motions are left out on purpose. `cursorDown` is a keypress, not a thing anyone
/// looks up by name, and listing forty of them would bury the commands people do
/// look for.
/// Commands the default keymap binds that deco does not implement yet.
///
/// # Why this list exists
///
/// A key bound to a command nothing handles does *nothing at all*, and a key that
/// does nothing is indistinguishable from an editor that has stopped responding.
/// Naming them here turns silence into a sentence — `Split Editor is not
/// implemented yet` — and gives a test something to check: every command the
/// default keymap binds either has a handler or is on this list, so a new binding
/// cannot be added as a dead key by accident.
///
/// They are deliberately *not* in [`PALETTE`]. A palette entry has to work when
/// chosen; offering one that only apologises is worse than a shorter list.
///
/// An identifier leaves this list when the feature lands.
pub const PENDING: &[(&str, &str)] = &[
    // The panel exists now; what is missing is a terminal to put in it, which
    // needs a PTY dependency — see docs/roadmap.md.
    (
        "workbench.action.terminal.toggleTerminal",
        "Toggle Terminal",
    ),
    ("workbench.action.toggleZenMode", "Zen Mode"),
    // Needs a font size the frontend can change, which the terminal does not own.
    ("workbench.action.zoomIn", "Zoom In"),
    ("workbench.action.zoomOut", "Zoom Out"),
    ("workbench.action.zoomReset", "Reset Zoom"),
    // Needs a new workspace root, which the file walk, the search and the language
    // servers are all anchored to.
    ("workbench.action.files.openFolder", "Open Folder"),
    // Needs an editor for a file deco reads but never writes.
    ("workbench.action.openSettings", "Open Settings"),
    (
        "workbench.action.openGlobalKeybindings",
        "Open Keyboard Shortcuts",
    ),
    // `deco --server` exists now; what does not is the client half — nothing in
    // the editor opens a file through a transport, so a menu would offer a list of
    // places it cannot go.
    ("deco.remote.showMenu", "Remote Menu"),
];

pub const PALETTE: &[(&str, &str)] = &[
    ("undo", "Undo"),
    ("redo", "Redo"),
    ("editor.action.selectAll", "Select All"),
    ("expandLineSelection", "Expand Line Selection"),
    ("removeSecondaryCursors", "Remove Secondary Cursors"),
    ("editor.action.commentLine", "Toggle Line Comment"),
    ("editor.action.addCommentLine", "Add Line Comment"),
    ("editor.action.removeCommentLine", "Remove Line Comment"),
    ("editor.action.blockComment", "Toggle Block Comment"),
    ("editor.action.toggleWordWrap", "View: Toggle Word Wrap"),
    (
        "workbench.action.toggleSidebarVisibility",
        "View: Toggle Primary Side Bar Visibility",
    ),
    (
        "workbench.action.togglePanel",
        "View: Toggle Panel Visibility",
    ),
    (
        "workbench.action.focusSideBar",
        "View: Focus into Primary Side Bar",
    ),
    ("workbench.action.focusPanel", "View: Focus into Panel"),
    (
        "workbench.action.focusActiveEditorGroup",
        "View: Focus Active Editor Group",
    ),
    ("editor.action.indentLines", "Indent Lines"),
    ("editor.action.outdentLines", "Outdent Lines"),
    ("editor.action.deleteLines", "Delete Line"),
    ("editor.action.insertLineAfter", "Insert Line Below"),
    ("editor.action.insertLineBefore", "Insert Line Above"),
    ("editor.action.moveLinesUpAction", "Move Line Up"),
    ("editor.action.moveLinesDownAction", "Move Line Down"),
    ("editor.action.copyLinesUpAction", "Copy Line Up"),
    ("editor.action.copyLinesDownAction", "Copy Line Down"),
    (
        "editor.action.addSelectionToNextFindMatch",
        "Add Selection To Next Find Match",
    ),
    ("editor.action.selectHighlights", "Select All Occurrences"),
    (
        "editor.action.moveSelectionToNextFindMatch",
        "Move Last Selection To Next Find Match",
    ),
    ("editor.action.clipboardCopyAction", "Copy"),
    ("editor.action.clipboardCutAction", "Cut"),
    ("editor.action.clipboardPasteAction", "Paste"),
    // Implemented by `Session::run` rather than by `execute`, because they need
    // the whole session. Listed here all the same: what matters to the palette is
    // that the editor runs them, not which function does.
    ("actions.find", "Find"),
    ("editor.action.startFindReplaceAction", "Replace"),
    ("editor.action.nextMatchFindAction", "Find Next"),
    ("editor.action.previousMatchFindAction", "Find Previous"),
    ("workbench.action.gotoLine", "Go to Line"),
    ("editor.action.marker.next", "Go to Next Problem"),
    ("editor.action.marker.prev", "Go to Previous Problem"),
    ("workbench.action.files.save", "Save"),
    ("workbench.action.splitEditor", "Split Editor"),
    ("workbench.action.files.saveAll", "Save All"),
    ("workbench.action.files.revert", "Revert File"),
    (
        "workbench.action.revertAndCloseActiveEditor",
        "Revert and Close Editor",
    ),
    ("workbench.action.files.saveAs", "Save As"),
    ("workbench.action.files.openFile", "Open File"),
    (
        "workbench.action.editor.changeLanguageMode",
        "Change Language Mode",
    ),
    ("workbench.action.quit", "Quit"),
    ("workbench.action.nextEditor", "Next Editor"),
    ("workbench.action.previousEditor", "Previous Editor"),
    ("workbench.action.closeActiveEditor", "Close Editor"),
    (
        "workbench.action.files.newUntitledFile",
        "New Untitled File",
    ),
];

/// Everything a command may touch.
pub struct Context<'a> {
    /// The document being edited.
    pub document: &'a mut Document,
    /// The view onto it.
    pub view: &'a mut View,
    /// Where cut and copy put their text.
    pub clipboard: &'a mut dyn Clipboard,
    /// A monotonic timestamp, used for undo grouping. Supplied by the frontend
    /// so that command behaviour stays deterministic under test.
    pub now_ms: u64,
}

/// Runs `command`.
pub fn execute(ctx: &mut Context<'_>, command: &str, args: Option<&Value>) -> Outcome {
    // Motions come first: they are by far the most frequent, and every one of
    // them shares the same "extend or not" shape.
    if let Some(outcome) = motion(ctx, command) {
        return outcome;
    }

    let outcome = match command {
        "type" => {
            let text = args
                .and_then(|a| a.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if text.is_empty() {
                return Outcome::Handled;
            }
            if let Some(outcome) = auto_close(ctx, text) {
                return outcome;
            }
            if text == "\n" {
                if let Some(outcome) = insert_newline(ctx) {
                    return outcome;
                }
            }
            insert_text(ctx, text);
            Outcome::Handled
        }
        "tab" => {
            indent_or_insert(ctx);
            Outcome::Handled
        }
        "outdent" | "editor.action.outdentLines" => {
            change_indent(ctx, false);
            Outcome::Handled
        }
        "editor.action.indentLines" => {
            change_indent(ctx, true);
            Outcome::Handled
        }
        "deleteLeft" => {
            delete_directional(ctx, true, false);
            Outcome::Handled
        }
        "deleteRight" => {
            delete_directional(ctx, false, false);
            Outcome::Handled
        }
        "deleteWordLeft" => {
            delete_directional(ctx, true, true);
            Outcome::Handled
        }
        "deleteWordRight" => {
            delete_directional(ctx, false, true);
            Outcome::Handled
        }
        "undo" => {
            // The history applies its own transaction, so this cannot go through
            // `Document::apply`; invalidate wholesale instead of guessing which
            // lines an undo touched.
            ctx.document.invalidate();
            if let Some(selections) = ctx.document.history.undo(&mut ctx.document.buffer) {
                ctx.view.selections = selections;
                ctx.document.dirty = true;
            }
            Outcome::Handled
        }
        "redo" => {
            ctx.document.invalidate();
            if let Some(selections) = ctx.document.history.redo(&mut ctx.document.buffer) {
                ctx.view.selections = selections;
                ctx.document.dirty = true;
            }
            Outcome::Handled
        }
        "editor.action.selectAll" => {
            let end = ctx.document.buffer.end_position();
            ctx.view.selections = SelectionSet::single(Selection::new(Position::ZERO, end));
            Outcome::Handled
        }
        "expandLineSelection" => {
            expand_line_selection(ctx);
            Outcome::Handled
        }
        "removeSecondaryCursors" => {
            ctx.view.selections.collapse_to_primary();
            Outcome::Handled
        }
        "cancelSelection" => {
            ctx.view.selections.map(|s| s.collapsed());
            Outcome::Handled
        }
        "editor.action.addSelectionToNextFindMatch" => add_next_match(ctx),
        "editor.action.selectHighlights" => select_all_matches(ctx),
        "editor.action.moveSelectionToNextFindMatch" => move_to_next_match(ctx),
        "editor.action.insertCursorBelow" => {
            add_cursor(ctx, 1);
            Outcome::Handled
        }
        "editor.action.insertCursorAbove" => {
            add_cursor(ctx, -1);
            Outcome::Handled
        }
        "editor.action.deleteLines" => {
            delete_lines(ctx);
            Outcome::Handled
        }
        "editor.action.insertLineAfter" => {
            insert_line(ctx, true);
            Outcome::Handled
        }
        "editor.action.insertLineBefore" => {
            insert_line(ctx, false);
            Outcome::Handled
        }
        "editor.action.moveLinesUpAction" => {
            move_lines(ctx, true);
            Outcome::Handled
        }
        "editor.action.moveLinesDownAction" => {
            move_lines(ctx, false);
            Outcome::Handled
        }
        "editor.action.copyLinesUpAction" => {
            copy_lines(ctx, true);
            Outcome::Handled
        }
        "editor.action.copyLinesDownAction" => {
            copy_lines(ctx, false);
            Outcome::Handled
        }
        "editor.action.commentLine" => {
            line_comment(ctx, CommentMode::Toggle);
            Outcome::Handled
        }
        "editor.action.addCommentLine" => {
            line_comment(ctx, CommentMode::Add);
            Outcome::Handled
        }
        "editor.action.blockComment" => {
            block_comment(ctx);
            Outcome::Handled
        }
        "editor.action.removeCommentLine" => {
            line_comment(ctx, CommentMode::Remove);
            Outcome::Handled
        }
        "editor.action.clipboardCopyAction" => {
            copy_selection(ctx);
            Outcome::Handled
        }
        "editor.action.clipboardCutAction" => {
            copy_selection(ctx);
            delete_selection_or_line(ctx);
            Outcome::Handled
        }
        "editor.action.clipboardPasteAction" => {
            let text = ctx.clipboard.read();
            if !text.is_empty() {
                insert_text(ctx, &text);
            }
            Outcome::Handled
        }
        "workbench.action.files.save" => Outcome::Save,
        "workbench.action.files.saveAll" => Outcome::SaveAll,
        "workbench.action.quit" | "workbench.action.closeWindow" => Outcome::Quit,
        _ => Outcome::NotFound,
    };

    if outcome == Outcome::Handled {
        ctx.view
            .reveal_cursor(&ctx.document.buffer, &ctx.document.settings);
    }
    outcome
}

/// Handles every cursor motion, or returns `None` if `command` is not one.
fn motion(ctx: &mut Context<'_>, command: &str) -> Option<Outcome> {
    let (base, extend) = match command.strip_suffix("Select") {
        Some(base) => (base, true),
        None => (command, false),
    };

    let buffer = &ctx.document.buffer;
    let separators = ctx.document.settings.word_separators.clone();
    let tab_size = ctx.document.settings.tab_size;
    let page = ctx.view.height.saturating_sub(1).max(1) as u32;

    // Vertical motions carry a sticky goal column, so they are applied through
    // `movement::vertical` rather than by computing a bare position.
    let vertical = match base {
        "cursorUp" => Some((VerticalDirection::Up, 1)),
        "cursorDown" => Some((VerticalDirection::Down, 1)),
        "cursorPageUp" => Some((VerticalDirection::Up, page)),
        "cursorPageDown" => Some((VerticalDirection::Down, page)),
        _ => None,
    };
    if let Some((direction, count)) = vertical {
        // Rows rather than lines while wrapping, because on screen a row is what
        // one press of the key looks like it moves by. The goal column is measured
        // within the row for the same reason.
        if ctx.view.wrap_column(&ctx.document.settings) > 0 {
            let view = &*ctx.view;
            let settings = &ctx.document.settings;
            let down = direction == VerticalDirection::Down;
            // Collected before assigning, because working out where a row is takes
            // the view and the selections live on it.
            let moved: Vec<Selection> = view
                .selections
                .iter()
                .map(|selection| {
                    let from = buffer.clamp_position(selection.active);
                    let goal = selection
                        .goal_column
                        .map(|goal| goal as usize)
                        .unwrap_or_else(|| view.goal_column(buffer, settings, from));
                    let to = view.step_rows(buffer, settings, from, down, count as usize, goal);
                    let mut next = if extend {
                        selection.extended_to(to)
                    } else {
                        selection.moved_to(to)
                    };
                    next.goal_column = Some(goal as u32);
                    next
                })
                .collect();
            let primary = view.selections.primary_index();
            ctx.view.selections = SelectionSet::from_vec(moved, primary);
        } else {
            ctx.view
                .selections
                .map(|s| movement::vertical(buffer, *s, direction, count, tab_size, extend));
        }
        ctx.view
            .reveal_cursor(&ctx.document.buffer, &ctx.document.settings);
        ctx.document.history.break_group();
        return Some(Outcome::Handled);
    }

    // `home` and `end` mean the ends of the row while wrapping, which is where the
    // key points on screen. On a line's first and last row those are the line's
    // ends, so an unwrapped line behaves exactly as it always did.
    if matches!(base, "cursorHome" | "cursorEnd")
        && ctx.view.wrap_column(&ctx.document.settings) > 0
    {
        let view = &*ctx.view;
        let settings = &ctx.document.settings;
        let home = base == "cursorHome";
        let moved: Vec<Selection> = view
            .selections
            .iter()
            .map(|selection| {
                let from = buffer.clamp_position(selection.active);
                let (start, end) = view.row_bounds(buffer, settings, from);
                let to = if home {
                    if start == 0 {
                        // The first row keeps `home`'s usual trick of stopping at
                        // the first non-whitespace and toggling to column zero.
                        movement::smart_home(buffer, from)
                    } else {
                        Position::new(from.line, start)
                    }
                } else {
                    match end {
                        None => movement::line_end(buffer, from),
                        // One short of the next row's start, or `end` would put the
                        // caret on the row below the one whose end was asked for.
                        Some(_) => {
                            let text = buffer
                                .line_content(from.line as usize)
                                .map(|slice| slice.to_string())
                                .unwrap_or_default();
                            Position::new(
                                from.line,
                                deco_core::wrap::column_in_row(
                                    &text,
                                    start,
                                    end,
                                    usize::MAX,
                                    settings.tab_size,
                                ),
                            )
                        }
                    }
                };
                let mut next = if extend {
                    selection.extended_to(to)
                } else {
                    selection.moved_to(to)
                };
                // Cleared, so a following `down` measures the column afresh from
                // where `home` or `end` actually landed.
                next.goal_column = None;
                next
            })
            .collect();
        let primary = view.selections.primary_index();
        ctx.view.selections = SelectionSet::from_vec(moved, primary);
        ctx.view
            .reveal_cursor(&ctx.document.buffer, &ctx.document.settings);
        ctx.document.history.break_group();
        return Some(Outcome::Handled);
    }

    let target: fn(&Buffer, Position, &str) -> Position = match base {
        "cursorLeft" => |b, p, _| movement::grapheme_left(b, p),
        "cursorRight" => |b, p, _| movement::grapheme_right(b, p),
        "cursorWordLeft" | "cursorWordStartLeft" => movement::word_start_left,
        "cursorWordRight" | "cursorWordEndRight" => movement::word_end_right,
        "cursorHome" => |b, p, _| movement::smart_home(b, p),
        "cursorEnd" => |b, p, _| movement::line_end(b, p),
        "cursorTop" => |_, _, _| Position::ZERO,
        "cursorBottom" => |b, _, _| b.end_position(),
        _ => return None,
    };

    ctx.view.selections.map(|s| {
        // A non-extending left/right motion with a selection collapses to that
        // selection's edge rather than moving from the caret, which is what
        // makes arrow keys feel right after a drag.
        if !extend && !s.is_empty() {
            match base {
                "cursorLeft" => return s.moved_to(s.start()),
                "cursorRight" => return s.moved_to(s.end()),
                _ => {}
            }
        }
        let position = target(buffer, s.active, &separators);
        if extend {
            s.extended_to(position)
        } else {
            s.moved_to(position)
        }
    });

    ctx.view
        .reveal_cursor(&ctx.document.buffer, &ctx.document.settings);
    // Moving the caret ends the current typing group, so the next character
    // typed starts a fresh undo step.
    ctx.document.history.break_group();
    Some(Outcome::Handled)
}

/// Applies one change per selection and places a caret after each insertion.
///
/// The running delta is what makes this correct for multiple cursors: change
/// *i*'s post-edit position depends on the net length change of every earlier
/// one.
fn edit_at_selections(
    ctx: &mut Context<'_>,
    kind: EditKind,
    mut plan: impl FnMut(&Buffer, &Selection) -> Option<(Range, String)>,
) {
    let before = ctx.view.selections.clone();
    let buffer = &ctx.document.buffer;

    // `false` for a caret, `true` for a trim: only the edits that came from a
    // selection leave a cursor behind, but both have to be in the transaction and
    // both shift what follows them.
    let mut planned: Vec<(usize, usize, String, bool)> = Vec::new();
    for selection in before.iter() {
        let Some((range, text)) = plan(buffer, selection) else {
            continue;
        };
        let range = buffer.clamp_range(range);
        planned.push((
            buffer.position_to_char(range.start),
            buffer.position_to_char(range.end),
            text,
            false,
        ));
    }
    if planned.is_empty() {
        return;
    }
    // `editor.trimAutoWhitespace`, folded into this edit rather than applied as one
    // of its own: an auto-indent left behind and the keystroke that left it behind
    // are one action, so `ctrl+z` should take back one thing.
    let kept: Vec<(Position, Position, bool)> = planned
        .iter()
        .map(|(start, end, text, _)| {
            (
                buffer.char_to_position(*start),
                buffer.char_to_position(*end),
                text.starts_with('\n'),
            )
        })
        .collect();
    for range in trimmable_whitespace(ctx.document, &kept) {
        planned.push((
            buffer.position_to_char(range.start),
            buffer.position_to_char(range.end),
            String::new(),
            true,
        ));
    }
    planned.sort_by_key(|(start, end, _, _)| (*start, *end));

    let changes: Vec<Change> = planned
        .iter()
        .map(|(start, end, text, _)| {
            Change::replace(
                Range::new(
                    buffer.char_to_position(*start),
                    buffer.char_to_position(*end),
                ),
                text.clone(),
            )
        })
        .collect();

    let Ok(transaction) = Transaction::new(changes) else {
        // Overlapping edits mean two cursors are fighting over the same text;
        // dropping the whole thing is safer than applying half of it.
        return;
    };

    let inverse = ctx.document.apply(&transaction);

    let mut carets = Vec::with_capacity(planned.len());
    let mut delta: isize = 0;
    for (start, end, text, trimmed) in &planned {
        let inserted = text.chars().count();
        let new_start = (*start as isize + delta) as usize;
        // A trim is nobody's cursor. It still counts towards the delta, which is what
        // keeps the cursors after it in the right place.
        if !trimmed {
            carets.push(Selection::caret(
                ctx.document.buffer.char_to_position(new_start + inserted),
            ));
        }
        delta += inserted as isize - (*end - *start) as isize;
    }

    // Every edit invalidates the record: whitespace on a line that has just been
    // edited is that line's indentation now, and whitespace this call has trimmed is
    // gone. `insert_newline` writes the new record after it returns.
    ctx.document.auto_whitespace.clear();
    let after = SelectionSet::from_vec(carets, 0);
    ctx.view.selections = after.clone();
    ctx.document
        .history
        .record(inverse, kind, before, after, ctx.now_ms);
    ctx.document.dirty = true;
}

/// Replaces each selection with `text`.
/// The auto-indents an edit may take back with it.
///
/// `changes` is what the edit is about to do, as `(start, end, starts with a newline)`
/// per change. Two conditions, and both are about not deleting anything anybody wants:
///
/// - The line must *still* hold exactly the whitespace that was put there and nothing
///   else, checked against the buffer rather than trusted from the record. A stale
///   entry then costs nothing instead of costing somebody their text.
/// - No change may leave content on the line. A change that inserts a newline at or
///   past the whitespace is the one exception, and the case the feature exists for:
///   pressing enter on a freshly indented line is how the line gets abandoned, and
///   what stays behind is exactly the whitespace to take back.
fn trimmable_whitespace(document: &Document, changes: &[(Position, Position, bool)]) -> Vec<Range> {
    if !document.settings.trim_auto_whitespace {
        return Vec::new();
    }
    document
        .auto_whitespace
        .iter()
        .filter(|(line, columns)| {
            let leaves_content = changes.iter().any(|(start, end, newline)| {
                if start.line > *line || end.line < *line {
                    return false;
                }
                // Anything spanning the line, or landing inside its whitespace, or
                // putting text other than a line break on it.
                !(*newline && start.line == *line && end == start && start.character >= *columns)
            });
            if leaves_content {
                return false;
            }
            let text = line_text(&document.buffer, *line);
            let width: u32 = text.chars().map(|c| c.len_utf16() as u32).sum();
            width == *columns && width > 0 && text.chars().all(char::is_whitespace)
        })
        .map(|(line, columns)| Range::new(Position::new(*line, 0), Position::new(*line, *columns)))
        .collect()
}

/// `editor.autoIndent`: starts the new line where the old one started.
///
/// `None` when the setting is off or there is nothing to indent, so the caller falls
/// back to a plain newline.
///
/// # Why `enter` needed this and `ctrl+enter` did not
///
/// `editor.action.insertLineAfter` has always copied the line's indentation, because
/// it is a command that knows it is making a line. `enter` is bound to `type` with a
/// newline in it — a plain insertion — so it landed at column zero, and the same
/// editor indented on one key and not the other.
///
/// # Between a pair of brackets
///
/// With `brackets` (the default), `{|}` and `enter` puts the closer on its own line
/// at the outer indent and leaves the caret on an indented line between them, which
/// is the shape everybody types next. It pairs with
/// [`auto_close`]: typing `{` produces `{}`, and `enter` opens it into a block.
///
/// All the carets have to be between a pair for that, or one keystroke would open a
/// block under some and not others — the same rule, for the same reason, as closing a
/// bracket.
fn insert_newline(ctx: &mut Context<'_>) -> Option<Outcome> {
    use deco_config::AutoIndent;
    let mode = ctx.document.settings.auto_indent;
    if mode == AutoIndent::None {
        return None;
    }
    // A selection is replaced by the newline, and the indentation of a line that is
    // about to be partly deleted is not the indentation of what is left.
    if ctx.view.selections.iter().any(|s| !s.is_empty()) {
        return None;
    }

    let unit = ctx.document.indent_unit();
    let pairs = crate::document::bracket_pairs(ctx.document.language());
    let buffer = &ctx.document.buffer;

    // Only the leading whitespace *before* the caret: pressing enter inside a line's
    // indentation should not hand the new line more indent than the caret had.
    let indent_at = |buffer: &Buffer, at: Position| -> String {
        let text = line_text(buffer, at.line);
        text.chars()
            .take_while(|c| c.is_whitespace())
            .take(at.character as usize)
            .collect()
    };

    // Whether every caret sits between an opening bracket and its own closer.
    let between_pair = mode == AutoIndent::Brackets
        && ctx.view.selections.iter().all(|s| {
            let at = buffer.clamp_position(s.active);
            let before = previous_char(buffer, at);
            let after = next_char(buffer, at);
            matches!(
                (before, after),
                (Some(open), Some(close))
                    if pairs.iter().any(|(o, c)| *o == open && *c == close)
            )
        });
    // And whether every caret merely *follows* one, which only adds a level.
    let after_opener = mode == AutoIndent::Brackets
        && ctx.view.selections.iter().all(|s| {
            let at = buffer.clamp_position(s.active);
            previous_char(buffer, at).is_some_and(|c| pairs.iter().any(|(o, _)| *o == c))
        });

    if !between_pair && !after_opener && indentation_is_absent(buffer, &ctx.view.selections) {
        // Nothing to copy and no bracket to open: a plain newline is the same answer
        // and a cheaper one.
        return None;
    }

    edit_at_selections(ctx, EditKind::Insert, |buffer, selection| {
        let at = buffer.clamp_position(selection.active);
        let base = indent_at(buffer, at);
        let inner = if between_pair || after_opener {
            format!("{base}{unit}")
        } else {
            base.clone()
        };
        let text = if between_pair {
            format!("\n{inner}\n{base}")
        } else {
            format!("\n{inner}")
        };
        Some((selection.range(), text))
    });

    // Recorded after the edit: each caret sits just past the indent this call put
    // there, so its column *is* how much whitespace to take back.
    let recorded: Vec<(u32, u32)> = ctx
        .view
        .selections
        .iter()
        .filter(|s| s.active.character > 0)
        .map(|s| (s.active.line, s.active.character))
        .collect();

    if between_pair {
        // The caret is after the closer's indent on the last inserted line; it belongs
        // at the end of the one above. Read off the buffer rather than counted back,
        // so every caret lands right whatever its own indent was.
        ctx.view.selections.map(|s| {
            let line = s.active.line.saturating_sub(1);
            let end = movement::line_end(&ctx.document.buffer, Position::new(line, 0));
            s.moved_to(end)
        });
    }
    ctx.document.auto_whitespace = recorded;
    ctx.view
        .reveal_cursor(&ctx.document.buffer, &ctx.document.settings);
    Some(Outcome::Handled)
}

/// Whether no caret has any indentation to carry to a new line.
fn indentation_is_absent(buffer: &Buffer, selections: &SelectionSet) -> bool {
    selections.iter().all(|s| {
        let at = buffer.clamp_position(s.active);
        at.character == 0
            || !line_text(buffer, at.line)
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
    })
}

/// The character before `at`, or `None` at the start of a line.
fn previous_char(buffer: &Buffer, at: Position) -> Option<char> {
    let at = buffer.clamp_position(at);
    if at.character == 0 {
        return None;
    }
    let line = line_text(buffer, at.line);
    let mut last = None;
    let mut column = 0u32;
    for c in line.chars() {
        if column >= at.character {
            break;
        }
        column += c.len_utf16() as u32;
        last = Some(c);
    }
    last
}

/// `editor.autoClosingBrackets`: closes a bracket the caret has just opened, and
/// steps over one it has already closed.
///
/// `None` when this keystroke is an ordinary insertion, so the caller carries on.
///
/// # What it deliberately does not do
///
/// - **Surround a selection.** Typing `(` with text selected replaces it, as it
///   always has. Wrapping instead is `editor.autoSurround`, a separate setting deco
///   does not read — and closing a bracket *around* a replacement while leaving the
///   replacement out would be neither behaviour.
/// - **Remember which closers it inserted.** Typing `)` in front of any `)` steps
///   over it. VS Code tracks the ones it added and only steps over those; the state
///   that needs is a per-document list invalidated by every other edit, and the two
///   answers differ only where somebody typed both halves of a pair by hand and then
///   typed a third closer.
fn auto_close(ctx: &mut Context<'_>, text: &str) -> Option<Outcome> {
    let mode = ctx.document.settings.auto_closing_brackets;
    if mode == deco_config::AutoClosingBrackets::Never {
        return None;
    }
    // One character, typed: a paste or a multi-character insertion is not somebody
    // reaching for a bracket.
    let mut chars = text.chars();
    let typed = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    // A selection is replaced, so there is no caret to put a closer after.
    if ctx.view.selections.iter().any(|s| !s.is_empty()) {
        return None;
    }

    let pairs = crate::document::bracket_pairs(ctx.document.language());
    let after = |ctx: &Context<'_>, at: Position| next_char(&ctx.document.buffer, at);

    // Stepping over a closer comes first: `"` both opens and closes, and in front of
    // one the useful answer is to move past it rather than to open another.
    if pairs.iter().any(|(_, close)| *close == typed)
        && ctx
            .view
            .selections
            .iter()
            .all(|s| after(ctx, s.active) == Some(typed))
    {
        ctx.view.selections.map(|s| {
            let over = deco_core::movement::grapheme_right(&ctx.document.buffer, s.active);
            s.moved_to(over)
        });
        ctx.document.history.break_group();
        return Some(Outcome::Handled);
    }

    let (_, closer) = pairs.iter().find(|(open, _)| *open == typed)?;
    // Every caret has to agree, or one keystroke would insert a pair in some places
    // and a bare bracket in others.
    if !ctx
        .view
        .selections
        .iter()
        .all(|s| mode.closes_before(after(ctx, s.active)))
    {
        return None;
    }

    let closer = *closer;
    let pair = format!("{typed}{closer}");
    edit_at_selections(ctx, EditKind::Insert, |_, selection| {
        Some((selection.range(), pair.clone()))
    });
    // Back between the two, which is the point. One column: every character in a
    // pair here is ASCII, so one UTF-16 unit.
    ctx.view.selections.map(|s| {
        let at = s.active;
        s.moved_to(Position::new(at.line, at.character.saturating_sub(1)))
    });
    Some(Outcome::Handled)
}

/// The character after `at`, or `None` at the end of a line.
fn next_char(buffer: &Buffer, at: Position) -> Option<char> {
    let at = buffer.clamp_position(at);
    let line = line_text(buffer, at.line);
    let mut column = 0u32;
    for c in line.chars() {
        if column == at.character {
            return Some(c);
        }
        column += c.len_utf16() as u32;
    }
    None
}

fn insert_text(ctx: &mut Context<'_>, text: &str) {
    edit_at_selections(ctx, EditKind::Insert, |_, selection| {
        Some((selection.range(), text.to_owned()))
    });
}

/// `Tab`: indents the selected lines, or inserts one indent unit.
fn indent_or_insert(ctx: &mut Context<'_>) {
    let spans_lines = ctx
        .view
        .selections
        .iter()
        .any(|s| !s.is_empty() && !s.range().is_single_line());
    if spans_lines {
        change_indent(ctx, true);
        return;
    }
    let unit = ctx.document.indent_unit();
    insert_text(ctx, &unit);
}

/// Adds or removes one indent level on every selected line.
fn change_indent(ctx: &mut Context<'_>, add: bool) {
    let unit = ctx.document.indent_unit();
    let tab_size = ctx.document.settings.tab_size;
    let before = ctx.view.selections.clone();
    let lines = selected_lines(&before, &ctx.document.buffer);

    let mut changes = Vec::new();
    for line in lines {
        let text = line_text(&ctx.document.buffer, line);
        if add {
            if text.trim().is_empty() && text.is_empty() {
                continue;
            }
            changes.push(Change::insert(Position::new(line, 0), unit.clone()));
        } else {
            // Remove up to one indent unit's worth of leading whitespace,
            // stopping at the first non-whitespace character.
            let mut removed = 0usize;
            let mut columns = 0usize;
            for c in text.chars() {
                if columns >= tab_size {
                    break;
                }
                match c {
                    ' ' => {
                        columns += 1;
                        removed += 1;
                    }
                    '\t' => {
                        columns = tab_size;
                        removed += 1;
                    }
                    _ => break,
                }
            }
            if removed > 0 {
                changes.push(Change::delete(Range::new(
                    Position::new(line, 0),
                    Position::new(line, removed as u32),
                )));
            }
        }
    }
    apply_line_changes(ctx, changes, before);
}

/// Backspace and Delete, optionally by word.
fn delete_directional(ctx: &mut Context<'_>, backwards: bool, by_word: bool) {
    let separators = ctx.document.settings.word_separators.clone();
    let indent_unit = ctx.document.indent_unit();
    let insert_spaces = ctx.document.settings.insert_spaces;
    let tab_size = ctx.document.settings.tab_size;

    edit_at_selections(ctx, EditKind::Delete, |buffer, selection| {
        if !selection.is_empty() {
            return Some((selection.range(), String::new()));
        }
        let caret = selection.active;
        let other = if backwards {
            if by_word {
                movement::word_start_left(buffer, caret, &separators)
            } else {
                // Backspacing through leading indentation removes a whole
                // indent level, matching VS Code.
                let line = line_text(buffer, caret.line);
                let prefix: String = line.chars().take(caret.character as usize).collect();
                if insert_spaces
                    && caret.character > 0
                    && prefix.chars().all(|c| c == ' ')
                    && caret.character as usize % tab_size == 0
                {
                    Position::new(caret.line, caret.character - indent_unit.len() as u32)
                } else {
                    movement::grapheme_left(buffer, caret)
                }
            }
        } else if by_word {
            movement::word_end_right(buffer, caret, &separators)
        } else {
            movement::grapheme_right(buffer, caret)
        };
        if other == caret {
            return None;
        }
        Some((Range::ordered(caret, other), String::new()))
    });
}

/// Grows each selection to cover whole lines.
fn expand_line_selection(ctx: &mut Context<'_>) {
    let buffer = &ctx.document.buffer;
    let last = buffer.line_count() - 1;
    ctx.view.selections.map(|s| {
        let start = Position::new(s.start().line, 0);
        let end = if (s.end().line as usize) < last {
            Position::new(s.end().line + 1, 0)
        } else {
            buffer.end_position()
        };
        Selection::new(start, end)
    });
}

/// Adds a cursor one line above or below the primary one.
fn add_cursor(ctx: &mut Context<'_>, delta: i32) {
    let buffer = &ctx.document.buffer;
    let primary = *ctx.view.selections.primary();
    let line = primary.active.line as i64 + delta as i64;
    if line < 0 || line as usize >= buffer.line_count() {
        return;
    }
    let position = buffer.clamp_position(Position::new(line as u32, primary.active.character));
    ctx.view.selections.add(Selection::caret(position));
}

/// The lines any selection touches, ascending and deduplicated.
fn selected_lines(selections: &SelectionSet, buffer: &Buffer) -> Vec<u32> {
    let last = (buffer.line_count() - 1) as u32;
    let mut lines: Vec<u32> = Vec::new();
    for selection in selections.iter() {
        let start = selection.start().line.min(last);
        // A selection ending exactly at column 0 does not include that line,
        // which is what stops a full-line selection from deleting two lines.
        let mut end = selection.end().line.min(last);
        if selection.end().character == 0 && end > start {
            end -= 1;
        }
        for line in start..=end {
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
    }
    lines.sort_unstable();
    lines
}

/// A line's text without its terminator.
fn line_text(buffer: &Buffer, line: u32) -> String {
    buffer
        .line_content(line as usize)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Applies whole-line changes and keeps the cursors roughly where they were.
fn apply_line_changes(ctx: &mut Context<'_>, changes: Vec<Change>, before: SelectionSet) {
    if changes.is_empty() {
        return;
    }
    let Ok(transaction) = Transaction::new(changes) else {
        return;
    };
    let inverse = ctx.document.apply(&transaction);

    let after = SelectionSet::from_vec(
        before
            .iter()
            .map(|s| {
                Selection::new(
                    ctx.document.buffer.clamp_position(s.anchor),
                    ctx.document.buffer.clamp_position(s.active),
                )
            })
            .collect(),
        before.primary_index(),
    );
    ctx.view.selections = after.clone();
    ctx.document
        .history
        .record(inverse, EditKind::Discrete, before, after, ctx.now_ms);
    ctx.document.dirty = true;
}

/// Deletes every line any cursor touches.
fn delete_lines(ctx: &mut Context<'_>) {
    let before = ctx.view.selections.clone();
    let buffer = &ctx.document.buffer;
    let last = (buffer.line_count() - 1) as u32;
    let lines = selected_lines(&before, buffer);

    let mut changes = Vec::new();
    for line in lines {
        let range = if line < last {
            Range::new(Position::new(line, 0), Position::new(line + 1, 0))
        } else if line > 0 {
            // The last line has no terminator of its own, so take the previous
            // line's instead or the file grows a blank line.
            Range::new(
                Position::new(line - 1, buffer.line_len_utf16(line as usize - 1)),
                buffer.end_position(),
            )
        } else {
            Range::new(Position::ZERO, buffer.end_position())
        };
        changes.push(Change::delete(range));
    }
    apply_line_changes(ctx, changes, before);
}

/// Opens a new line above or below and puts the caret on it.
fn insert_line(ctx: &mut Context<'_>, below: bool) {
    let buffer = &ctx.document.buffer;
    let line = ctx.view.selections.primary().active.line;
    let indent: String = line_text(buffer, line)
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();

    let (position, text) = if below {
        (
            movement::line_end(buffer, Position::new(line, 0)),
            format!("\n{indent}"),
        )
    } else {
        (Position::new(line, 0), format!("{indent}\n"))
    };

    let before = ctx.view.selections.clone();
    let transaction = Transaction::single(Change::insert(position, text.clone()));
    let inverse = ctx.document.apply(&transaction);

    let caret = if below {
        Position::new(line + 1, indent.chars().count() as u32)
    } else {
        Position::new(line, indent.chars().count() as u32)
    };
    let after = SelectionSet::caret(ctx.document.buffer.clamp_position(caret));
    ctx.view.selections = after.clone();
    ctx.document
        .history
        .record(inverse, EditKind::Discrete, before, after, ctx.now_ms);
    ctx.document.dirty = true;
}

/// Swaps the selected lines with the line above or below.
fn move_lines(ctx: &mut Context<'_>, up: bool) {
    let before = ctx.view.selections.clone();
    let buffer = &ctx.document.buffer;
    let lines = selected_lines(&before, buffer);
    let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else {
        return;
    };

    let last_line = (buffer.line_count() - 1) as u32;
    if (up && first == 0) || (!up && last >= last_line) {
        return;
    }

    // Rewriting the whole span in one change keeps this a single undo step and
    // avoids any question of overlapping edits.
    let (block_start, block_end, other) = if up {
        (first - 1, last, first - 1)
    } else {
        (first, last + 1, last + 1)
    };

    let mut texts: Vec<String> = (block_start..=block_end)
        .map(|l| line_text(buffer, l))
        .collect();
    let moved = if up {
        texts.remove(0)
    } else {
        texts.pop().unwrap_or_default()
    };
    if up {
        texts.push(moved);
    } else {
        texts.insert(0, moved);
    }
    let _ = other;

    let range = Range::new(
        Position::new(block_start, 0),
        Position::new(block_end, buffer.line_len_utf16(block_end as usize)),
    );
    let transaction = Transaction::single(Change::replace(range, texts.join("\n")));
    let inverse = ctx.document.apply(&transaction);

    let shift: i64 = if up { -1 } else { 1 };
    let after = SelectionSet::from_vec(
        before
            .iter()
            .map(|s| {
                let move_end = |p: Position| {
                    ctx.document.buffer.clamp_position(Position::new(
                        (p.line as i64 + shift).max(0) as u32,
                        p.character,
                    ))
                };
                Selection::new(move_end(s.anchor), move_end(s.active))
            })
            .collect(),
        before.primary_index(),
    );
    ctx.view.selections = after.clone();
    ctx.document
        .history
        .record(inverse, EditKind::Discrete, before, after, ctx.now_ms);
    ctx.document.dirty = true;
}

/// Duplicates the selected lines above or below.
fn copy_lines(ctx: &mut Context<'_>, up: bool) {
    let before = ctx.view.selections.clone();
    let buffer = &ctx.document.buffer;
    let lines = selected_lines(&before, buffer);
    let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else {
        return;
    };

    let block: Vec<String> = (first..=last).map(|l| line_text(buffer, l)).collect();
    let text = format!("{}\n", block.join("\n"));
    let transaction = Transaction::single(Change::insert(Position::new(first, 0), text));
    let inverse = ctx.document.apply(&transaction);

    let shift = if up { 0 } else { (last - first + 1) as i64 };
    let after = SelectionSet::from_vec(
        before
            .iter()
            .map(|s| {
                let shifted = |p: Position| {
                    ctx.document
                        .buffer
                        .clamp_position(Position::new((p.line as i64 + shift) as u32, p.character))
                };
                Selection::new(shifted(s.anchor), shifted(s.active))
            })
            .collect(),
        before.primary_index(),
    );
    ctx.view.selections = after.clone();
    ctx.document
        .history
        .record(inverse, EditKind::Discrete, before, after, ctx.now_ms);
    ctx.document.dirty = true;
}

/// Wraps each selection in a block comment, or unwraps one already there.
///
/// `editor.action.blockComment`. One transaction for every selection, so a
/// multi-cursor wrap is one undo step.
///
/// # What counts as already commented
///
/// Two shapes, because both arise from pressing the key twice. Either the
/// selection *contains* the delimiters — which is what you get by selecting a
/// commented region — or it sits immediately *inside* them, which is what this
/// command leaves behind, so that pressing the key again undoes it. Recognising
/// only the first would make the command not its own inverse.
///
/// An empty selection inserts an empty comment and puts the caret inside it,
/// which is what VS Code does: the point of pressing it with no selection is to
/// write the comment next.
fn block_comment(ctx: &mut Context<'_>) {
    let Some((open, close)) = block_comment_tokens(ctx.document.language()) else {
        return;
    };
    let before = ctx.view.selections.clone();
    let buffer = &ctx.document.buffer;

    // Planned in character indices, since every edit after the first sits at a
    // position the earlier ones moved.
    let mut planned: Vec<Planned> = Vec::new();
    for selection in before.iter() {
        let range = buffer.clamp_range(selection.range());
        let start = buffer.position_to_char(range.start);
        let end = buffer.position_to_char(range.end);
        planned.push(plan_block_comment(buffer, start, end, open, close));
    }
    planned.sort_by_key(|p| (p.start, p.end));

    let changes: Vec<Change> = planned
        .iter()
        .map(|p| {
            Change::replace(
                Range::new(
                    buffer.char_to_position(p.start),
                    buffer.char_to_position(p.end),
                ),
                p.text.clone(),
            )
        })
        .collect();
    let Ok(transaction) = Transaction::new(changes) else {
        // Two cursors fighting over the same text. Dropping the whole thing is
        // safer than applying half of it.
        return;
    };
    let inverse = ctx.document.apply(&transaction);

    // The selection each edit leaves behind: the text inside the delimiters, so
    // that pressing the key again unwraps exactly what it just wrapped.
    let mut selections = Vec::with_capacity(planned.len());
    let mut delta: isize = 0;
    for plan in &planned {
        let moved = (plan.start as isize + delta) as usize;
        let anchor = ctx
            .document
            .buffer
            .char_to_position(moved + plan.inner.start);
        let active = ctx.document.buffer.char_to_position(moved + plan.inner.end);
        selections.push(Selection::new(anchor, active));
        delta += plan.text.chars().count() as isize - (plan.end - plan.start) as isize;
    }

    let after = SelectionSet::from_vec(selections, before.primary_index());
    ctx.view.selections = after.clone();
    ctx.document
        .history
        .record(inverse, EditKind::Discrete, before, after, ctx.now_ms);
    ctx.document.dirty = true;
}

/// One selection's share of a block-comment edit.
struct Planned {
    /// The character range being replaced.
    start: usize,
    end: usize,
    /// What replaces it.
    text: String,
    /// Where the interesting part of `text` is, in characters from its start —
    /// the text inside the delimiters after a wrap, or the whole of it after an
    /// unwrap.
    inner: std::ops::Range<usize>,
}

/// Decides whether one selection is being wrapped or unwrapped, and how.
fn plan_block_comment(
    buffer: &Buffer,
    start: usize,
    end: usize,
    open: &str,
    close: &str,
) -> Planned {
    let selected: String = buffer.text_in_range(Range::new(
        buffer.char_to_position(start),
        buffer.char_to_position(end),
    ));
    let trimmed = selected.trim();

    // The selection contains the delimiters: `/* foo */` was selected whole.
    if trimmed.len() >= open.len() + close.len()
        && trimmed.starts_with(open)
        && trimmed.ends_with(close)
    {
        let inner = trimmed[open.len()..trimmed.len() - close.len()].trim();
        return Planned {
            start,
            end,
            text: inner.to_owned(),
            inner: 0..inner.chars().count(),
        };
    }

    // The selection sits inside them, which is what a wrap leaves behind. The
    // delimiters are swallowed along with one space each, since that is what the
    // wrap inserted.
    if let Some((outer_start, outer_end)) = surrounding_comment(buffer, start, end, open, close) {
        return Planned {
            start: outer_start,
            end: outer_end,
            text: selected.clone(),
            inner: 0..selected.chars().count(),
        };
    }

    // Otherwise wrap. `/* foo */`, and `/*  */` for an empty selection — with the
    // caret between the spaces, which is where the comment gets typed.
    let text = format!("{open} {selected} {close}");
    let lead = open.chars().count() + 1;
    Planned {
        start,
        end,
        text,
        inner: lead..lead + selected.chars().count(),
    }
}

/// The range of a block comment that immediately surrounds `start..end`, if one
/// does.
///
/// Allows one space on each side, because that is what a wrap inserts.
fn surrounding_comment(
    buffer: &Buffer,
    start: usize,
    end: usize,
    open: &str,
    close: &str,
) -> Option<(usize, usize)> {
    let read = |from: usize, to: usize| {
        (from <= to && to <= buffer.len_chars()).then(|| {
            buffer.text_in_range(Range::new(
                buffer.char_to_position(from),
                buffer.char_to_position(to),
            ))
        })
    };

    for space in [1usize, 0] {
        let open_len = open.chars().count() + space;
        let close_len = close.chars().count() + space;
        let Some(before) = start.checked_sub(open_len).and_then(|s| read(s, start)) else {
            continue;
        };
        let Some(after) = read(end, end + close_len) else {
            continue;
        };
        let wanted_open = if space == 1 {
            format!("{open} ")
        } else {
            open.to_owned()
        };
        let wanted_close = if space == 1 {
            format!(" {close}")
        } else {
            close.to_owned()
        };
        if before == wanted_open && after == wanted_close {
            return Some((start - open_len, end + close_len));
        }
    }
    None
}

/// Which way [`line_comment`] should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentMode {
    /// `editor.action.commentLine` — comment unless everything already is.
    Toggle,
    /// `editor.action.addCommentLine` — always comment.
    Add,
    /// `editor.action.removeCommentLine` — always uncomment.
    Remove,
}

/// Adds or removes line comments on the selected lines.
///
/// `Toggle` comments unless *every* non-blank line already is, which is what
/// makes the command its own inverse. Blank lines are never touched, and a line
/// that is not actually commented is never uncommented — without that check,
/// removing comments from a mixed selection would eat real code.
fn line_comment(ctx: &mut Context<'_>, mode: CommentMode) {
    let Some(token) = line_comment_token(ctx.document.language()) else {
        return;
    };
    let before = ctx.view.selections.clone();
    let lines = selected_lines(&before, &ctx.document.buffer);
    let texts: Vec<(u32, String)> = lines
        .iter()
        .map(|l| (*l, line_text(&ctx.document.buffer, *l)))
        .collect();

    let interesting: Vec<&(u32, String)> = texts
        .iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .collect();
    if interesting.is_empty() {
        return;
    }

    let uncomment = match mode {
        CommentMode::Add => false,
        CommentMode::Remove => true,
        CommentMode::Toggle => interesting
            .iter()
            .all(|(_, text)| text.trim_start().starts_with(token)),
    };

    let mut changes = Vec::new();
    for (line, text) in &texts {
        if text.trim().is_empty() {
            continue;
        }
        let indent = text.chars().take_while(|c| c.is_whitespace()).count() as u32;
        if uncomment {
            if !text.trim_start().starts_with(token) {
                continue;
            }
            let after_token = indent + token.chars().count() as u32;
            // Swallow the single space comment styles conventionally add.
            let trailing_space = text
                .chars()
                .nth(after_token as usize)
                .map(|c| c == ' ')
                .unwrap_or(false);
            let end = after_token + u32::from(trailing_space);
            changes.push(Change::delete(Range::new(
                Position::new(*line, indent),
                Position::new(*line, end),
            )));
        } else {
            changes.push(Change::insert(
                Position::new(*line, indent),
                format!("{token} "),
            ));
        }
    }
    apply_line_changes(ctx, changes, before);
}

/// Copies the selections, or the whole line when nothing is selected.
fn copy_selection(ctx: &mut Context<'_>) {
    let buffer = &ctx.document.buffer;
    let parts: Vec<String> = if ctx.view.selections.iter().all(Selection::is_empty) {
        selected_lines(&ctx.view.selections, buffer)
            .iter()
            .map(|l| format!("{}\n", line_text(buffer, *l)))
            .collect()
    } else {
        ctx.view
            .selections
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| buffer.text_in_range(s.range()))
            .collect()
    };
    ctx.clipboard.write(&parts.join(""));
}

/// Cut's delete half: the selection, or the whole line if there is none.
fn delete_selection_or_line(ctx: &mut Context<'_>) {
    if ctx.view.selections.iter().all(Selection::is_empty) {
        delete_lines(ctx);
    } else {
        edit_at_selections(ctx, EditKind::Delete, |_, selection| {
            (!selection.is_empty()).then(|| (selection.range(), String::new()))
        });
    }
}

/// The text `ctrl+d` and `ctrl+shift+l` search for, and the selection it came from.
///
/// With a selection, that text. With a bare caret, the word under it — and the
/// caret is replaced by a selection of that word, which is `ctrl+d`'s first press
/// and the reason it feels like two different commands.
fn search_term(ctx: &mut Context<'_>) -> Option<(String, Selection)> {
    let primary = *ctx.view.selections.primary();

    if !primary.is_empty() {
        let text = ctx.document.buffer.text_in_range(primary.range());
        // A selection spanning a line break is a legitimate search term, but an
        // empty one is not — and `text_in_range` on a collapsed range gives "".
        return (!text.is_empty()).then_some((text, primary));
    }

    let word = deco_core::search::word_at(&ctx.document.buffer, primary.active)?;
    let text = ctx.document.buffer.text_in_range(word);
    let selection = Selection::new(word.start, word.end);

    // Every caret expands to its own word, which is what VS Code does. Cursors are
    // placed deliberately and expanding each of them keeps that placement —
    // discarding them would not, but discarding them was never the alternative.
    // A caret with no word under it stays a caret rather than selecting the
    // whitespace it sits in, and a selection that is already a selection is left
    // as the user made it.
    let index = ctx.view.selections.primary_index();
    let selections: Vec<Selection> = ctx
        .view
        .selections
        .iter()
        .map(|existing| {
            if !existing.is_empty() {
                return *existing;
            }
            match deco_core::search::word_at(&ctx.document.buffer, existing.active) {
                Some(word) => Selection::new(word.start, word.end),
                None => *existing,
            }
        })
        .collect();
    ctx.view.selections = SelectionSet::from_vec(selections, index);
    Some((text, selection))
}

/// `ctrl+d`: selects the word under the caret, then adds each next occurrence.
///
/// Two behaviours behind one key, which is what VS Code does and what makes it
/// worth using: the first press turns a caret into a selection, and every press
/// after that adds a cursor at the following match.
fn add_next_match(ctx: &mut Context<'_>) -> Outcome {
    // Read before `search_term`, which turns a bare caret into a selection of the
    // word under it and so destroys the evidence of which press this was.
    let was_caret = ctx.view.selections.primary().is_empty();
    let Some((needle, primary)) = search_term(ctx) else {
        return Outcome::Handled;
    };
    // The first press only selects the word. Adding a cursor in the same breath
    // would skip an occurrence, and the user has not yet seen what they selected.
    if was_caret {
        return Outcome::Handled;
    }

    let matches = deco_core::search::find_all(
        &ctx.document.buffer,
        &needle,
        deco_core::search::SearchOptions::EXACT,
    );
    let taken: Vec<Range> = ctx.view.selections.iter().map(Selection::range).collect();

    // Every occurrence from the one after the primary selection onwards, then
    // round to the top of the file — skipping anything a cursor already sits on,
    // so holding ctrl+d walks the file rather than stalling on the occurrence
    // after the last one added.
    let start_at = matches
        .iter()
        .position(|range| range.start >= primary.end())
        .unwrap_or(0);
    let after = matches[start_at..]
        .iter()
        .chain(matches[..start_at].iter())
        .find(|range| !taken.contains(range));

    let Some(&next) = after else {
        // Every occurrence already has a cursor. Saying so beats a key that
        // silently does nothing.
        return Outcome::Message(format!(
            "all {} occurrences of {needle:?} are selected",
            matches.len()
        ));
    };

    ctx.view
        .selections
        .add(Selection::new(next.start, next.end));
    ctx.view
        .reveal_cursor(&ctx.document.buffer, &ctx.document.settings);
    Outcome::Handled
}

/// `ctrl+shift+l`: puts a cursor on every occurrence.
fn select_all_matches(ctx: &mut Context<'_>) -> Outcome {
    let Some((needle, _)) = search_term(ctx) else {
        return Outcome::Handled;
    };
    let matches = deco_core::search::find_all(
        &ctx.document.buffer,
        &needle,
        deco_core::search::SearchOptions::EXACT,
    );
    if matches.is_empty() {
        return Outcome::Handled;
    }

    let selections: Vec<Selection> = matches
        .iter()
        .map(|range| Selection::new(range.start, range.end))
        .collect();
    // The last one is primary, so the view scrolls to the end of the file and the
    // user can see how far the change reaches.
    let primary = selections.len() - 1;
    ctx.view.selections = SelectionSet::from_vec(selections, primary);
    ctx.view
        .reveal_cursor(&ctx.document.buffer, &ctx.document.settings);
    Outcome::Message(format!("{} occurrences selected", matches.len()))
}

/// `ctrl+k ctrl+d`: moves the last cursor to the next occurrence instead of adding one.
///
/// The escape hatch for `ctrl+d` pressed once too often: it skips the occurrence
/// you did not want rather than making you start over.
fn move_to_next_match(ctx: &mut Context<'_>) -> Outcome {
    let was_caret = ctx.view.selections.primary().is_empty();
    let Some((needle, primary)) = search_term(ctx) else {
        return Outcome::Handled;
    };
    // Like `ctrl+d`, the first press only selects the word — there is nothing to
    // move yet.
    if was_caret {
        return Outcome::Handled;
    }
    let Some(next) = deco_core::search::find_next(
        &ctx.document.buffer,
        &needle,
        primary.end(),
        deco_core::search::SearchOptions::EXACT,
    ) else {
        return Outcome::Handled;
    };

    let mut selections: Vec<Selection> = ctx.view.selections.as_slice().to_vec();
    let index = ctx.view.selections.primary_index();
    selections[index] = Selection::new(next.start, next.end);
    ctx.view.selections = SelectionSet::from_vec(selections, index);
    ctx.view
        .reveal_cursor(&ctx.document.buffer, &ctx.document.settings);
    Outcome::Handled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use deco_config::EditorSettings;
    use serde_json::json;
    use std::path::PathBuf;

    struct Harness {
        document: Document,
        view: View,
        clipboard: MemoryClipboard,
        clock: u64,
    }

    impl Harness {
        fn new(text: &str) -> Self {
            Self::with_language(text, "txt")
        }

        fn with_language(text: &str, extension: &str) -> Self {
            let document = Document::from_file(
                PathBuf::from(format!("/w/file.{extension}")),
                text,
                EditorSettings {
                    // Off, so a test's indentation is the one it set rather than one
                    // read out of its own fixture. `editor.detectIndentation` has its
                    // own tests in `deco_config::indent`, and its effect on `tab` and
                    // `outdent` is asserted through a whole session.
                    detect_indentation: false,
                    ..EditorSettings::default()
                },
            );
            Self {
                document,
                view: View {
                    height: 10,
                    ..Default::default()
                },
                clipboard: MemoryClipboard::default(),
                clock: 0,
            }
        }

        fn at(mut self, line: u32, character: u32) -> Self {
            self.view.selections = SelectionSet::caret(Position::new(line, character));
            self
        }

        fn selecting(mut self, from: (u32, u32), to: (u32, u32)) -> Self {
            self.view.selections = SelectionSet::single(Selection::new(
                Position::new(from.0, from.1),
                Position::new(to.0, to.1),
            ));
            self
        }

        fn run(&mut self, command: &str) -> Outcome {
            self.run_with(command, None)
        }

        fn run_with(&mut self, command: &str, args: Option<Value>) -> Outcome {
            // Each command lands in its own undo group unless a test says
            // otherwise, which keeps assertions about undo unambiguous.
            self.clock += 10_000;
            let mut ctx = Context {
                document: &mut self.document,
                view: &mut self.view,
                clipboard: &mut self.clipboard,
                now_ms: self.clock,
            };
            execute(&mut ctx, command, args.as_ref())
        }

        fn type_text(&mut self, text: &str) -> Outcome {
            self.run_with("type", Some(json!({ "text": text })))
        }

        fn text(&self) -> String {
            self.document.buffer.text()
        }

        fn cursor(&self) -> Position {
            self.view.selections.primary().active
        }
    }

    // ---- Auto-indent on enter ---------------------------------------------

    /// Presses enter, which is bound to `type` with a newline in it.
    fn enter(h: &mut Harness) {
        h.type_text("\n");
    }

    #[test]
    fn a_new_line_starts_where_the_old_one_started() {
        // The gap this closes: `enter` went to column zero while `ctrl+enter` copied
        // the indent, so the same editor indented on one key and not the other.
        let mut h = Harness::with_language("fn main() {\n    let x = 1;\n}\n", "rs").at(1, 14);
        enter(&mut h);
        assert_eq!(h.text(), "fn main() {\n    let x = 1;\n    \n}\n");
        assert_eq!(h.cursor(), Position::new(2, 4));
    }

    #[test]
    fn none_starts_at_column_zero() {
        let mut h = Harness::with_language("fn main() {\n    let x = 1;\n}\n", "rs").at(1, 14);
        h.document.settings.auto_indent = deco_config::AutoIndent::None;
        enter(&mut h);
        assert_eq!(h.text(), "fn main() {\n    let x = 1;\n\n}\n");
    }

    #[test]
    fn enter_inside_the_indentation_carries_only_what_the_caret_had() {
        // Otherwise pressing enter two spaces into a four-space indent would hand the
        // new line more indentation than the caret was sitting at.
        let mut h = Harness::new("        deep\n").at(0, 2);
        enter(&mut h);
        // Two spaces stay above; the new line carries those two, and the six the
        // caret had not reached stay in front of `deep`.
        assert_eq!(h.text(), "  \n        deep\n");
        assert_eq!(h.cursor(), Position::new(1, 2));
    }

    #[test]
    fn an_opening_bracket_adds_a_level() {
        let mut h = Harness::with_language("fn main() {\n", "rs").at(0, 11);
        enter(&mut h);
        assert_eq!(h.text(), "fn main() {\n    \n");
        assert_eq!(h.cursor(), Position::new(1, 4));
    }

    #[test]
    fn enter_between_a_pair_opens_a_block() {
        // The shape everybody types next, and the pair to `auto_close`: typing `{`
        // produces `{}`, and `enter` opens it into a block.
        let mut h = Harness::with_language("fn main() {}\n", "rs").at(0, 11);
        enter(&mut h);
        assert_eq!(h.text(), "fn main() {\n    \n}\n");
        assert_eq!(h.cursor(), Position::new(1, 4), "on the line between");
    }

    #[test]
    fn opening_a_block_keeps_the_outer_indent_on_the_closer() {
        let mut h = Harness::with_language("    if a {}\n", "rs").at(0, 10);
        enter(&mut h);
        assert_eq!(h.text(), "    if a {\n        \n    }\n");
        assert_eq!(h.cursor(), Position::new(1, 8));
    }

    #[test]
    fn typing_a_bracket_and_pressing_enter_composes() {
        // The two features in sequence, which is how they are actually used.
        let mut h = Harness::with_language("fn main() \n", "rs").at(0, 10);
        h.type_text("{");
        assert_eq!(h.text(), "fn main() {}\n", "closed by `auto_close`");
        enter(&mut h);
        assert_eq!(h.text(), "fn main() {\n    \n}\n");
    }

    #[test]
    fn keep_copies_the_indent_but_does_not_open_a_block() {
        let mut h = Harness::with_language("    if a {}\n", "rs").at(0, 10);
        h.document.settings.auto_indent = deco_config::AutoIndent::Keep;
        enter(&mut h);
        assert_eq!(
            h.text(),
            "    if a {\n    }\n",
            "the indent, and no extra line"
        );
    }

    #[test]
    fn a_bracket_that_is_not_this_languages_pair_opens_nothing() {
        // Rust has no apostrophe pair, so `'|'` is two lifetimes and not a block.
        let mut h = Harness::with_language("let a = ''\n", "rs").at(0, 9);
        enter(&mut h);
        assert_eq!(
            h.text(),
            "let a = '\n'\n",
            "a plain newline, no indent added"
        );
    }

    #[test]
    fn a_selection_is_replaced_rather_than_indented() {
        // The indentation of a line about to be partly deleted is not the indentation
        // of what is left of it.
        let mut h = Harness::new("    one    two\n").selecting((0, 4), (0, 11));
        enter(&mut h);
        assert_eq!(h.text(), "    \ntwo\n");
    }

    #[test]
    fn every_caret_gets_its_own_indent() {
        let mut h = Harness::new("  a\n      b\n");
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::new(Position::new(0, 3), Position::new(0, 3)),
                Selection::new(Position::new(1, 7), Position::new(1, 7)),
            ],
            0,
        );
        enter(&mut h);
        assert_eq!(h.text(), "  a\n  \n      b\n      \n");
    }

    #[test]
    fn a_line_with_no_indent_is_left_to_the_plain_path() {
        let mut h = Harness::new("abc\n").at(0, 3);
        enter(&mut h);
        assert_eq!(h.text(), "abc\n\n");
        assert_eq!(h.cursor(), Position::new(1, 0));
    }

    #[test]
    fn an_indented_newline_is_one_undo_step() {
        let mut h = Harness::with_language("fn main() {}\n", "rs").at(0, 11);
        enter(&mut h);
        assert_eq!(h.text(), "fn main() {\n    \n}\n");
        h.run("undo");
        assert_eq!(h.text(), "fn main() {}\n", "all three lines back to one");
    }

    // ---- Trimming an auto-indent ------------------------------------------

    #[test]
    fn one_enter_too_many_leaves_no_trailing_whitespace() {
        // The mess `editor.autoIndent` makes and this cleans up: without it the line
        // you pressed past keeps four spaces, and a diff shows it.
        let mut h = Harness::with_language("fn main() {\n    let x = 1;\n}\n", "rs").at(1, 14);
        enter(&mut h);
        assert_eq!(h.text(), "fn main() {\n    let x = 1;\n    \n}\n");
        enter(&mut h);
        assert_eq!(
            h.text(),
            "fn main() {\n    let x = 1;\n\n    \n}\n",
            "the abandoned line is empty, not four spaces"
        );
    }

    #[test]
    fn the_trim_is_part_of_the_same_undo_step() {
        // The whitespace and the keystroke that abandoned it are one action, so one
        // `ctrl+z` should take back one thing.
        let mut h = Harness::with_language("    a\n", "rs").at(0, 5);
        enter(&mut h);
        enter(&mut h);
        assert_eq!(h.text(), "    a\n\n    \n");
        h.run("undo");
        assert_eq!(h.text(), "    a\n    \n", "one press, one step");
    }

    #[test]
    fn whitespace_that_was_typed_is_not_touched() {
        // Only an indent deco inserted is trimmable. This one was typed.
        let mut h = Harness::new("a\n").at(0, 1);
        enter(&mut h);
        h.type_text("    ");
        assert_eq!(h.text(), "a\n    \n");
        enter(&mut h);
        assert_eq!(h.text(), "a\n    \n    \n", "both lines keep their spaces");
    }

    #[test]
    fn typing_on_the_line_makes_the_indent_its_own() {
        let mut h = Harness::with_language("    a\n", "rs").at(0, 5);
        enter(&mut h);
        h.type_text("b");
        enter(&mut h);
        assert_eq!(h.text(), "    a\n    b\n    \n", "`    b` keeps its indent");
    }

    #[test]
    fn an_edit_elsewhere_trims_the_line_that_was_left() {
        // Not only the next `enter`: any edit is the moment the abandoned line stops
        // being where the caret is.
        let mut h = Harness::with_language("    a\nb\n", "rs").at(0, 5);
        enter(&mut h);
        assert_eq!(h.text(), "    a\n    \nb\n");
        h.view.selections = SelectionSet::caret(Position::new(2, 1));
        h.type_text("!");
        assert_eq!(h.text(), "    a\n\nb!\n");
    }

    #[test]
    fn the_setting_turns_it_off() {
        let mut h = Harness::with_language("    a\n", "rs").at(0, 5);
        h.document.settings.trim_auto_whitespace = false;
        enter(&mut h);
        enter(&mut h);
        assert_eq!(h.text(), "    a\n    \n    \n");
    }

    #[test]
    fn a_line_that_gained_text_some_other_way_is_left_alone() {
        // The record is a record, not an authority: the line is checked against the
        // buffer, so a stale entry does nothing rather than deleting somebody's text.
        let mut h = Harness::with_language("    a\n", "rs").at(0, 5);
        enter(&mut h);
        // Put text on the tracked line without going through an edit that clears the
        // record, which is what a stale entry looks like.
        let end = h.document.buffer.end_position();
        let transaction =
            Transaction::single(Change::insert(Position::new(1, 4), "kept".to_owned()));
        h.document.apply(&transaction);
        assert_eq!(h.document.buffer.text(), "    a\n    kept\n");
        assert_eq!(end.line, 2, "the fixture is the shape this test assumes");

        h.view.selections = SelectionSet::caret(Position::new(0, 5));
        h.type_text("!");
        assert_eq!(
            h.document.buffer.text(),
            "    a!\n    kept\n",
            "nothing was trimmed"
        );
    }

    #[test]
    fn every_caret_that_left_an_indent_has_it_trimmed() {
        let mut h = Harness::new("  a\n  b\n");
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::new(Position::new(0, 3), Position::new(0, 3)),
                Selection::new(Position::new(1, 3), Position::new(1, 3)),
            ],
            0,
        );
        enter(&mut h);
        assert_eq!(h.text(), "  a\n  \n  b\n  \n");
        enter(&mut h);
        assert_eq!(h.text(), "  a\n\n  \n  b\n\n  \n", "both trimmed");
    }

    // ---- Auto-closing brackets --------------------------------------------

    #[test]
    fn typing_an_opening_bracket_closes_it_and_stays_inside() {
        let mut h = Harness::with_language("fn main() {\n\n}\n", "rs").at(1, 0);
        h.type_text("foo");
        h.type_text("(");
        assert_eq!(h.text(), "fn main() {\nfoo()\n}\n");
        assert_eq!(h.cursor(), Position::new(1, 4), "between the two");
    }

    #[test]
    fn typing_the_closer_steps_over_it_rather_than_doubling_it() {
        let mut h = Harness::with_language("fn main() {\n\n}\n", "rs").at(1, 0);
        for text in ["foo", "(", "1", ")"] {
            h.type_text(text);
        }
        assert_eq!(h.text(), "fn main() {\nfoo(1)\n}\n", "one closer, not two");
        assert_eq!(h.cursor(), Position::new(1, 6), "and past it");
    }

    #[test]
    fn a_quote_both_opens_and_closes() {
        // Which is why stepping over is tried before opening: in front of a `"` the
        // useful answer is to move past it, not to open another pair.
        let mut h = Harness::with_language("let x = ;\n", "rs").at(0, 8);
        h.type_text("\"");
        assert_eq!(h.text(), "let x = \"\";\n");
        h.type_text("hi");
        h.type_text("\"");
        assert_eq!(h.text(), "let x = \"hi\";\n");
        assert_eq!(h.cursor(), Position::new(0, 12), "past the closing quote");
    }

    #[test]
    fn nothing_closes_in_the_middle_of_a_word() {
        // `languageDefined`, the default. Closing here would turn `word` into
        // `wo(r)rd`, which is the reason the default is conditional.
        let mut h = Harness::new("word\n").at(0, 2);
        h.type_text("(");
        assert_eq!(h.text(), "wo(rd\n");
    }

    #[test]
    fn always_closes_in_the_middle_of_a_word() {
        let mut h = Harness::new("word\n").at(0, 2);
        h.document.settings.auto_closing_brackets = deco_config::AutoClosingBrackets::Always;
        h.type_text("(");
        assert_eq!(h.text(), "wo()rd\n");
    }

    #[test]
    fn before_whitespace_refuses_what_language_defined_allows() {
        // `languageDefined` closes in front of `)`; `beforeWhitespace` does not.
        let mut h = Harness::new("()\n").at(0, 1);
        h.document.settings.auto_closing_brackets =
            deco_config::AutoClosingBrackets::BeforeWhitespace;
        h.type_text("[");
        assert_eq!(h.text(), "([)\n");
    }

    #[test]
    fn never_leaves_typing_exactly_as_it_was() {
        let mut h = Harness::new("\n").at(0, 0);
        h.document.settings.auto_closing_brackets = deco_config::AutoClosingBrackets::Never;
        h.type_text("(");
        h.type_text(")");
        assert_eq!(h.text(), "()\n", "both typed, neither inserted");
        assert_eq!(h.cursor(), Position::new(0, 2));
    }

    #[test]
    fn a_rust_apostrophe_is_a_lifetime_and_does_not_close() {
        // `&'a str` is ordinary Rust, and `&''a str` is what closing it would write.
        let mut h = Harness::with_language("let a = b;\n", "rs").at(0, 9);
        h.type_text("'");
        assert_eq!(h.text(), "let a = b';\n");
    }

    #[test]
    fn an_apostrophe_does_close_in_a_language_that_quotes_with_it() {
        let mut h = Harness::with_language("x = ;\n", "py").at(0, 4);
        h.type_text("'");
        assert_eq!(h.text(), "x = '';\n");
    }

    #[test]
    fn a_backtick_closes_in_typescript_and_not_elsewhere() {
        let mut h = Harness::with_language("const x = ;\n", "ts").at(0, 10);
        h.type_text("`");
        assert_eq!(h.text(), "const x = ``;\n");

        let mut other = Harness::with_language("let x = ;\n", "rs").at(0, 8);
        other.type_text("`");
        assert_eq!(other.text(), "let x = `;\n");
    }

    #[test]
    fn a_selection_is_replaced_rather_than_surrounded() {
        // Wrapping instead is `editor.autoSurround`, which deco does not read.
        // Closing a bracket *around* a replacement while leaving the replacement out
        // would be neither behaviour.
        let mut h = Harness::new("hello world\n").selecting((0, 0), (0, 5));
        h.type_text("(");
        assert_eq!(h.text(), "( world\n");
    }

    #[test]
    fn a_pasted_pair_is_not_reopened() {
        // More than one character is not somebody reaching for a bracket.
        let mut h = Harness::new("\n").at(0, 0);
        h.type_text("foo()");
        assert_eq!(h.text(), "foo()\n");
    }

    #[test]
    fn every_caret_closes_or_none_of_them_does() {
        // One keystroke inserting a pair in some places and a bare bracket in others
        // is the sort of multi-cursor edit nobody can undo by looking at it.
        let mut h = Harness::new("aa\nbb\n");
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::new(Position::new(0, 2), Position::new(0, 2)),
                // Mid-word on the second line, where `languageDefined` refuses.
                Selection::new(Position::new(1, 1), Position::new(1, 1)),
            ],
            0,
        );
        h.type_text("(");
        assert_eq!(h.text(), "aa(\nb(b\n", "neither closed");
    }

    #[test]
    fn a_closed_pair_is_one_undo_step() {
        let mut h = Harness::new("\n").at(0, 0);
        h.type_text("(");
        assert_eq!(h.text(), "()\n");
        h.run("undo");
        assert_eq!(h.text(), "\n", "both halves, since one keystroke made them");
    }

    // ---- Motion through a wrapped line ------------------------------------

    /// A harness whose window wraps at ten columns.
    fn wrapped(text: &str) -> Harness {
        let mut h = Harness::new(text);
        h.document.settings.word_wrap = deco_config::WordWrap::On;
        h.view.text_width = 10;
        h
    }

    /// Six-column words, so a ten-column window puts one per row: the rows of
    /// `WORDS` start at 0, 6, 12 and 18.
    const WORDS: &str = "aaaaa bbbbb ccccc ddddd\nnext\n";

    #[test]
    fn the_rows_of_the_sample_are_where_the_tests_below_assume() {
        // Stated once, so a failure in the motion tests is about motion rather
        // than about where this particular sentence happens to break.
        let h = wrapped(WORDS);
        assert_eq!(
            h.view
                .row_starts(&h.document.buffer, &h.document.settings, 0),
            [0, 6, 12, 18]
        );
    }

    #[test]
    fn down_moves_one_row_and_not_one_line() {
        // Line 0 is four rows. Moving by line would skip all of it, which in prose
        // is most of a paragraph passing under one keypress.
        let mut h = wrapped(WORDS).at(0, 0);
        for expected in [6, 12, 18] {
            h.run("cursorDown");
            assert_eq!(h.cursor(), Position::new(0, expected), "still line 0");
        }
        h.run("cursorDown");
        assert_eq!(h.cursor(), Position::new(1, 0), "and on to the next line");
    }

    #[test]
    fn down_still_moves_one_line_when_nothing_is_wrapped() {
        // The same key, the same file, wrapping off: the rows and the lines are
        // the same thing, and the old path is what runs.
        let mut h = Harness::new(WORDS).at(0, 0);
        h.run("cursorDown");
        assert_eq!(h.cursor(), Position::new(1, 0));
    }

    #[test]
    fn up_and_down_keep_the_column_within_the_row() {
        // Two columns into row 1 is column 8 of the line, and coming back up has to
        // be two columns into row 0 — column 2 — rather than column 8, which is
        // where a goal measured from the line's start would land.
        let mut h = wrapped(WORDS).at(0, 8);
        h.run("cursorUp");
        assert_eq!(h.cursor(), Position::new(0, 2));
        h.run("cursorDown");
        assert_eq!(h.cursor(), Position::new(0, 8));
    }

    #[test]
    fn a_goal_past_the_end_of_a_row_stops_on_that_row() {
        // Row 0 is ten columns and row 1 is two, so the goal overshoots. Landing on
        // the next row's first column would make one press of `down` move two rows.
        let mut h = wrapped("aaaaaaaaaa bb\ncc\n").at(0, 9);
        h.run("cursorDown");
        assert_eq!(h.cursor(), Position::new(0, 13), "the end of row 1");
    }

    #[test]
    fn page_down_moves_a_window_of_rows() {
        // Ten columns to a row and a window nine rows deep.
        let mut h = wrapped(&format!("{}\nlast\n", "word ".repeat(20))).at(0, 0);
        h.run("cursorPageDown");
        assert_eq!(h.cursor(), Position::new(0, 90));
    }

    #[test]
    fn home_and_end_are_the_rows_ends_on_a_continuation_row() {
        let mut h = wrapped(WORDS).at(0, 8);
        h.run("cursorHome");
        assert_eq!(h.cursor(), Position::new(0, 6), "the start of the row");
        h.run("cursorEnd");
        assert_eq!(
            h.cursor(),
            Position::new(0, 11),
            "the last character the row shows"
        );
    }

    #[test]
    fn home_on_a_lines_first_row_still_stops_at_the_indent() {
        // The row's start and the line's start are the same cell there, so the key
        // keeps the trick it has when nothing is wrapped.
        let mut h = wrapped("  aaaa bbbb cccc\n").at(0, 4);
        h.run("cursorHome");
        assert_eq!(h.cursor(), Position::new(0, 2), "the first non-whitespace");
        h.run("cursorHome");
        assert_eq!(h.cursor(), Position::new(0, 0), "and then column zero");
    }

    #[test]
    fn end_on_a_lines_last_row_is_the_end_of_the_line() {
        let mut h = wrapped(WORDS).at(0, 20);
        h.run("cursorEnd");
        assert_eq!(
            h.cursor(),
            Position::new(0, 23),
            "one past the last character"
        );
    }

    #[test]
    fn end_then_down_measures_the_column_from_where_it_landed() {
        // `home` and `end` clear the sticky goal. Keeping it would move `down` back
        // to whatever column was last aimed at rather than down from the end.
        let mut h = wrapped(WORDS).at(0, 0);
        h.run("cursorEnd");
        assert_eq!(h.cursor(), Position::new(0, 5));
        h.run("cursorDown");
        assert_eq!(h.cursor(), Position::new(0, 11));
    }

    #[test]
    fn the_goal_column_is_a_column_of_the_screen_and_not_of_the_row() {
        // `editor.wrappingIndent` pushes a continuation row in, so the same offset
        // into two rows is two different places on screen. The caret keeps the
        // screen column: what looks like straight down has to be straight down.
        let mut h = Harness::new("  aaaaa bbbbb ccccc\n");
        h.document.settings.word_wrap = deco_config::WordWrap::On;
        h.document.settings.wrapping_indent = deco_config::WrappingIndent::Same;
        h.document.settings.tab_size = 2;
        h.view.text_width = 10;

        let starts = h
            .view
            .row_starts(&h.document.buffer, &h.document.settings, 0);
        assert!(starts.len() > 1, "the line wraps: {starts:?}");

        // Two columns into row 0's text is screen column 2; row 1 is pushed in by
        // two, so the same screen column is its *first* character.
        h.view.selections = SelectionSet::caret(Position::new(0, 2));
        assert_eq!(
            h.view.goal_column(
                &h.document.buffer,
                &h.document.settings,
                Position::new(0, 2)
            ),
            2
        );
        h.run("cursorDown");
        assert_eq!(
            h.view
                .goal_column(&h.document.buffer, &h.document.settings, h.cursor()),
            2,
            "the same column of the screen"
        );
        assert_eq!(h.cursor().character, starts[1], "row 1's first character");
    }

    #[test]
    fn a_goal_inside_the_indent_lands_on_the_rows_first_character() {
        // There is no column further left on that row to land on.
        let mut h = Harness::new("      aaaaa bbbbb ccccc\n");
        h.document.settings.word_wrap = deco_config::WordWrap::On;
        h.document.settings.wrapping_indent = deco_config::WrappingIndent::Same;
        h.view.text_width = 14;

        let starts = h
            .view
            .row_starts(&h.document.buffer, &h.document.settings, 0);
        assert!(starts.len() > 1, "{starts:?}");
        h.view.selections = SelectionSet::caret(Position::new(0, 1));
        h.run("cursorDown");
        assert_eq!(h.cursor().character, starts[1]);
    }

    #[test]
    fn every_cursor_moves_by_rows() {
        // Two carets on one wrapped line, on different rows of it.
        let mut h = wrapped(WORDS);
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::new(Position::new(0, 1), Position::new(0, 1)),
                Selection::new(Position::new(0, 13), Position::new(0, 13)),
            ],
            0,
        );
        h.run("cursorDown");
        let after: Vec<Position> = h.view.selections.iter().map(|s| s.active).collect();
        assert_eq!(after, [Position::new(0, 7), Position::new(0, 19)]);
    }

    // ---- Block comment ----------------------------------------------------

    #[test]
    fn a_selection_is_wrapped_in_the_languages_block_delimiters() {
        let mut h = Harness::with_language("let x = 1;\n", "rs").selecting((0, 8), (0, 9));
        h.run("editor.action.blockComment");
        assert_eq!(h.text(), "let x = /* 1 */;\n");
    }

    #[test]
    fn pressing_it_twice_leaves_the_text_as_it_was() {
        // Its own inverse, which needs the selection it leaves behind to be the
        // text inside the delimiters.
        let mut h = Harness::with_language("let x = 1;\n", "rs").selecting((0, 8), (0, 9));
        h.run("editor.action.blockComment");
        h.run("editor.action.blockComment");
        assert_eq!(h.text(), "let x = 1;\n");
    }

    #[test]
    fn selecting_a_whole_comment_removes_it() {
        // The other shape a commented selection comes in: selected from outside
        // rather than left behind by a wrap.
        let mut h = Harness::with_language("let x = /* 1 */;\n", "rs").selecting((0, 8), (0, 15));
        h.run("editor.action.blockComment");
        assert_eq!(h.text(), "let x = 1;\n");
    }

    #[test]
    fn an_empty_selection_opens_a_comment_with_the_caret_inside_it() {
        let mut h = Harness::with_language("let x = 1;\n", "rs").at(0, 10);
        h.run("editor.action.blockComment");
        assert_eq!(h.text(), "let x = 1;/*  */\n");
        // Between the spaces, which is where the comment gets typed.
        assert_eq!(h.cursor(), Position::new(0, 13));
    }

    #[test]
    fn a_multi_line_selection_is_wrapped_once_not_per_line() {
        // The difference from a line comment, and the reason to have both.
        let mut h = Harness::with_language("a();\nb();\n", "rs").selecting((0, 0), (1, 4));
        h.run("editor.action.blockComment");
        assert_eq!(h.text(), "/* a();\nb(); */\n");
    }

    #[test]
    fn every_cursor_is_wrapped_in_one_undo_step() {
        let mut h = Harness::with_language("a();\nb();\n", "rs");
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::new(Position::new(0, 0), Position::new(0, 1)),
                Selection::new(Position::new(1, 0), Position::new(1, 1)),
            ],
            0,
        );
        h.run("editor.action.blockComment");
        assert_eq!(h.text(), "/* a */();\n/* b */();\n");
        h.run("undo");
        assert_eq!(h.text(), "a();\nb();\n", "one step, not two");
    }

    #[test]
    fn each_language_gets_its_own_delimiters() {
        for (extension, expected) in [
            ("rs", "/* x */\n"),
            ("css", "/* x */\n"),
            ("sql", "/* x */\n"),
            ("html", "<!-- x -->\n"),
            ("xml", "<!-- x -->\n"),
            ("md", "<!-- x -->\n"),
            ("lua", "--[[ x ]]\n"),
            ("py", "\"\"\" x \"\"\"\n"),
        ] {
            let mut h = Harness::with_language("x\n", extension).selecting((0, 0), (0, 1));
            h.run("editor.action.blockComment");
            assert_eq!(h.text(), expected, "{extension}");
        }
    }

    #[test]
    fn a_language_with_no_block_comment_is_left_alone() {
        // Shell, YAML, TOML and friends have none, and neither does VS Code claim
        // one for them. Ruby's `=begin` must sit alone at the start of a line, so
        // wrapping a selection with it would produce text Ruby cannot parse.
        for extension in ["sh", "yaml", "toml", "rb", "json", "txt"] {
            let mut h = Harness::with_language("x\n", extension).selecting((0, 0), (0, 1));
            h.run("editor.action.blockComment");
            assert_eq!(h.text(), "x\n", "{extension}");
        }
    }

    #[test]
    fn a_comment_wrapped_without_spaces_is_still_recognised() {
        // Not what this command inserts, but what a person writes by hand.
        let mut h = Harness::with_language("/*x*/\n", "rs").selecting((0, 0), (0, 5));
        h.run("editor.action.blockComment");
        assert_eq!(h.text(), "x\n");
    }

    #[test]
    fn unwrapping_leaves_the_text_selected_so_it_can_be_wrapped_again() {
        let mut h = Harness::with_language("let x = 1;\n", "rs").selecting((0, 8), (0, 9));
        h.run("editor.action.blockComment");
        h.run("editor.action.blockComment");
        h.run("editor.action.blockComment");
        assert_eq!(h.text(), "let x = /* 1 */;\n", "and back again");
    }

    #[test]
    fn an_unknown_command_is_reported() {
        let mut h = Harness::new("x");
        assert_eq!(h.run("no.such.command"), Outcome::NotFound);
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut h = Harness::new("hello world").at(0, 5);
        h.type_text(",");
        assert_eq!(h.text(), "hello, world");
        assert_eq!(h.cursor(), Position::new(0, 6));
        assert!(h.document.dirty);
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut h = Harness::new("hello world").selecting((0, 0), (0, 5));
        h.type_text("bye");
        assert_eq!(h.text(), "bye world");
        assert_eq!(h.cursor(), Position::new(0, 3));
    }

    #[test]
    fn typing_at_several_cursors_inserts_at_each() {
        let mut h = Harness::new("aa\nbb\ncc");
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::caret(Position::new(0, 1)),
                Selection::caret(Position::new(1, 1)),
                Selection::caret(Position::new(2, 1)),
            ],
            0,
        );
        h.type_text("X");
        assert_eq!(h.text(), "aXa\nbXb\ncXc");
        // Every caret ends up after its own insertion.
        let carets: Vec<Position> = h.view.selections.iter().map(|s| s.active).collect();
        assert_eq!(
            carets,
            [
                Position::new(0, 2),
                Position::new(1, 2),
                Position::new(2, 2)
            ]
        );
    }

    #[test]
    fn undo_reverts_a_multi_cursor_edit_in_one_step() {
        let mut h = Harness::new("aa\nbb");
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::caret(Position::new(0, 1)),
                Selection::caret(Position::new(1, 1)),
            ],
            0,
        );
        h.type_text("X");
        assert_eq!(h.text(), "aXa\nbXb");
        h.run("undo");
        assert_eq!(h.text(), "aa\nbb");
    }

    #[test]
    fn undo_and_redo_round_trip() {
        let mut h = Harness::new("abc").at(0, 3);
        h.type_text("d");
        h.run("undo");
        assert_eq!(h.text(), "abc");
        h.run("redo");
        assert_eq!(h.text(), "abcd");
    }

    #[test]
    fn backspace_removes_the_character_to_the_left() {
        let mut h = Harness::new("abc").at(0, 2);
        h.run("deleteLeft");
        assert_eq!(h.text(), "ac");
        assert_eq!(h.cursor(), Position::new(0, 1));
    }

    #[test]
    fn backspace_at_the_start_of_a_line_joins_it_to_the_previous_one() {
        let mut h = Harness::new("ab\ncd").at(1, 0);
        h.run("deleteLeft");
        assert_eq!(h.text(), "abcd");
        assert_eq!(h.cursor(), Position::new(0, 2));
    }

    #[test]
    fn backspace_removes_a_whole_indent_level() {
        let mut h = Harness::new("        x").at(0, 8);
        h.run("deleteLeft");
        // Four spaces, not one.
        assert_eq!(h.text(), "    x");
    }

    #[test]
    fn backspace_removes_one_character_when_not_on_an_indent_boundary() {
        let mut h = Harness::new("      x").at(0, 6);
        h.run("deleteLeft");
        assert_eq!(h.text(), "     x");
    }

    #[test]
    fn backspace_deletes_the_selection_when_there_is_one() {
        let mut h = Harness::new("hello").selecting((0, 1), (0, 4));
        h.run("deleteLeft");
        assert_eq!(h.text(), "ho");
    }

    #[test]
    fn delete_removes_the_character_to_the_right() {
        let mut h = Harness::new("abc").at(0, 1);
        h.run("deleteRight");
        assert_eq!(h.text(), "ac");
        assert_eq!(h.cursor(), Position::new(0, 1));
    }

    #[test]
    fn delete_word_removes_a_whole_word() {
        let mut h = Harness::new("hello world").at(0, 5);
        h.run("deleteWordLeft");
        assert_eq!(h.text(), " world");

        let mut h = Harness::new("hello world").at(0, 5);
        h.run("deleteWordRight");
        assert_eq!(h.text(), "hello");
    }

    #[test]
    fn deleting_at_the_document_edges_does_nothing() {
        let mut h = Harness::new("abc").at(0, 0);
        h.run("deleteLeft");
        assert_eq!(h.text(), "abc");

        let mut h = Harness::new("abc").at(0, 3);
        h.run("deleteRight");
        assert_eq!(h.text(), "abc");
    }

    #[test]
    fn arrow_keys_move_the_caret() {
        let mut h = Harness::new("abc\ndef").at(0, 1);
        h.run("cursorRight");
        assert_eq!(h.cursor(), Position::new(0, 2));
        h.run("cursorLeft");
        assert_eq!(h.cursor(), Position::new(0, 1));
        h.run("cursorDown");
        assert_eq!(h.cursor(), Position::new(1, 1));
        h.run("cursorUp");
        assert_eq!(h.cursor(), Position::new(0, 1));
    }

    #[test]
    fn a_plain_arrow_collapses_a_selection_to_its_edge() {
        let mut h = Harness::new("hello").selecting((0, 1), (0, 4));
        h.run("cursorLeft");
        assert_eq!(h.cursor(), Position::new(0, 1));
        assert!(h.view.selections.primary().is_empty());

        let mut h = Harness::new("hello").selecting((0, 1), (0, 4));
        h.run("cursorRight");
        assert_eq!(h.cursor(), Position::new(0, 4));
    }

    #[test]
    fn shift_arrow_extends_the_selection() {
        let mut h = Harness::new("hello").at(0, 1);
        h.run("cursorRightSelect");
        h.run("cursorRightSelect");
        let selection = h.view.selections.primary();
        assert_eq!(selection.anchor, Position::new(0, 1));
        assert_eq!(selection.active, Position::new(0, 3));
    }

    #[test]
    fn vertical_motion_keeps_its_goal_column() {
        let mut h = Harness::new("aaaaaaaa\nbb\ncccccccc").at(0, 6);
        h.run("cursorDown");
        assert_eq!(h.cursor(), Position::new(1, 2));
        h.run("cursorDown");
        assert_eq!(h.cursor(), Position::new(2, 6));
    }

    #[test]
    fn home_and_end_use_smart_home() {
        let mut h = Harness::new("    indented").at(0, 8);
        h.run("cursorHome");
        assert_eq!(h.cursor(), Position::new(0, 4));
        h.run("cursorHome");
        assert_eq!(h.cursor(), Position::new(0, 0));
        h.run("cursorEnd");
        assert_eq!(h.cursor(), Position::new(0, 12));
    }

    #[test]
    fn top_and_bottom_reach_the_document_ends() {
        let mut h = Harness::new("a\nb\nc").at(1, 1);
        h.run("cursorBottom");
        assert_eq!(h.cursor(), Position::new(2, 1));
        h.run("cursorTop");
        assert_eq!(h.cursor(), Position::ZERO);
    }

    #[test]
    fn page_motion_moves_by_the_viewport_height() {
        let mut h = Harness::new(&"x\n".repeat(100)).at(0, 0);
        h.view.height = 10;
        h.run("cursorPageDown");
        assert_eq!(h.cursor().line, 9);
    }

    #[test]
    fn moving_the_caret_breaks_the_undo_group() {
        let mut h = Harness::new("").at(0, 0);
        // Same undo group without the motion in between.
        h.clock = 0;
        let mut ctx = Context {
            document: &mut h.document,
            view: &mut h.view,
            clipboard: &mut h.clipboard,
            now_ms: 0,
        };
        execute(&mut ctx, "type", Some(&json!({"text": "ab"})));
        execute(&mut ctx, "cursorLeft", None);
        execute(&mut ctx, "type", Some(&json!({"text": "c"})));
        assert_eq!(h.text(), "acb");

        h.run("undo");
        assert_eq!(
            h.text(),
            "ab",
            "the motion should have started a new undo step"
        );
    }

    #[test]
    fn select_all_covers_the_document() {
        let mut h = Harness::new("abc\ndef");
        h.run("editor.action.selectAll");
        let selection = h.view.selections.primary();
        assert_eq!(selection.start(), Position::ZERO);
        assert_eq!(selection.end(), Position::new(1, 3));
    }

    #[test]
    fn tab_inserts_an_indent_unit() {
        let mut h = Harness::new("x").at(0, 0);
        h.run("tab");
        assert_eq!(h.text(), "    x");
    }

    #[test]
    fn tab_indents_a_multi_line_selection() {
        let mut h = Harness::new("a\nb\nc").selecting((0, 0), (2, 1));
        h.run("tab");
        assert_eq!(h.text(), "    a\n    b\n    c");
    }

    #[test]
    fn outdent_removes_one_level_and_stops_at_the_text() {
        let mut h = Harness::new("        a\n  b\nc").selecting((0, 0), (2, 1));
        h.run("outdent");
        assert_eq!(h.text(), "    a\nb\nc");
    }

    #[test]
    fn indent_and_outdent_are_inverses() {
        let mut h = Harness::new("a\n  b\n\tc").selecting((0, 0), (2, 2));
        let original = h.text();
        h.run("editor.action.indentLines");
        h.run("editor.action.outdentLines");
        assert_eq!(h.text(), original);
    }

    #[test]
    fn tabs_are_inserted_when_insert_spaces_is_off() {
        let mut h = Harness::new("x").at(0, 0);
        h.document.settings.insert_spaces = false;
        h.run("tab");
        assert_eq!(h.text(), "\tx");
    }

    #[test]
    fn extra_cursors_can_be_added_above_and_below() {
        let mut h = Harness::new("aaa\nbbb\nccc").at(1, 1);
        h.run("editor.action.insertCursorBelow");
        assert_eq!(h.view.selections.len(), 2);
        h.run("editor.action.insertCursorAbove");
        // Above the *primary*, which the previous command moved to line 2.
        assert_eq!(h.view.selections.len(), 2);
    }

    #[test]
    fn adding_a_cursor_past_the_document_does_nothing() {
        let mut h = Harness::new("only line").at(0, 0);
        h.run("editor.action.insertCursorAbove");
        assert_eq!(h.view.selections.len(), 1);
        h.run("editor.action.insertCursorBelow");
        assert_eq!(h.view.selections.len(), 1);
    }

    #[test]
    fn escape_collapses_to_the_primary_cursor() {
        let mut h = Harness::new("a\nb\nc");
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::caret(Position::new(0, 0)),
                Selection::caret(Position::new(1, 0)),
            ],
            0,
        );
        h.run("removeSecondaryCursors");
        assert_eq!(h.view.selections.len(), 1);
    }

    #[test]
    fn deleting_a_line_removes_it_and_its_terminator() {
        let mut h = Harness::new("a\nb\nc").at(1, 0);
        h.run("editor.action.deleteLines");
        assert_eq!(h.text(), "a\nc");
    }

    #[test]
    fn deleting_the_last_line_does_not_leave_a_blank_one() {
        let mut h = Harness::new("a\nb").at(1, 0);
        h.run("editor.action.deleteLines");
        assert_eq!(h.text(), "a");
    }

    #[test]
    fn deleting_the_only_line_empties_the_document() {
        let mut h = Harness::new("only").at(0, 0);
        h.run("editor.action.deleteLines");
        assert_eq!(h.text(), "");
    }

    #[test]
    fn insert_line_below_keeps_the_indentation() {
        let mut h = Harness::new("    abc").at(0, 2);
        h.run("editor.action.insertLineAfter");
        assert_eq!(h.text(), "    abc\n    ");
        assert_eq!(h.cursor(), Position::new(1, 4));
    }

    #[test]
    fn insert_line_above_keeps_the_indentation() {
        let mut h = Harness::new("    abc").at(0, 6);
        h.run("editor.action.insertLineBefore");
        assert_eq!(h.text(), "    \n    abc");
        assert_eq!(h.cursor(), Position::new(0, 4));
    }

    #[test]
    fn lines_move_up_and_down() {
        let mut h = Harness::new("a\nb\nc").at(1, 0);
        h.run("editor.action.moveLinesUpAction");
        assert_eq!(h.text(), "b\na\nc");
        assert_eq!(h.cursor().line, 0);

        h.run("editor.action.moveLinesDownAction");
        assert_eq!(h.text(), "a\nb\nc");
    }

    #[test]
    fn moving_lines_past_the_edges_does_nothing() {
        let mut h = Harness::new("a\nb").at(0, 0);
        h.run("editor.action.moveLinesUpAction");
        assert_eq!(h.text(), "a\nb");

        let mut h = Harness::new("a\nb").at(1, 0);
        h.run("editor.action.moveLinesDownAction");
        assert_eq!(h.text(), "a\nb");
    }

    #[test]
    fn a_multi_line_selection_moves_as_a_block() {
        let mut h = Harness::new("a\nb\nc\nd").selecting((1, 0), (2, 1));
        h.run("editor.action.moveLinesDownAction");
        assert_eq!(h.text(), "a\nd\nb\nc");
    }

    #[test]
    fn lines_can_be_duplicated() {
        let mut h = Harness::new("a\nb").at(0, 0);
        h.run("editor.action.copyLinesDownAction");
        assert_eq!(h.text(), "a\na\nb");
        assert_eq!(h.cursor().line, 1, "the caret follows the copy downwards");
    }

    #[test]
    fn comment_toggling_is_its_own_inverse() {
        let mut h =
            Harness::with_language("fn main() {}\nlet x = 1;", "rs").selecting((0, 0), (1, 5));
        h.run("editor.action.commentLine");
        assert_eq!(h.text(), "// fn main() {}\n// let x = 1;");
        h.run("editor.action.commentLine");
        assert_eq!(h.text(), "fn main() {}\nlet x = 1;");
    }

    #[test]
    fn commenting_preserves_indentation() {
        let mut h = Harness::with_language("    let x = 1;", "rs").at(0, 0);
        h.run("editor.action.commentLine");
        assert_eq!(h.text(), "    // let x = 1;");
    }

    #[test]
    fn a_partly_commented_selection_becomes_fully_commented() {
        let mut h = Harness::with_language("// a\nb", "rs").selecting((0, 0), (1, 1));
        h.run("editor.action.commentLine");
        assert_eq!(h.text(), "// // a\n// b");
    }

    #[test]
    fn commenting_uses_the_language_token() {
        let mut h = Harness::with_language("x = 1", "py").at(0, 0);
        h.run("editor.action.commentLine");
        assert_eq!(h.text(), "# x = 1");
    }

    #[test]
    fn add_and_remove_comment_are_unconditional() {
        let mut h = Harness::with_language("// a\nb", "rs").selecting((0, 0), (1, 1));
        h.run("editor.action.addCommentLine");
        assert_eq!(h.text(), "// // a\n// b");
        h.run("editor.action.removeCommentLine");
        assert_eq!(h.text(), "// a\nb");
    }

    #[test]
    fn removing_comments_leaves_uncommented_lines_alone() {
        // Without the guard this would delete the first two characters of `bb`.
        let mut h = Harness::with_language("// a\nbb", "rs").selecting((0, 0), (1, 2));
        h.run("editor.action.removeCommentLine");
        assert_eq!(h.text(), "a\nbb");
    }

    #[test]
    fn commenting_a_language_without_a_token_does_nothing() {
        let mut h = Harness::with_language("<p>hi</p>", "html").at(0, 0);
        h.run("editor.action.commentLine");
        assert_eq!(h.text(), "<p>hi</p>");
    }

    #[test]
    fn blank_lines_are_left_alone_when_commenting() {
        let mut h = Harness::with_language("a\n\nb", "rs").selecting((0, 0), (2, 1));
        h.run("editor.action.commentLine");
        assert_eq!(h.text(), "// a\n\n// b");
    }

    #[test]
    fn copy_and_paste_move_text() {
        let mut h = Harness::new("hello world").selecting((0, 0), (0, 5));
        h.run("editor.action.clipboardCopyAction");
        assert_eq!(h.clipboard.read(), "hello");

        h.view.selections = SelectionSet::caret(Position::new(0, 11));
        h.run("editor.action.clipboardPasteAction");
        assert_eq!(h.text(), "hello worldhello");
    }

    #[test]
    fn copying_with_no_selection_takes_the_whole_line() {
        let mut h = Harness::new("first\nsecond").at(0, 2);
        h.run("editor.action.clipboardCopyAction");
        assert_eq!(h.clipboard.read(), "first\n");
    }

    #[test]
    fn cut_removes_what_it_copied() {
        let mut h = Harness::new("hello world").selecting((0, 0), (0, 6));
        h.run("editor.action.clipboardCutAction");
        assert_eq!(h.clipboard.read(), "hello ");
        assert_eq!(h.text(), "world");
    }

    #[test]
    fn cut_with_no_selection_takes_the_line() {
        let mut h = Harness::new("a\nb\nc").at(1, 0);
        h.run("editor.action.clipboardCutAction");
        assert_eq!(h.clipboard.read(), "b\n");
        assert_eq!(h.text(), "a\nc");
    }

    #[test]
    fn save_and_quit_are_reported_to_the_frontend() {
        let mut h = Harness::new("x");
        assert_eq!(h.run("workbench.action.files.save"), Outcome::Save);
        assert_eq!(h.run("workbench.action.quit"), Outcome::Quit);
    }

    #[test]
    fn editing_scrolls_the_cursor_into_view() {
        let mut h = Harness::new(&"x\n".repeat(100)).at(80, 0);
        h.view.height = 10;
        h.view.scroll_top = 0;
        h.type_text("y");
        assert!(
            h.view.visible_lines(&h.document.buffer).contains(&80),
            "line 80 should be visible, got {:?}",
            h.view.visible_lines(&h.document.buffer)
        );
    }

    #[test]
    fn editing_a_document_with_astral_characters_keeps_offsets_right() {
        let mut h = Harness::new("a😀b").at(0, 3);
        h.type_text("X");
        assert_eq!(h.text(), "a😀Xb");
        h.run("deleteLeft");
        assert_eq!(h.text(), "a😀b");
        h.run("deleteLeft");
        assert_eq!(h.text(), "ab", "backspace should remove the whole emoji");
    }

    /// Every selection as `(line, start_character)..(line, end_character)`,
    /// in document order.
    fn spans(h: &Harness) -> Vec<((u32, u32), (u32, u32))> {
        h.view
            .selections
            .iter()
            .map(|s| {
                (
                    (s.start().line, s.start().character),
                    (s.end().line, s.end().character),
                )
            })
            .collect()
    }

    const THREE_FOOS: &str = "foo bar\nfoo baz\nqux foo\n";

    #[test]
    fn the_first_add_next_match_only_selects_the_word_under_the_caret() {
        let mut h = Harness::new(THREE_FOOS).at(0, 1);
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((0, 0), (0, 3))]);
    }

    #[test]
    fn add_next_match_adds_a_cursor_at_each_following_occurrence() {
        let mut h = Harness::new(THREE_FOOS).at(0, 1);
        h.run("editor.action.addSelectionToNextFindMatch");
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((0, 0), (0, 3)), ((1, 0), (1, 3))]);
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(
            spans(&h),
            vec![((0, 0), (0, 3)), ((1, 0), (1, 3)), ((2, 4), (2, 7))]
        );
    }

    #[test]
    fn add_next_match_wraps_from_the_last_occurrence_to_the_first() {
        // Starting on the *last* occurrence: the only match after it is at the
        // top of the file.
        let mut h = Harness::new(THREE_FOOS).selecting((2, 4), (2, 7));
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((0, 0), (0, 3)), ((2, 4), (2, 7))]);
    }

    #[test]
    fn add_next_match_says_so_once_every_occurrence_is_selected() {
        let mut h = Harness::new(THREE_FOOS).at(0, 1);
        for _ in 0..4 {
            h.run("editor.action.addSelectionToNextFindMatch");
        }
        assert_eq!(h.view.selections.len(), 3);
        let outcome = h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(
            outcome,
            Outcome::Message("all 3 occurrences of \"foo\" are selected".to_owned())
        );
        assert_eq!(h.view.selections.len(), 3, "no cursor should have moved");
    }

    #[test]
    fn add_next_match_searches_for_the_selected_text_not_the_word() {
        // A partial selection is a search term in its own right, so `oo` matches
        // inside every `foo` — which a word-based search would miss.
        let mut h = Harness::new(THREE_FOOS).selecting((0, 1), (0, 3));
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((0, 1), (0, 3)), ((1, 1), (1, 3))]);
    }

    #[test]
    fn add_next_match_matches_case_exactly() {
        let mut h = Harness::new("foo\nFOO\nfoo\n").at(0, 0);
        h.run("editor.action.addSelectionToNextFindMatch");
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(
            spans(&h),
            vec![((0, 0), (0, 3)), ((2, 0), (2, 3))],
            "FOO is a different string"
        );
    }

    #[test]
    fn add_next_match_on_a_caret_in_whitespace_does_nothing() {
        let mut h = Harness::new("foo   bar").at(0, 4);
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((0, 4), (0, 4))]);
    }

    #[test]
    fn expanding_a_caret_leaves_an_existing_selection_as_the_user_made_it() {
        let mut h = Harness::new(THREE_FOOS);
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::caret(Position::new(0, 1)),
                Selection::new(Position::new(1, 4), Position::new(1, 7)),
            ],
            0,
        );
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((0, 0), (0, 3)), ((1, 4), (1, 7))]);
    }

    #[test]
    fn every_caret_expands_to_its_own_word() {
        // What VS Code does. The cursors were placed deliberately and expanding
        // each of them keeps that placement; the alternative the old comment
        // argued against — discarding them — was never on the table.
        let mut h = Harness::new(THREE_FOOS);
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::caret(Position::new(0, 1)),
                Selection::caret(Position::new(1, 1)),
            ],
            0,
        );
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((0, 0), (0, 3)), ((1, 0), (1, 3))]);
    }

    #[test]
    fn a_caret_with_no_word_under_it_stays_a_caret() {
        // Selecting the whitespace it sits in would be worse than leaving it.
        let mut h = Harness::new("foo\n   \n");
        h.view.selections = SelectionSet::from_vec(
            vec![
                Selection::caret(Position::new(0, 1)),
                Selection::caret(Position::new(1, 1)),
            ],
            0,
        );
        h.run("editor.action.addSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((0, 0), (0, 3)), ((1, 1), (1, 1))]);
    }

    #[test]
    fn select_all_matches_puts_a_cursor_on_every_occurrence() {
        let mut h = Harness::new(THREE_FOOS).at(0, 1);
        let outcome = h.run("editor.action.selectHighlights");
        assert_eq!(
            spans(&h),
            vec![((0, 0), (0, 3)), ((1, 0), (1, 3)), ((2, 4), (2, 7))]
        );
        assert_eq!(
            outcome,
            Outcome::Message("3 occurrences selected".to_owned())
        );
    }

    #[test]
    fn select_all_matches_makes_the_last_occurrence_primary() {
        let mut h = Harness::new(THREE_FOOS).at(0, 1);
        h.run("editor.action.selectHighlights");
        assert_eq!(h.view.selections.primary_index(), 2);
        assert_eq!(h.cursor(), Position::new(2, 7));
    }

    #[test]
    fn select_all_matches_then_typing_replaces_every_occurrence() {
        let mut h = Harness::new(THREE_FOOS).at(0, 1);
        h.run("editor.action.selectHighlights");
        h.type_text("quux");
        assert_eq!(h.text(), "quux bar\nquux baz\nqux quux\n");
    }

    #[test]
    fn move_to_next_match_moves_the_cursor_instead_of_adding_one() {
        let mut h = Harness::new(THREE_FOOS).at(0, 1);
        h.run("editor.action.addSelectionToNextFindMatch");
        h.run("editor.action.moveSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((1, 0), (1, 3))]);
    }

    #[test]
    fn move_to_next_match_moves_only_the_primary_cursor() {
        let mut h = Harness::new(THREE_FOOS).at(0, 1);
        h.run("editor.action.addSelectionToNextFindMatch");
        h.run("editor.action.addSelectionToNextFindMatch");
        // The second occurrence is primary; it skips ahead to the third and the
        // first stays put.
        h.run("editor.action.moveSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((0, 0), (0, 3)), ((2, 4), (2, 7))]);
    }

    #[test]
    fn the_first_move_to_next_match_only_selects_the_word() {
        let mut h = Harness::new(THREE_FOOS).at(2, 5);
        h.run("editor.action.moveSelectionToNextFindMatch");
        assert_eq!(spans(&h), vec![((2, 4), (2, 7))]);
    }

    #[test]
    fn the_palette_has_no_duplicates_and_every_entry_is_titled() {
        let mut ids: Vec<&str> = PALETTE.iter().map(|(id, _)| *id).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(before, ids.len(), "an identifier is listed twice");

        for (id, title) in PALETTE {
            assert!(!title.is_empty(), "{id} has no title");
            // A title that is just the identifier is not a title; the palette is
            // searched by what things are called.
            assert_ne!(title, id, "{id} needs a human title");
        }
    }
}
