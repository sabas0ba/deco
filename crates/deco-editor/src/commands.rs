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

use crate::document::{line_comment_token, Document, View};

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
    /// The editor should exit.
    Quit,
    /// Something worth telling the user.
    Message(String),
}

/// One entry in the command palette: what to run, and what to call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    /// The command identifier, VS Code's.
    pub id: String,
    /// The title to show, VS Code's wording where it has one.
    pub title: String,
}

impl PaletteEntry {
    /// Builds an entry from a borrowed pair.
    pub fn new(id: &str, title: &str) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
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
pub const PALETTE: &[(&str, &str)] = &[
    ("undo", "Undo"),
    ("redo", "Redo"),
    ("editor.action.selectAll", "Select All"),
    ("expandLineSelection", "Expand Line Selection"),
    ("removeSecondaryCursors", "Remove Secondary Cursors"),
    ("editor.action.commentLine", "Toggle Line Comment"),
    ("editor.action.addCommentLine", "Add Line Comment"),
    ("editor.action.removeCommentLine", "Remove Line Comment"),
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
        ctx.view
            .selections
            .map(|s| movement::vertical(buffer, *s, direction, count, tab_size, extend));
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

    let mut planned: Vec<(usize, usize, String)> = Vec::new();
    for selection in before.iter() {
        let Some((range, text)) = plan(buffer, selection) else {
            continue;
        };
        let range = buffer.clamp_range(range);
        planned.push((
            buffer.position_to_char(range.start),
            buffer.position_to_char(range.end),
            text,
        ));
    }
    if planned.is_empty() {
        return;
    }
    planned.sort_by_key(|(start, end, _)| (*start, *end));

    let changes: Vec<Change> = planned
        .iter()
        .map(|(start, end, text)| {
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
    for (start, end, text) in &planned {
        let inserted = text.chars().count();
        let new_start = (*start as isize + delta) as usize;
        carets.push(Selection::caret(
            ctx.document.buffer.char_to_position(new_start + inserted),
        ));
        delta += inserted as isize - (*end - *start) as isize;
    }

    let after = SelectionSet::from_vec(carets, 0);
    ctx.view.selections = after.clone();
    ctx.document
        .history
        .record(inverse, kind, before, after, ctx.now_ms);
    ctx.document.dirty = true;
}

/// Replaces each selection with `text`.
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
    // Only the primary cursor is expanded. The others are left alone: they were
    // placed deliberately, and throwing them away to answer a question about the
    // word under one of them would lose more than it gains.
    let index = ctx.view.selections.primary_index();
    let mut selections = ctx.view.selections.as_slice().to_vec();
    selections[index] = selection;
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
                EditorSettings::default(),
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
    fn add_next_match_leaves_the_other_cursors_alone_when_expanding_a_caret() {
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
