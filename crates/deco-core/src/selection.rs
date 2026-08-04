//! Cursors and selections, including the multi-cursor set.

use crate::position::{Position, Range};

/// A single cursor with an optional selected region.
///
/// `anchor` is the fixed end (where the selection started) and `active` is the
/// end that moves with the caret, so `anchor > active` for a backwards
/// selection. Keeping direction lets shift+arrow shrink a selection instead of
/// always growing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Selection {
    /// The fixed end of the selection.
    pub anchor: Position,
    /// The moving end — where the caret is drawn.
    pub active: Position,
    /// Column the caret "wants" when moving vertically.
    ///
    /// Moving down from column 40 through a 3-character line and back must
    /// return to column 40; without a sticky goal column the caret would get
    /// dragged left permanently.
    pub goal_column: Option<u32>,
}

impl Default for Selection {
    fn default() -> Self {
        Self::caret(Position::ZERO)
    }
}

impl Selection {
    /// A collapsed selection (a bare caret) at `pos`.
    pub fn caret(pos: Position) -> Self {
        Self {
            anchor: pos,
            active: pos,
            goal_column: None,
        }
    }

    /// A selection from `anchor` to `active`.
    pub fn new(anchor: Position, active: Position) -> Self {
        Self {
            anchor,
            active,
            goal_column: None,
        }
    }

    /// Whether nothing is selected.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.active
    }

    /// Whether `active` comes before `anchor`.
    pub fn is_reversed(&self) -> bool {
        self.active < self.anchor
    }

    /// The earlier of the two ends.
    pub fn start(&self) -> Position {
        self.anchor.min(self.active)
    }

    /// The later of the two ends.
    pub fn end(&self) -> Position {
        self.anchor.max(self.active)
    }

    /// The selection as an ordered range.
    pub fn range(&self) -> Range {
        Range::new(self.start(), self.end())
    }

    /// Collapses to a caret at `active`.
    pub fn collapsed(self) -> Self {
        Self {
            anchor: self.active,
            active: self.active,
            goal_column: self.goal_column,
        }
    }

    /// Moves `active` to `pos`, keeping `anchor` (an extending move).
    pub fn extended_to(self, pos: Position) -> Self {
        Self {
            anchor: self.anchor,
            active: pos,
            goal_column: None,
        }
    }

    /// Moves both ends to `pos` (a non-extending move).
    pub fn moved_to(self, pos: Position) -> Self {
        Self {
            anchor: pos,
            active: pos,
            goal_column: None,
        }
    }
}

/// The set of cursors in a view, with exactly one marked primary.
///
/// The primary cursor is the one that drives scrolling, the status bar and any
/// command that only makes sense once (e.g. "reveal definition").
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SelectionSet {
    selections: Vec<Selection>,
    primary: usize,
}

impl Default for SelectionSet {
    fn default() -> Self {
        Self::single(Selection::default())
    }
}

impl SelectionSet {
    /// A set holding one selection.
    pub fn single(selection: Selection) -> Self {
        Self {
            selections: vec![selection],
            primary: 0,
        }
    }

    /// A set holding one caret at `pos`.
    pub fn caret(pos: Position) -> Self {
        Self::single(Selection::caret(pos))
    }

    /// Builds a set from `selections`, marking index `primary` primary and
    /// merging overlaps. Falls back to a caret at the origin if empty.
    pub fn from_vec(selections: Vec<Selection>, primary: usize) -> Self {
        if selections.is_empty() {
            return Self::default();
        }
        let primary = primary.min(selections.len() - 1);
        let mut set = Self {
            selections,
            primary,
        };
        set.normalize();
        set
    }

    /// All selections, ordered by position.
    pub fn iter(&self) -> std::slice::Iter<'_, Selection> {
        self.selections.iter()
    }

    /// All selections as a slice.
    pub fn as_slice(&self) -> &[Selection] {
        &self.selections
    }

    /// Number of cursors.
    pub fn len(&self) -> usize {
        self.selections.len()
    }

    /// Always false — a set always holds at least one cursor.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The primary cursor.
    pub fn primary(&self) -> &Selection {
        &self.selections[self.primary]
    }

    /// Index of the primary cursor.
    pub fn primary_index(&self) -> usize {
        self.primary
    }

    /// Mutable access to the primary cursor.
    pub fn primary_mut(&mut self) -> &mut Selection {
        &mut self.selections[self.primary]
    }

    /// Whether more than one cursor is active.
    pub fn is_multi(&self) -> bool {
        self.selections.len() > 1
    }

    /// Replaces the whole set with a single selection.
    pub fn set_single(&mut self, selection: Selection) {
        self.selections.clear();
        self.selections.push(selection);
        self.primary = 0;
    }

    /// Adds a cursor and makes it primary (`Ctrl+Alt+Down`, `Alt+Click`).
    pub fn add(&mut self, selection: Selection) {
        self.selections.push(selection);
        self.primary = self.selections.len() - 1;
        self.normalize();
    }

    /// Applies `f` to every selection, then re-normalises.
    pub fn map(&mut self, mut f: impl FnMut(&Selection) -> Selection) {
        let mapped: Vec<Selection> = self.selections.iter().map(&mut f).collect();
        self.selections = mapped;
        self.normalize();
    }

    /// Collapses back to just the primary cursor (`Escape`).
    pub fn collapse_to_primary(&mut self) {
        let primary = self.selections[self.primary];
        self.set_single(primary);
    }

    /// Sorts by start position and merges cursors that overlap.
    ///
    /// Two carets landing on the same character would otherwise both insert
    /// text there, duplicating every keystroke.
    fn normalize(&mut self) {
        let primary_marker = self.selections[self.primary];
        self.selections.sort_by_key(|s| (s.start(), s.end()));

        let mut merged: Vec<Selection> = Vec::with_capacity(self.selections.len());
        for sel in self.selections.drain(..) {
            match merged.last_mut() {
                // `intersects` already treats two carets at the same position
                // as overlapping, which is what collapses duplicates.
                Some(prev) if prev.range().intersects(&sel.range()) => {
                    let start = prev.start().min(sel.start());
                    let end = prev.end().max(sel.end());
                    // Keep the direction of whichever cursor was primary; if
                    // neither was, keep the earlier one's.
                    let reversed = if sel == primary_marker {
                        sel.is_reversed()
                    } else {
                        prev.is_reversed()
                    };
                    *prev = if reversed {
                        Selection::new(end, start)
                    } else {
                        Selection::new(start, end)
                    };
                }
                _ => merged.push(sel),
            }
        }

        self.selections = merged;
        self.primary = self
            .selections
            .iter()
            .position(|s| *s == primary_marker)
            .or_else(|| {
                // The primary was merged away; follow it into its merged cursor.
                self.selections
                    .iter()
                    .position(|s| s.range().intersects(&primary_marker.range()))
            })
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: u32, ch: u32) -> Position {
        Position::new(line, ch)
    }

    #[test]
    fn selection_direction_is_preserved() {
        let s = Selection::new(p(0, 5), p(0, 2));
        assert!(s.is_reversed());
        assert_eq!(s.start(), p(0, 2));
        assert_eq!(s.end(), p(0, 5));
    }

    #[test]
    fn extending_keeps_anchor_and_can_shrink() {
        let s = Selection::new(p(0, 0), p(0, 5)).extended_to(p(0, 2));
        assert_eq!(s.anchor, p(0, 0));
        assert_eq!(s.active, p(0, 2));
        assert!(!s.is_reversed());
    }

    #[test]
    fn set_sorts_selections() {
        let set = SelectionSet::from_vec(
            vec![
                Selection::caret(p(3, 0)),
                Selection::caret(p(1, 0)),
                Selection::caret(p(2, 0)),
            ],
            0,
        );
        let lines: Vec<u32> = set.iter().map(|s| s.active.line).collect();
        assert_eq!(lines, [1, 2, 3]);
    }

    #[test]
    fn overlapping_selections_merge() {
        let set = SelectionSet::from_vec(
            vec![
                Selection::new(p(0, 0), p(0, 5)),
                Selection::new(p(0, 3), p(0, 8)),
                Selection::caret(p(2, 0)),
            ],
            0,
        );
        assert_eq!(set.len(), 2);
        assert_eq!(set.as_slice()[0].range(), Range::new(p(0, 0), p(0, 8)));
    }

    #[test]
    fn duplicate_carets_collapse_to_one() {
        let set = SelectionSet::from_vec(
            vec![Selection::caret(p(1, 4)), Selection::caret(p(1, 4))],
            0,
        );
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn primary_survives_sorting() {
        let set = SelectionSet::from_vec(
            vec![Selection::caret(p(3, 0)), Selection::caret(p(1, 0))],
            0, // the line-3 caret
        );
        assert_eq!(set.primary().active, p(3, 0));
        assert_eq!(set.primary_index(), 1);
    }

    #[test]
    fn primary_follows_the_cursor_it_merged_into() {
        let set = SelectionSet::from_vec(
            vec![
                Selection::new(p(0, 0), p(0, 5)),
                Selection::new(p(0, 2), p(0, 3)),
            ],
            1, // the inner selection, which gets absorbed
        );
        assert_eq!(set.len(), 1);
        assert_eq!(set.primary().range(), Range::new(p(0, 0), p(0, 5)));
    }

    #[test]
    fn adding_a_cursor_makes_it_primary() {
        let mut set = SelectionSet::caret(p(0, 0));
        set.add(Selection::caret(p(5, 2)));
        assert_eq!(set.len(), 2);
        assert_eq!(set.primary().active, p(5, 2));
    }

    #[test]
    fn collapse_to_primary_drops_the_others() {
        let mut set = SelectionSet::from_vec(
            vec![
                Selection::caret(p(0, 0)),
                Selection::caret(p(1, 0)),
                Selection::caret(p(2, 0)),
            ],
            1,
        );
        set.collapse_to_primary();
        assert_eq!(set.len(), 1);
        assert_eq!(set.primary().active, p(1, 0));
    }

    #[test]
    fn empty_input_falls_back_to_a_caret_at_origin() {
        let set = SelectionSet::from_vec(vec![], 7);
        assert_eq!(set.len(), 1);
        assert_eq!(set.primary().active, Position::ZERO);
    }
}
