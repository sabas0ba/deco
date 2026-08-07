//! The completion list, as state.
//!
//! Pure: it holds the items a server returned and the prefix typed since, and
//! answers what should be shown and what should be inserted. Nothing here reads
//! the terminal or touches the document, which is what lets the filtering and
//! selection rules — the parts that are actually easy to get wrong — be tested
//! directly.
//!
//! # Filtering happens here, not on the server
//!
//! A server is asked once, at the position where the list opened, and returns
//! everything plausible there. As the user keeps typing, the list narrows
//! locally. That is how VS Code behaves and it is the only way the list can feel
//! immediate — a round trip per keystroke would make every character wait on a
//! process.
//!
//! The consequence is that the widget has to know where the list opened, so it
//! can tell which of the characters on screen are the prefix being matched. Get
//! that wrong and the list filters against the wrong text, which looks like the
//! server returning nonsense.

use deco_core::position::Position;
use deco_lsp::requests::{CompletionItem, CompletionKind};

/// An item as it should appear, with the reason it matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown<'a> {
    /// The item.
    pub item: &'a CompletionItem,
    /// How well it matched, for ordering. Lower sorts first.
    rank: Rank,
}

/// How an item matched the typed prefix.
///
/// Ordered so that the better kind of match sorts first. A prefix match is what
/// the user almost always means; a fuzzy match is a guess worth offering but not
/// worth putting at the top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// The prefix matched exactly, including case.
    Prefix,
    /// The prefix matched ignoring case.
    PrefixInsensitive,
    /// Every character of the prefix appears in order, but not contiguously.
    Subsequence,
}

/// The completion list on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggest {
    items: Vec<CompletionItem>,
    /// Where the list was opened, which is where the matched prefix begins.
    anchor: Position,
    /// What has been typed since, matched against each item's filter text.
    prefix: String,
    /// Which visible row is selected.
    selected: usize,
    /// Whether the server said its list was partial.
    incomplete: bool,
}

/// How many rows the list will show at once.
///
/// Enough to be useful, few enough to leave the code visible — the point of a
/// completion list is to choose between candidates in context.
pub const MAX_ROWS: usize = 8;

impl Suggest {
    /// Opens a list at `anchor` with the items a server returned.
    ///
    /// The selected row starts on the server's `preselect` if it marked one, and
    /// on the first item otherwise. Honouring `preselect` matters: a server that
    /// knows the likely answer puts it there, and ignoring it means the user
    /// arrows past what they wanted.
    pub fn new(items: Vec<CompletionItem>, anchor: Position, incomplete: bool) -> Self {
        let mut suggest = Self {
            items,
            anchor,
            prefix: String::new(),
            selected: 0,
            incomplete,
        };
        suggest.selected = suggest
            .visible()
            .iter()
            .position(|shown| shown.item.preselect)
            .unwrap_or(0);
        suggest
    }

    /// Where the list was opened.
    pub fn anchor(&self) -> Position {
        self.anchor
    }

    /// The prefix being matched.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Whether the server's list was partial.
    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    /// How many items matched, before the row limit.
    pub fn matches(&self) -> usize {
        self.visible().len()
    }

    /// Whether there is nothing left to show.
    ///
    /// The caller should close the list: an empty box is worse than none, and it
    /// is how the widget says the user has typed past every candidate.
    pub fn is_empty(&self) -> bool {
        self.visible().is_empty()
    }

    /// Extends the prefix as the user types, keeping the selection where it can.
    ///
    /// Returns whether the list still has anything to show.
    pub fn push(&mut self, c: char) -> bool {
        let previous = self.selected_item().map(|item| item.label.clone());
        self.prefix.push(c);
        self.restore_selection(previous);
        !self.is_empty()
    }

    /// Shortens the prefix on backspace.
    ///
    /// Returns `false` once the prefix is empty and there is nothing left to
    /// delete, which means the user has backspaced out of the word the list was
    /// opened for.
    pub fn pop(&mut self) -> bool {
        let previous = self.selected_item().map(|item| item.label.clone());
        if self.prefix.pop().is_none() {
            return false;
        }
        self.restore_selection(previous);
        true
    }

    /// Keeps the same item selected across a filter change when it survived.
    ///
    /// Without this, narrowing the list resets the selection to the top, and an
    /// item the user had arrowed to jumps away under them.
    fn restore_selection(&mut self, previous: Option<String>) {
        let visible = self.visible();
        self.selected = previous
            .and_then(|label| visible.iter().position(|shown| shown.item.label == label))
            .unwrap_or(0);
    }

    /// Moves the selection down, wrapping.
    pub fn next(&mut self) {
        let count = self.visible().len();
        if count > 0 {
            self.selected = (self.selected + 1) % count;
        }
    }

    /// Moves the selection up, wrapping.
    pub fn previous(&mut self) {
        let count = self.visible().len();
        if count > 0 {
            self.selected = (self.selected + count - 1) % count;
        }
    }

    /// Which row is selected.
    pub fn selected_row(&self) -> usize {
        self.selected
    }

    /// The item that would be accepted.
    pub fn selected_item(&self) -> Option<&CompletionItem> {
        let visible = self.visible();
        visible.get(self.selected).map(|shown| shown.item)
    }

    /// The items to draw, best match first, capped at [`MAX_ROWS`].
    ///
    /// Recomputed rather than cached: the prefix changes on every keystroke, and
    /// a cache that has to be invalidated on each one buys nothing but a chance
    /// to forget.
    pub fn visible(&self) -> Vec<Shown<'_>> {
        let mut matched: Vec<Shown<'_>> = self
            .items
            .iter()
            .filter_map(|item| rank(&item.filter, &self.prefix).map(|rank| Shown { item, rank }))
            .collect();

        matched.sort_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                // The server's own ordering within a rank: it uses `sortText` to
                // put the likely answer first, and overriding that with
                // alphabetical order makes a good server look arbitrary.
                .then_with(|| a.item.sort_key().cmp(b.item.sort_key()))
                .then_with(|| a.item.label.cmp(&b.item.label))
        });
        matched.truncate(MAX_ROWS);
        matched
    }

    /// The rows to draw, as `(marker, label, detail)`.
    pub fn rows(&self) -> Vec<(char, &str, Option<&str>)> {
        self.visible()
            .into_iter()
            .map(|shown| {
                (
                    shown.item.kind.marker(),
                    shown.item.label.as_str(),
                    shown.item.detail.as_deref(),
                )
            })
            .collect()
    }
}

/// How well `filter` matches `prefix`, or `None` if it does not.
///
/// An empty prefix matches everything, which is what makes a list opened by
/// `ctrl+space` show the whole set.
fn rank(filter: &str, prefix: &str) -> Option<Rank> {
    if prefix.is_empty() {
        return Some(Rank::Prefix);
    }
    if filter.starts_with(prefix) {
        return Some(Rank::Prefix);
    }
    // Case-insensitively next, because typing `hash` should find `HashMap` —
    // and comparing lowercase forms rather than ASCII-lowering both, so that
    // a non-ASCII identifier is matched by the same rule as an ASCII one.
    if filter.to_lowercase().starts_with(&prefix.to_lowercase()) {
        return Some(Rank::PrefixInsensitive);
    }
    if is_subsequence(&filter.to_lowercase(), &prefix.to_lowercase()) {
        return Some(Rank::Subsequence);
    }
    None
}

/// Whether every character of `needle` appears in `haystack`, in order.
fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.any(|available| available == wanted))
}

/// The marker column's width, including its trailing space.
pub const MARKER_WIDTH: usize = 2;

/// A hint at what an item is, for callers that want it without the whole item.
pub fn marker_of(kind: CompletionKind) -> char {
    kind.marker()
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_lsp::requests::CompletionKind;

    fn item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_owned(),
            kind: CompletionKind::Other,
            detail: None,
            insert: label.to_owned(),
            replace: None,
            filter: label.to_owned(),
            sort: None,
            preselect: false,
            was_snippet: false,
        }
    }

    fn suggest(labels: &[&str]) -> Suggest {
        Suggest::new(
            labels.iter().map(|label| item(label)).collect(),
            Position::new(0, 0),
            false,
        )
    }

    fn labels(suggest: &Suggest) -> Vec<String> {
        suggest
            .visible()
            .iter()
            .map(|shown| shown.item.label.clone())
            .collect()
    }

    #[test]
    fn an_empty_prefix_shows_everything() {
        // What a list opened by ctrl+space has to do.
        let s = suggest(&["push", "pop", "len"]);
        assert_eq!(s.matches(), 3);
        assert!(!s.is_empty());
    }

    #[test]
    fn typing_narrows_the_list() {
        let mut s = suggest(&["push", "pop", "len"]);
        assert!(s.push('p'));
        assert_eq!(labels(&s), vec!["pop", "push"]);
        assert!(s.push('u'));
        assert_eq!(labels(&s), vec!["push"]);
    }

    #[test]
    fn typing_past_every_candidate_empties_the_list() {
        // The caller closes the widget on this: an empty box is worse than none.
        let mut s = suggest(&["push"]);
        for c in "pushx".chars() {
            s.push(c);
        }
        assert!(s.is_empty());
    }

    #[test]
    fn backspace_widens_the_list_again() {
        let mut s = suggest(&["push", "pop"]);
        s.push('p');
        s.push('u');
        assert_eq!(labels(&s), vec!["push"]);
        assert!(s.pop());
        assert_eq!(labels(&s), vec!["pop", "push"]);
    }

    #[test]
    fn backspacing_out_of_the_word_reports_that_it_is_done() {
        // The user has deleted back past where the list opened, so it no longer
        // describes what is being typed.
        let mut s = suggest(&["push"]);
        assert!(!s.pop(), "there was no prefix to delete");
    }

    #[test]
    fn a_case_insensitive_prefix_matches_but_ranks_below_an_exact_one() {
        // Typing `hash` should find `HashMap`, and `hasher` should still come
        // first if it is spelled the way it was typed.
        let s = {
            let mut s = suggest(&["HashMap", "hasher"]);
            for c in "has".chars() {
                s.push(c);
            }
            s
        };
        assert_eq!(labels(&s), vec!["hasher", "HashMap"]);
    }

    #[test]
    fn a_subsequence_matches_and_ranks_last() {
        // `hm` finding `HashMap` is useful; it should not outrank a real prefix.
        let mut s = suggest(&["hmm", "HashMap"]);
        s.push('h');
        s.push('m');
        assert_eq!(labels(&s), vec!["hmm", "HashMap"]);
    }

    #[test]
    fn something_that_matches_no_rule_is_excluded() {
        let mut s = suggest(&["push"]);
        s.push('z');
        assert!(s.is_empty());
    }

    #[test]
    fn filter_text_is_matched_rather_than_the_label() {
        // rust-analyzer labels an item `foo(…)` and filters on `foo`; matching
        // the label fails as soon as the user types `f`.
        let mut items = vec![item("foo(…)")];
        items[0].filter = "foo".into();
        let mut s = Suggest::new(items, Position::ZERO, false);
        assert!(s.push('f'));
        assert_eq!(labels(&s), vec!["foo(…)"]);
    }

    #[test]
    fn the_servers_sort_text_orders_within_a_rank() {
        // Servers put the likely answer first; alphabetical order would override
        // that and make a good server look arbitrary.
        let mut items = vec![item("apple"), item("zebra")];
        items[1].sort = Some("0000".into());
        let s = Suggest::new(items, Position::ZERO, false);
        assert_eq!(labels(&s), vec!["zebra", "apple"]);
    }

    #[test]
    fn preselect_decides_the_initial_selection() {
        let mut items = vec![item("a"), item("b"), item("c")];
        items[2].preselect = true;
        let s = Suggest::new(items, Position::ZERO, false);
        assert_eq!(s.selected_item().map(|i| i.label.as_str()), Some("c"));
    }

    #[test]
    fn without_preselect_the_first_item_is_selected() {
        let s = suggest(&["a", "b"]);
        assert_eq!(s.selected_row(), 0);
        assert_eq!(s.selected_item().map(|i| i.label.as_str()), Some("a"));
    }

    #[test]
    fn the_selection_wraps_in_both_directions() {
        let mut s = suggest(&["a", "b", "c"]);
        s.previous();
        assert_eq!(s.selected_row(), 2, "up from the top wraps to the bottom");
        s.next();
        assert_eq!(s.selected_row(), 0);
    }

    #[test]
    fn the_selection_follows_its_item_when_the_list_narrows() {
        // Otherwise narrowing resets to the top and the item the user arrowed to
        // jumps away under them.
        let mut s = suggest(&["pop", "push", "pull"]);
        s.push('p');
        s.next();
        let chosen = s.selected_item().unwrap().label.clone();
        assert_eq!(chosen, "pull", "sorted: pop, pull, push");

        s.push('u');
        assert_eq!(
            s.selected_item().map(|i| i.label.as_str()),
            Some("pull"),
            "the same item stays selected"
        );
    }

    #[test]
    fn the_selection_falls_back_to_the_top_when_its_item_is_filtered_out() {
        let mut s = suggest(&["pop", "push"]);
        s.next();
        assert_eq!(s.selected_item().map(|i| i.label.as_str()), Some("push"));
        s.push('o');
        assert_eq!(s.selected_item().map(|i| i.label.as_str()), Some("pop"));
        assert_eq!(s.selected_row(), 0);
    }

    #[test]
    fn the_list_is_capped_so_the_code_stays_visible() {
        let many: Vec<&str> = ["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "a10"]
            .into_iter()
            .collect();
        let s = suggest(&many);
        assert_eq!(s.visible().len(), MAX_ROWS);
        assert_eq!(s.rows().len(), MAX_ROWS);
    }

    #[test]
    fn selection_on_an_empty_list_is_harmless() {
        let mut s = suggest(&[]);
        s.next();
        s.previous();
        assert!(s.selected_item().is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn rows_carry_the_marker_and_the_detail() {
        let mut items = vec![item("push")];
        items[0].kind = CompletionKind::Function;
        items[0].detail = Some("fn(&mut self, T)".into());
        let s = Suggest::new(items, Position::ZERO, false);

        let rows = s.rows();
        assert_eq!(rows[0].0, 'f');
        assert_eq!(rows[0].1, "push");
        assert_eq!(rows[0].2, Some("fn(&mut self, T)"));
        assert_eq!(marker_of(CompletionKind::Function), 'f');
    }

    #[test]
    fn the_anchor_and_incomplete_flag_are_carried() {
        let s = Suggest::new(vec![item("a")], Position::new(3, 7), true);
        assert_eq!(s.anchor(), Position::new(3, 7));
        assert!(s.is_incomplete());
        assert_eq!(s.prefix(), "");
    }

    #[test]
    fn a_non_ascii_prefix_matches_by_the_same_rule_as_ascii() {
        let mut items = vec![item("Ünicode"), item("unrelated")];
        items[0].filter = "Ünicode".into();
        let mut s = Suggest::new(items, Position::ZERO, false);
        assert!(s.push('ü'), "case-insensitive on a non-ASCII letter");
        assert_eq!(labels(&s), vec!["Ünicode"]);
    }
}
