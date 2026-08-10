//! The quick-open prompt: go to line, and the command palette.
//!
//! One type for both, because they are the same interaction — a line of text at
//! the bottom of the screen, optionally over a filtered list — and the only thing
//! that differs is what accepting it means.
//!
//! State only, as with [`crate::find`]: what was typed and which choice is
//! selected are the same in a terminal and in a window.
//!
//! # Why the palette is assembled from two lists
//!
//! A palette that offers a command the editor cannot run is worse than one that
//! is short. But whether a command works depends partly on the frontend: the
//! terminal frontend can format a document because it has a language server
//! client, and the GPU frontend cannot because it has neither. So the core
//! contributes the commands it implements itself, and the frontend adds the ones
//! it implements — [`Session::frontend_commands`](crate::Session::frontend_commands).
//! Nothing is listed on the assumption that somebody downstream will handle it.

use crate::commands::PaletteEntry;
use crate::input::Input;

/// What a prompt is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// `ctrl+g`: a line number to jump to.
    GoToLine,
    /// `ctrl+shift+p`: a command to run.
    Commands,
}

impl PromptKind {
    /// The label drawn at the left of the prompt.
    pub fn label(self) -> &'static str {
        match self {
            Self::GoToLine => "Go to line:",
            Self::Commands => "Command:",
        }
    }
}

/// An open prompt.
#[derive(Debug, Clone)]
pub struct Prompt {
    kind: PromptKind,
    input: Input,
    /// Every choice, unfiltered. Empty for a prompt that is only a text field.
    choices: Vec<PaletteEntry>,
    /// Indices into `choices` that match what has been typed, best first.
    matching: Vec<usize>,
    /// Index into `matching`.
    selected: usize,
}

/// How many choices are offered at once.
///
/// The list costs rows of the file, and a prompt that covered half the screen
/// would hide the thing being navigated. Eight is the same limit the completion
/// list uses.
pub const MAX_ROWS: usize = 8;

impl Prompt {
    /// A prompt with no list: just a line of text to type into.
    pub fn plain(kind: PromptKind) -> Self {
        Self {
            kind,
            input: Input::new(),
            choices: Vec::new(),
            matching: Vec::new(),
            selected: 0,
        }
    }

    /// A prompt over a list of choices, all of them matching to begin with.
    pub fn list(kind: PromptKind, choices: Vec<PaletteEntry>) -> Self {
        let mut prompt = Self {
            kind,
            input: Input::new(),
            choices,
            matching: Vec::new(),
            selected: 0,
        };
        prompt.refilter();
        prompt
    }

    /// What kind of prompt this is.
    pub fn kind(&self) -> PromptKind {
        self.kind
    }

    /// What has been typed.
    pub fn text(&self) -> &str {
        self.input.text()
    }

    /// Caret offset within the typed text, in characters.
    pub fn caret(&self) -> usize {
        self.input.caret()
    }

    /// Whether this prompt offers a list at all.
    pub fn has_list(&self) -> bool {
        !self.choices.is_empty()
    }

    /// How many choices match what has been typed.
    pub fn matches(&self) -> usize {
        self.matching.len()
    }

    /// The choices to draw, best first, capped at [`MAX_ROWS`].
    ///
    /// The window scrolls with the selection, so a choice below the cap can still
    /// be reached with the arrow keys.
    pub fn visible(&self) -> Vec<&PaletteEntry> {
        let first = self.scroll_top();
        self.matching
            .iter()
            .skip(first)
            .take(MAX_ROWS)
            .map(|index| &self.choices[*index])
            .collect()
    }

    /// Which visible row is selected.
    pub fn selected_row(&self) -> usize {
        self.selected - self.scroll_top()
    }

    /// The selected choice, if there is one.
    pub fn selected(&self) -> Option<&PaletteEntry> {
        self.matching
            .get(self.selected)
            .map(|index| &self.choices[*index])
    }

    /// First visible index, chosen so the selection is always on screen.
    fn scroll_top(&self) -> usize {
        // Keep the selection in the window by scrolling only as much as needed,
        // which is what makes holding `down` feel like a list rather than pages.
        self.selected.saturating_sub(MAX_ROWS - 1)
    }

    /// Moves the selection down, wrapping.
    pub fn next(&mut self) {
        if self.matching.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.matching.len();
    }

    /// Moves the selection up, wrapping.
    pub fn previous(&mut self) {
        if self.matching.is_empty() {
            return;
        }
        self.selected = match self.selected {
            0 => self.matching.len() - 1,
            current => current - 1,
        };
    }

    /// Applies a command to the text field, if it owns it.
    ///
    /// Re-filters afterwards, so the list always describes what is typed.
    pub fn consume(
        &mut self,
        command: &str,
        args: Option<&serde_json::Value>,
        clipboard: &mut dyn crate::commands::Clipboard,
    ) -> bool {
        let before = self.input.text().to_owned();
        let consumed = self.input.consume(command, args, clipboard);
        if consumed && self.input.text() != before {
            self.refilter();
        }
        consumed
    }

    /// Recomputes which choices match, keeping the selection on the same choice
    /// where it survives the narrowing.
    ///
    /// Following the choice rather than the row index is what stops a keystroke
    /// from silently moving the selection onto something else — the thing that
    /// makes a palette dangerous, since the next key runs it.
    fn refilter(&mut self) {
        let previous = self.selected().map(|entry| entry.id.clone());
        let query = self.input.text();

        let mut scored: Vec<(Rank, usize)> = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| rank(entry, query).map(|rank| (rank, index)))
            .collect();
        // By rank, then by title, so the order is stable rather than dependent on
        // however the registry happened to be written.
        scored.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| self.choices[a.1].title.cmp(&self.choices[b.1].title))
        });

        self.matching = scored.into_iter().map(|(_, index)| index).collect();
        self.selected = previous
            .and_then(|id| {
                self.matching
                    .iter()
                    .position(|index| self.choices[*index].id == id)
            })
            .unwrap_or(0);
    }
}

/// How well a choice matched, best first.
///
/// Deliberately not shared with the completion list's ranking. That one matches
/// identifiers typed one character at a time; this one matches phrases with
/// spaces in them, where "what a word in the title starts with" is the useful
/// middle ground and has no equivalent for a single identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// The title starts with what was typed.
    TitlePrefix,
    /// A word of the title starts with it, so `line` finds `Go to Line`.
    WordPrefix,
    /// The title contains it.
    TitleContains,
    /// The identifier contains it, so `commentLine` finds the command by its
    /// VS Code name even when the title reads differently.
    IdContains,
    /// The title's characters appear in order, so `gtl` finds `Go to Line`.
    Subsequence,
}

/// Ranks `entry` against `query`, or `None` if it does not match at all.
fn rank(entry: &PaletteEntry, query: &str) -> Option<Rank> {
    if query.is_empty() {
        return Some(Rank::TitlePrefix);
    }
    let query = query.to_lowercase();
    let title = entry.title.to_lowercase();
    let id = entry.id.to_lowercase();

    if title.starts_with(&query) {
        return Some(Rank::TitlePrefix);
    }
    if title
        .split_whitespace()
        .any(|word| word.starts_with(&query))
    {
        return Some(Rank::WordPrefix);
    }
    if title.contains(&query) {
        return Some(Rank::TitleContains);
    }
    if id.contains(&query) {
        return Some(Rank::IdContains);
    }
    // Spaces are dropped from the haystack so `gtl` matches `Go to Line`, and
    // from the needle so a typed space does not have to line up with one.
    let flat: String = title.chars().filter(|c| !c.is_whitespace()).collect();
    is_subsequence(&flat, &query.replace(' ', "")).then_some(Rank::Subsequence)
}

/// Whether every character of `needle` appears in `haystack`, in order.
fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|wanted| chars.any(|c| c == wanted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::MemoryClipboard;

    fn entry(id: &str, title: &str) -> PaletteEntry {
        PaletteEntry {
            id: id.to_owned(),
            title: title.to_owned(),
        }
    }

    fn palette() -> Prompt {
        Prompt::list(
            PromptKind::Commands,
            vec![
                entry("editor.action.commentLine", "Toggle Line Comment"),
                entry("editor.action.selectAll", "Select All"),
                entry("workbench.action.gotoLine", "Go to Line"),
                entry("undo", "Undo"),
            ],
        )
    }

    fn typed(prompt: &mut Prompt, text: &str) {
        prompt.consume(
            "type",
            Some(&serde_json::json!({ "text": text })),
            &mut MemoryClipboard::default(),
        );
    }

    #[test]
    fn a_text_prompt_has_no_list() {
        let prompt = Prompt::plain(PromptKind::GoToLine);
        assert!(!prompt.has_list());
        assert_eq!(prompt.matches(), 0);
        assert!(prompt.selected().is_none());
    }

    #[test]
    fn an_empty_query_offers_everything() {
        let prompt = palette();
        assert_eq!(prompt.matches(), 4);
    }

    #[test]
    fn typing_narrows_the_list() {
        let mut prompt = palette();
        typed(&mut prompt, "comment");
        assert_eq!(prompt.matches(), 1);
        assert_eq!(prompt.selected().unwrap().title, "Toggle Line Comment");
    }

    #[test]
    fn a_word_in_the_middle_of_a_title_is_found() {
        // The reason `WordPrefix` exists: nobody types the first word of
        // "Toggle Line Comment" to find it.
        let mut prompt = palette();
        typed(&mut prompt, "line");
        let titles: Vec<&str> = prompt
            .visible()
            .iter()
            .map(|entry| entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Go to Line"), "{titles:?}");
        assert!(titles.contains(&"Toggle Line Comment"), "{titles:?}");
    }

    #[test]
    fn a_title_prefix_outranks_a_word_prefix() {
        let mut prompt = palette();
        typed(&mut prompt, "go");
        assert_eq!(prompt.selected().unwrap().title, "Go to Line");
    }

    #[test]
    fn a_command_can_be_found_by_its_vscode_identifier() {
        let mut prompt = palette();
        typed(&mut prompt, "selectAll");
        assert_eq!(prompt.selected().unwrap().id, "editor.action.selectAll");
    }

    #[test]
    fn initials_find_a_multi_word_title() {
        let mut prompt = palette();
        typed(&mut prompt, "gtl");
        assert_eq!(prompt.selected().unwrap().title, "Go to Line");
    }

    #[test]
    fn a_query_matching_nothing_leaves_no_selection() {
        let mut prompt = palette();
        typed(&mut prompt, "zzzzz");
        assert_eq!(prompt.matches(), 0);
        assert!(prompt.selected().is_none());
        assert!(prompt.visible().is_empty());
    }

    #[test]
    fn the_selection_follows_the_same_choice_as_the_list_narrows() {
        // The important one. If the selection stayed on row 0, a keystroke would
        // silently move it onto a different command — and the next key runs it.
        let mut prompt = palette();
        typed(&mut prompt, "line");
        prompt.next();
        let chosen = prompt.selected().unwrap().id.clone();
        typed(&mut prompt, "n");
        assert_eq!(
            prompt.selected().map(|entry| entry.id.clone()),
            Some(chosen),
            "the selection should have followed the command, not the row"
        );
    }

    #[test]
    fn a_selection_that_no_longer_matches_falls_back_to_the_best_choice() {
        let mut prompt = palette();
        typed(&mut prompt, "undo");
        assert_eq!(prompt.selected().unwrap().id, "undo");
        typed(&mut prompt, "zzz");
        assert!(prompt.selected().is_none());
    }

    #[test]
    fn the_selection_wraps_in_both_directions() {
        let mut prompt = palette();
        assert_eq!(prompt.selected_row(), 0);
        prompt.previous();
        assert_eq!(prompt.selected_row(), 3, "up from the top goes to the end");
        prompt.next();
        assert_eq!(prompt.selected_row(), 0);
    }

    #[test]
    fn moving_the_selection_on_an_empty_list_does_nothing() {
        let mut prompt = palette();
        typed(&mut prompt, "zzzzz");
        prompt.next();
        prompt.previous();
        assert!(prompt.selected().is_none());
    }

    #[test]
    fn the_list_scrolls_to_keep_the_selection_visible() {
        let choices: Vec<PaletteEntry> = (0..20)
            .map(|n| entry(&format!("cmd.{n}"), &format!("Command {n:02}")))
            .collect();
        let mut prompt = Prompt::list(PromptKind::Commands, choices);
        assert_eq!(prompt.visible().len(), MAX_ROWS);
        // Down past the window's edge scrolls by one rather than by a page.
        for _ in 0..MAX_ROWS {
            prompt.next();
        }
        assert_eq!(prompt.selected_row(), MAX_ROWS - 1);
        assert_eq!(prompt.visible().len(), MAX_ROWS);
        assert_eq!(prompt.visible()[MAX_ROWS - 1].title, "Command 08");
    }

    #[test]
    fn backspace_widens_the_list_again() {
        let mut prompt = palette();
        typed(&mut prompt, "comment");
        assert_eq!(prompt.matches(), 1);
        for _ in 0..7 {
            prompt.consume("deleteLeft", None, &mut MemoryClipboard::default());
        }
        assert_eq!(prompt.matches(), 4);
    }

    #[test]
    fn a_command_the_field_does_not_own_is_left_alone() {
        let mut prompt = palette();
        for command in ["cursorUp", "workbench.action.acceptSelectedQuickOpenItem"] {
            assert!(
                !prompt.consume(command, None, &mut MemoryClipboard::default()),
                "{command}"
            );
        }
    }

    #[test]
    fn the_labels_say_what_is_being_asked_for() {
        assert_eq!(PromptKind::GoToLine.label(), "Go to line:");
        assert_eq!(PromptKind::Commands.label(), "Command:");
    }

    #[test]
    fn subsequence_matching_requires_order() {
        assert!(is_subsequence("gotoline", "gtl"));
        assert!(!is_subsequence("gotoline", "ltg"));
        assert!(is_subsequence("anything", ""));
        assert!(!is_subsequence("", "a"));
    }
}
