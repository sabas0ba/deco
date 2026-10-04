//! Document-local tracking of snippet fields while the user fills them in.
//!
//! A tab stop can occur more than once (`$1 … $1`); its occurrences are
//! selected together as multiple cursors, so typing edits all of them. A field
//! can be nested in another (`${1:call(${2:arg})}`); editing the outer field
//! over the inner one removes the inner one, as in VS Code.
//!
//! An occurrence with a transform (`${1/(.*)/${1:/upcase}/}`) is not selected
//! and not typed into. When navigation leaves its stop, it is replaced with the
//! transformed text of the stop's first other occurrence.

use deco_core::{Change, Position, Range, Selection, SelectionSet, Transaction};
use deco_lsp::snippet::Transform;

#[derive(Debug)]
pub(crate) struct ActiveSnippet {
    fields: Vec<ActiveField>,
    /// Navigation order, as positions in `fields`; the final stop is last.
    stops: Vec<Vec<usize>>,
    current: usize,
}

#[derive(Debug)]
struct ActiveField {
    range: Range,
    parent: Option<usize>,
    choices: Vec<String>,
    transform: Option<Transform>,
    /// False once an edit of an enclosing field has replaced it.
    alive: bool,
}

impl ActiveSnippet {
    /// Tracks `snippet`, whose ranges `place` converts to document positions.
    pub fn new(snippet: &deco_lsp::snippet::Snippet, place: impl Fn(Position) -> Position) -> Self {
        Self {
            fields: snippet
                .fields
                .iter()
                .map(|field| ActiveField {
                    range: Range::new(place(field.range.start), place(field.range.end)),
                    parent: field.parent,
                    choices: field.choices.clone(),
                    transform: field.transform.clone(),
                    alive: true,
                })
                .collect(),
            stops: snippet.stops.clone(),
            current: 0,
        }
    }

    /// The occurrences of the current tab stop that still exist, including
    /// those with a transform.
    fn current_occurrences(&self) -> impl Iterator<Item = usize> + '_ {
        self.stops[self.current]
            .iter()
            .copied()
            .filter(|&id| self.fields[id].alive)
    }

    /// The occurrences of the current tab stop that the user edits: those that
    /// still exist and have no transform.
    fn current_fields(&self) -> impl Iterator<Item = usize> + '_ {
        self.current_occurrences()
            .filter(|&id| self.fields[id].transform.is_none())
    }

    /// Changes that bring the current stop's transformed occurrences up to
    /// date, or `None` when there is nothing to change.
    ///
    /// `read` returns the document's text in a range. The source is the
    /// stop's first editable occurrence; every editable occurrence holds the
    /// same text, because they are edited together.
    pub fn mirror_updates(&self, read: impl Fn(Range) -> String) -> Option<Transaction> {
        let source = read(self.fields[self.current_fields().next()?].range);
        let changes: Vec<Change> = self
            .current_occurrences()
            .filter_map(|id| {
                let field = &self.fields[id];
                let shown = field.transform.as_ref()?.apply(&source);
                (read(field.range) != shown).then(|| Change::replace(field.range, shown))
            })
            .collect();
        if changes.is_empty() {
            return None;
        }
        Transaction::new(changes).ok()
    }

    /// Selections covering every occurrence of the current tab stop.
    pub fn selections(&self) -> SelectionSet {
        let selections: Vec<Selection> = self
            .current_fields()
            .map(|id| {
                let range = self.fields[id].range;
                Selection::new(range.start, range.end)
            })
            .collect();
        SelectionSet::from_vec(selections, 0)
    }

    /// The options of the current tab stop, if it is a choice.
    pub fn choices(&self) -> Option<&[String]> {
        self.current_fields()
            .map(|id| self.fields[id].choices.as_slice())
            .find(|choices| !choices.is_empty())
    }

    /// Changes that put `text` in every occurrence of the current tab stop.
    pub fn fill(&self, text: &str) -> Option<Transaction> {
        let changes = self
            .current_fields()
            .map(|id| Change::replace(self.fields[id].range, text.to_owned()))
            .collect();
        Transaction::new(changes).ok()
    }

    /// Whether the current tab stop is the final one.
    pub fn at_final_stop(&self) -> bool {
        self.current + 1 == self.stops.len()
    }

    /// Moves to the next or previous tab stop, skipping stops whose fields
    /// have all been removed. Stays in place at either end.
    pub fn step(&mut self, previous: bool) {
        let mut next = self.current;
        loop {
            next = if previous {
                match next.checked_sub(1) {
                    Some(next) => next,
                    None => return,
                }
            } else if next + 1 < self.stops.len() {
                next + 1
            } else {
                return;
            };
            let editable = |id: &usize| {
                let field = &self.fields[*id];
                field.alive && field.transform.is_none()
            };
            if self.stops[next].iter().any(editable) {
                self.current = next;
                return;
            }
        }
    }

    /// Whether every selection is inside a distinct occurrence of the current
    /// tab stop, and every occurrence holds one.
    pub fn contains(&self, selections: &SelectionSet) -> bool {
        let mut ranges: Vec<Range> = self
            .current_fields()
            .map(|id| self.fields[id].range)
            .collect();
        ranges.sort_by_key(|range| (range.start, range.end));
        ranges.len() == selections.len()
            && ranges.iter().zip(selections.iter()).all(|(field, s)| {
                let range = s.range();
                range.start >= field.start && range.end <= field.end
            })
    }

    /// Updates the fields for `transaction`, or returns false when it cannot
    /// be tracked, which ends navigation.
    ///
    /// Every change must lie inside an occurrence of the current tab stop,
    /// which may be one with a transform being brought up to date.
    /// Changes are applied last first, so each one's coordinates are still
    /// valid when it is applied, as when editing a file from the bottom up.
    pub fn apply(&mut self, transaction: &Transaction) -> bool {
        if transaction.is_empty() {
            return true;
        }
        let mut used = Vec::new();
        for change in transaction.changes().iter().rev() {
            if has_other_line_break(&change.text) {
                return false;
            }
            // The last occurrence containing the change, so that two carets
            // in adjacent empty occurrences are matched one to each.
            let Some(edited) = self
                .current_occurrences()
                .filter(|id| !used.contains(id))
                .filter(|&id| {
                    let range = self.fields[id].range;
                    change.range.start >= range.start && change.range.end <= range.end
                })
                .last()
            else {
                return false;
            };
            used.push(edited);
            self.apply_one(change, edited);
        }
        true
    }

    /// Applies one change inside field `edited`.
    fn apply_one(&mut self, change: &Change, edited: usize) {
        let inserted_end = end_of_insertion(change);
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
        for id in 0..self.fields.len() {
            if !self.fields[id].alive {
                continue;
            }
            let range = self.fields[id].range;
            if id == edited || self.is_ancestor(id, edited) {
                // The field grows or shrinks with the text typed into it.
                self.fields[id].range.end = shift(range.end);
            } else if self.is_ancestor(edited, id) {
                // A field inside the edited one survives only an edit that
                // does not touch it.
                if range.start <= change.range.end && range.end >= change.range.start {
                    self.fields[id].alive = false;
                } else if range.start >= change.range.end {
                    self.fields[id].range = Range::new(shift(range.start), shift(range.end));
                }
            } else if id > edited {
                // Fields are in document order, so an unrelated field after
                // the edited one is entirely after the change.
                self.fields[id].range = Range::new(shift(range.start), shift(range.end));
            }
        }
    }

    /// Whether field `outer` encloses field `inner`.
    fn is_ancestor(&self, outer: usize, inner: usize) -> bool {
        let mut at = self.fields[inner].parent;
        while let Some(parent) = at {
            if parent == outer {
                return true;
            }
            at = self.fields[parent].parent;
        }
        false
    }
}

/// Where the text a change inserts ends.
fn end_of_insertion(change: &Change) -> Position {
    let mut end = change.range.start;
    for c in change.text.chars() {
        if c == '\n' {
            end.line += 1;
            end.character = 0;
        } else {
            end.character += c.len_utf16() as u32;
        }
    }
    end
}

/// Whether `text` contains a line break other than `\n`, which the positions
/// here do not count.
fn has_other_line_break(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c,
            '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_lsp::snippet::Snippet;

    fn active(source: &str) -> ActiveSnippet {
        ActiveSnippet::new(&Snippet::parse(source).unwrap(), |p| p)
    }

    fn ranges(snippet: &ActiveSnippet) -> Vec<Range> {
        snippet
            .current_fields()
            .map(|id| snippet.fields[id].range)
            .collect()
    }

    fn at(character: u32) -> Position {
        Position::new(0, character)
    }

    #[test]
    fn typing_into_linked_occurrences_keeps_them_all_tracked() {
        // "name = name" with both occurrences of 1 selected, replaced by "x".
        let mut snippet = active("${1:name} = $1$0");
        let transaction = Transaction::new(vec![
            Change::replace(Range::new(at(0), at(4)), "x".to_owned()),
            Change::replace(Range::new(at(7), at(11)), "x".to_owned()),
        ])
        .unwrap();
        assert!(snippet.apply(&transaction));
        assert_eq!(
            ranges(&snippet),
            vec![Range::new(at(0), at(1)), Range::new(at(4), at(5))]
        );
        snippet.step(false);
        assert_eq!(ranges(&snippet), vec![Range::empty(at(5))]);
    }

    #[test]
    fn replacing_a_parent_removes_the_field_inside_it() {
        let mut snippet = active("${1:call(${2:arg})} ${3:x}");
        let transaction =
            Transaction::single(Change::replace(Range::new(at(0), at(9)), "go".to_owned()));
        assert!(snippet.apply(&transaction));
        assert_eq!(ranges(&snippet), vec![Range::new(at(0), at(2))]);
        // Stop 2 is gone, so the next stop is 3, moved left by the edit.
        snippet.step(false);
        assert_eq!(ranges(&snippet), vec![Range::new(at(3), at(4))]);
    }

    #[test]
    fn editing_a_nested_field_grows_its_parent() {
        let mut snippet = active("${1:call(${2:arg})} end");
        snippet.step(false);
        let transaction = Transaction::single(Change::replace(
            Range::new(at(5), at(8)),
            "value".to_owned(),
        ));
        assert!(snippet.apply(&transaction));
        assert_eq!(ranges(&snippet), vec![Range::new(at(5), at(10))]);
        assert_eq!(snippet.fields[0].range, Range::new(at(0), at(11)));
        // The final stop, after " end", moved with the text.
        assert_eq!(snippet.fields[2].range, Range::empty(at(15)));
    }

    #[test]
    fn empty_neighbours_keep_their_side_of_the_edit() {
        // `${2}${1}`: typing into 1 must not move 2, which comes before it,
        // and must move the final stop, which comes after it.
        let mut snippet = active("${2}${1}$0");
        let transaction = Transaction::single(Change::insert(at(0), "ab".to_owned()));
        assert!(snippet.apply(&transaction));
        assert_eq!(ranges(&snippet), vec![Range::new(at(0), at(2))]);
        snippet.step(false);
        assert_eq!(ranges(&snippet), vec![Range::empty(at(0))]);
        snippet.step(false);
        assert_eq!(ranges(&snippet), vec![Range::empty(at(2))]);
    }

    #[test]
    fn a_transformed_occurrence_is_not_selected_and_follows_on_request() {
        let mut snippet = active("${1:ab} ${1/(.*)/${1:/upcase}/}$0");
        assert_eq!(
            snippet.selections().len(),
            1,
            "only the editable occurrence"
        );
        // The user typed `xyz` over `ab`.
        let typed =
            Transaction::single(Change::replace(Range::new(at(0), at(2)), "xyz".to_owned()));
        assert!(snippet.apply(&typed));
        let text = "xyz AB";
        let read = |range: Range| {
            text.chars()
                .skip(range.start.character as usize)
                .take((range.end.character - range.start.character) as usize)
                .collect::<String>()
        };
        let update = snippet
            .mirror_updates(read)
            .expect("the copy is out of date");
        assert_eq!(update.changes().len(), 1);
        assert_eq!(update.changes()[0].range, Range::new(at(4), at(6)));
        assert_eq!(update.changes()[0].text, "XYZ");
        assert!(snippet.apply(&update));
        snippet.step(false);
        assert_eq!(ranges(&snippet), vec![Range::empty(at(7))]);
    }

    #[test]
    fn an_edit_outside_the_current_stop_is_not_tracked() {
        let mut snippet = active("a ${1:b} c");
        let outside = Transaction::single(Change::insert(at(0), "x".to_owned()));
        assert!(!snippet.apply(&outside));
    }

    #[test]
    fn selections_cover_every_occurrence() {
        let snippet = active("${1:a}-$1");
        let selections = snippet.selections();
        assert_eq!(selections.len(), 2);
        assert!(snippet.contains(&selections));
        assert!(!snippet.contains(&SelectionSet::caret(at(0))));
    }
}
