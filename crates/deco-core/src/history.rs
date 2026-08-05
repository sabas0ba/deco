//! Undo/redo with VS Code-style edit coalescing.

use crate::buffer::Buffer;
use crate::edit::Transaction;
use crate::selection::SelectionSet;

/// What kind of edit produced a history step. Only edits of the same kind are
/// coalesced, so a burst of typing is one undo step but "type, delete, type" is
/// three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Text was typed.
    Insert,
    /// Text was removed with backspace/delete.
    Delete,
    /// Anything else — paste, formatting, refactors, multi-cursor rewrites.
    /// Never coalesced, because users expect one undo to revert exactly one.
    Discrete,
}

/// Tuning for [`History`].
#[derive(Debug, Clone, Copy)]
pub struct HistoryOptions {
    /// How long after an edit a same-kind edit still joins the same undo step.
    pub coalesce_window_ms: u64,
    /// Maximum number of undo steps retained; the oldest are dropped first.
    pub max_entries: usize,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        // 500ms matches the feel of VS Code's typing groups closely enough that
        // muscle memory transfers.
        Self {
            coalesce_window_ms: 500,
            max_entries: 1000,
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    /// Transactions to apply, in this exact order, to move the buffer to the
    /// other side of this history step.
    steps: Vec<Transaction>,
    /// Selection to restore after applying `steps`.
    selection_after_apply: SelectionSet,
    /// Selection the opposite-direction entry should restore.
    selection_before_apply: SelectionSet,
    kind: EditKind,
    last_ms: u64,
}

/// An undo/redo stack bound to a single buffer.
///
/// The history never stores document snapshots — only invertible transactions —
/// so memory stays proportional to how much was edited, not to file size.
#[derive(Debug, Clone)]
pub struct History {
    undo_stack: Vec<Entry>,
    redo_stack: Vec<Entry>,
    options: HistoryOptions,
}

impl Default for History {
    fn default() -> Self {
        Self::new(HistoryOptions::default())
    }
}

impl History {
    /// Builds an empty history.
    pub fn new(options: HistoryOptions) -> Self {
        Self {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            options,
        }
    }

    /// Number of available undo steps.
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    /// Number of available redo steps.
    pub fn redo_depth(&self) -> usize {
        self.redo_stack.len()
    }

    /// Whether [`History::undo`] would do anything.
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// Whether [`History::redo`] would do anything.
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Drops all recorded steps.
    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }

    /// Forces the next edit to start a new undo step even if it would otherwise
    /// coalesce. Called when the cursor moves, the file is saved, or focus
    /// changes — all the points where VS Code breaks a typing group.
    pub fn break_group(&mut self) {
        if let Some(entry) = self.undo_stack.last_mut() {
            entry.kind = EditKind::Discrete;
        }
    }

    /// Records an applied edit.
    ///
    /// `inverse` is what [`Buffer::apply`] returned, `now_ms` is a monotonic
    /// timestamp supplied by the frontend — the core deliberately owns no clock
    /// so that history behaviour is deterministic under test.
    pub fn record(
        &mut self,
        inverse: Transaction,
        kind: EditKind,
        selection_before: SelectionSet,
        selection_after: SelectionSet,
        now_ms: u64,
    ) {
        if inverse.is_empty() {
            return;
        }
        self.redo_stack.clear();

        let coalesce = matches!(kind, EditKind::Insert | EditKind::Delete)
            && self.undo_stack.last().is_some_and(|last| {
                last.kind == kind
                    && now_ms.saturating_sub(last.last_ms) <= self.options.coalesce_window_ms
            });

        if coalesce {
            let last = self.undo_stack.last_mut().expect("checked above");
            // Undoing the group must replay the newest inverse first.
            last.steps.insert(0, inverse);
            last.selection_before_apply = selection_after;
            last.last_ms = now_ms;
            return;
        }

        self.undo_stack.push(Entry {
            steps: vec![inverse],
            selection_after_apply: selection_before,
            selection_before_apply: selection_after,
            kind,
            last_ms: now_ms,
        });

        if self.undo_stack.len() > self.options.max_entries {
            let overflow = self.undo_stack.len() - self.options.max_entries;
            self.undo_stack.drain(..overflow);
        }
    }

    /// Undoes one step, returning the selection to restore.
    pub fn undo(&mut self, buffer: &mut Buffer) -> Option<SelectionSet> {
        let entry = self.undo_stack.pop()?;
        let opposite = Self::apply_entry(buffer, entry);
        // `apply_entry` returns the entry destined for the *other* stack; the
        // selection to show now is the one that entry would undo back to.
        let restored = opposite.selection_before_apply.clone();
        self.redo_stack.push(opposite);
        Some(restored)
    }

    /// Redoes one step, returning the selection to restore.
    pub fn redo(&mut self, buffer: &mut Buffer) -> Option<SelectionSet> {
        let entry = self.redo_stack.pop()?;
        let opposite = Self::apply_entry(buffer, entry);
        let restored = opposite.selection_before_apply.clone();
        self.undo_stack.push(opposite);
        Some(restored)
    }

    /// Applies `entry` to `buffer` and builds the entry that reverses it.
    fn apply_entry(buffer: &mut Buffer, entry: Entry) -> Entry {
        let mut inverses = Vec::with_capacity(entry.steps.len());
        for tx in &entry.steps {
            inverses.push(buffer.apply(tx));
        }
        // Applying [A, B] means the reversal is [inv(B), inv(A)].
        inverses.reverse();

        Entry {
            steps: inverses,
            // Applying the reversal returns us to where we were before `entry`.
            selection_after_apply: entry.selection_before_apply.clone(),
            selection_before_apply: entry.selection_after_apply,
            kind: EditKind::Discrete,
            last_ms: entry.last_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Change;
    use crate::position::Position;
    use crate::selection::Selection;

    /// Applies `text` at `pos`, recording it in `history`.
    fn type_text(
        buffer: &mut Buffer,
        history: &mut History,
        pos: Position,
        text: &str,
        now_ms: u64,
    ) {
        let before = SelectionSet::caret(pos);
        let tx = Transaction::single(Change::insert(pos, text.to_owned()));
        let inverse = buffer.apply(&tx);
        let after = SelectionSet::caret(
            buffer.char_to_position(buffer.position_to_char(pos) + text.chars().count()),
        );
        history.record(inverse, EditKind::Insert, before, after, now_ms);
    }

    #[test]
    fn undo_and_redo_round_trip() {
        let mut buffer = Buffer::from_text("");
        let mut history = History::default();

        type_text(&mut buffer, &mut history, Position::new(0, 0), "hello", 0);
        assert_eq!(buffer.text(), "hello");

        history.undo(&mut buffer);
        assert_eq!(buffer.text(), "");

        history.redo(&mut buffer);
        assert_eq!(buffer.text(), "hello");
    }

    #[test]
    fn fast_typing_coalesces_into_one_undo_step() {
        let mut buffer = Buffer::from_text("");
        let mut history = History::default();

        type_text(&mut buffer, &mut history, Position::new(0, 0), "a", 0);
        type_text(&mut buffer, &mut history, Position::new(0, 1), "b", 100);
        type_text(&mut buffer, &mut history, Position::new(0, 2), "c", 200);

        assert_eq!(buffer.text(), "abc");
        assert_eq!(history.undo_depth(), 1);

        history.undo(&mut buffer);
        assert_eq!(buffer.text(), "");
        assert!(!history.can_undo());

        history.redo(&mut buffer);
        assert_eq!(buffer.text(), "abc");
    }

    #[test]
    fn a_pause_starts_a_new_undo_step() {
        let mut buffer = Buffer::from_text("");
        let mut history = History::default();

        type_text(&mut buffer, &mut history, Position::new(0, 0), "ab", 0);
        type_text(&mut buffer, &mut history, Position::new(0, 2), "cd", 5_000);

        assert_eq!(history.undo_depth(), 2);
        history.undo(&mut buffer);
        assert_eq!(buffer.text(), "ab");
        history.undo(&mut buffer);
        assert_eq!(buffer.text(), "");
    }

    #[test]
    fn break_group_splits_a_typing_run() {
        let mut buffer = Buffer::from_text("");
        let mut history = History::default();

        type_text(&mut buffer, &mut history, Position::new(0, 0), "ab", 0);
        history.break_group(); // e.g. the user clicked elsewhere
        type_text(&mut buffer, &mut history, Position::new(0, 2), "cd", 10);

        assert_eq!(history.undo_depth(), 2);
        history.undo(&mut buffer);
        assert_eq!(buffer.text(), "ab");
    }

    #[test]
    fn discrete_edits_never_coalesce() {
        let mut buffer = Buffer::from_text("");
        let mut history = History::default();
        for (idx, text) in ["x", "y", "z"].iter().enumerate() {
            let pos = Position::new(0, idx as u32);
            let inverse = buffer.apply(&Transaction::single(Change::insert(
                pos,
                (*text).to_owned(),
            )));
            history.record(
                inverse,
                EditKind::Discrete,
                SelectionSet::caret(pos),
                SelectionSet::caret(Position::new(0, idx as u32 + 1)),
                0,
            );
        }
        assert_eq!(history.undo_depth(), 3);
    }

    #[test]
    fn new_edit_clears_the_redo_stack() {
        let mut buffer = Buffer::from_text("");
        let mut history = History::default();

        type_text(&mut buffer, &mut history, Position::new(0, 0), "one", 0);
        history.undo(&mut buffer);
        assert!(history.can_redo());

        type_text(
            &mut buffer,
            &mut history,
            Position::new(0, 0),
            "two",
            10_000,
        );
        assert!(!history.can_redo());
    }

    #[test]
    fn undo_restores_the_pre_edit_selection() {
        let mut buffer = Buffer::from_text("hello");
        let mut history = History::default();

        let before = SelectionSet::single(Selection::new(Position::new(0, 0), Position::new(0, 5)));
        let inverse = buffer.apply(&Transaction::single(Change::replace(
            crate::position::Range::new(Position::new(0, 0), Position::new(0, 5)),
            "bye".into(),
        )));
        history.record(
            inverse,
            EditKind::Discrete,
            before.clone(),
            SelectionSet::caret(Position::new(0, 3)),
            0,
        );

        let restored = history.undo(&mut buffer).unwrap();
        assert_eq!(buffer.text(), "hello");
        assert_eq!(restored, before);
    }

    #[test]
    fn redo_restores_the_post_edit_selection() {
        let mut buffer = Buffer::from_text("hello");
        let mut history = History::default();

        let after = SelectionSet::caret(Position::new(0, 3));
        let inverse = buffer.apply(&Transaction::single(Change::replace(
            crate::position::Range::new(Position::new(0, 0), Position::new(0, 5)),
            "bye".into(),
        )));
        history.record(
            inverse,
            EditKind::Discrete,
            SelectionSet::caret(Position::new(0, 0)),
            after.clone(),
            0,
        );

        history.undo(&mut buffer);
        let restored = history.redo(&mut buffer).unwrap();
        assert_eq!(buffer.text(), "bye");
        assert_eq!(restored, after);
    }

    #[test]
    fn multi_step_group_undoes_and_redoes_in_the_right_order() {
        let mut buffer = Buffer::from_text("");
        let mut history = History::default();

        type_text(&mut buffer, &mut history, Position::new(0, 0), "1", 0);
        type_text(&mut buffer, &mut history, Position::new(0, 1), "2", 10);
        type_text(&mut buffer, &mut history, Position::new(0, 2), "3", 20);
        assert_eq!(buffer.text(), "123");

        history.undo(&mut buffer);
        assert_eq!(buffer.text(), "");
        history.redo(&mut buffer);
        assert_eq!(buffer.text(), "123");
        history.undo(&mut buffer);
        assert_eq!(buffer.text(), "");
    }

    #[test]
    fn history_is_bounded() {
        let mut buffer = Buffer::from_text("");
        let mut history = History::new(HistoryOptions {
            coalesce_window_ms: 0,
            max_entries: 3,
        });
        for i in 0..10u64 {
            let pos = Position::new(0, i as u32);
            let inverse = buffer.apply(&Transaction::single(Change::insert(pos, "x".into())));
            history.record(
                inverse,
                EditKind::Discrete,
                SelectionSet::caret(pos),
                SelectionSet::caret(pos),
                i * 1000,
            );
        }
        assert_eq!(history.undo_depth(), 3);
    }

    #[test]
    fn undo_on_empty_history_is_a_noop() {
        let mut buffer = Buffer::from_text("abc");
        let mut history = History::default();
        assert!(history.undo(&mut buffer).is_none());
        assert_eq!(buffer.text(), "abc");
    }
}
