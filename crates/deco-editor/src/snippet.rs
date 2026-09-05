//! Document-local tracking for flat, independently editable snippet fields.

use deco_core::{Position, Range, SelectionSet, Transaction};

#[derive(Debug)]
pub(crate) struct ActiveSnippet {
    pub stops: Vec<Range>,
    pub current: usize,
}

impl ActiveSnippet {
    pub fn contains(&self, selections: &SelectionSet) -> bool {
        let active = self.stops[self.current];
        selections.len() == 1
            && selections.iter().all(|s| {
                let range = s.range();
                range.start >= active.start && range.end <= active.end
            })
    }

    /// Track only a single edit confined to the current field. External edits
    /// and multi-cursor operations cancel navigation rather than retaining stale
    /// coordinates. Processing the transaction here also covers paste and pairs.
    pub fn apply(&mut self, transaction: &Transaction) -> bool {
        if transaction.is_empty() {
            return true;
        }
        let [change] = transaction.changes() else {
            return false;
        };
        if change.text.chars().any(|c| {
            matches!(
                c,
                '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        }) {
            return false;
        }
        let active = self.stops[self.current];
        if change.range.start < active.start || change.range.end > active.end {
            return false;
        }
        let mut inserted_end = change.range.start;
        for c in change.text.chars() {
            if c == '\n' {
                inserted_end.line += 1;
                inserted_end.character = 0;
            } else {
                inserted_end.character += c.len_utf16() as u32;
            }
        }
        let shift = |position: Position| {
            if position.line == change.range.end.line {
                Position::new(
                    inserted_end.line,
                    inserted_end.character + position.character - change.range.end.character,
                )
            } else {
                Position::new(
                    inserted_end.line + position.line - change.range.end.line,
                    position.character,
                )
            }
        };
        for (index, stop) in self.stops.iter_mut().enumerate() {
            if index == self.current {
                stop.end = shift(stop.end);
            } else if stop.start >= active.end {
                stop.start = shift(stop.start);
                stop.end = shift(stop.end);
            }
        }
        true
    }
}
