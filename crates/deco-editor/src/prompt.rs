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
    /// `ctrl+p`: a file in the workspace to open.
    Files,
    /// `ctrl+shift+f`: a place in the workspace where a term was found.
    SearchResults,
    /// `ctrl+shift+o`: a name the language server found in this document.
    Symbols,
    /// `ctrl+k m`: which language this document is.
    Languages,
    /// `ctrl+k ctrl+t`: which colour theme to use.
    Themes,
    /// `ctrl+shift+s`: where to write this document.
    SaveAs,
    /// `ctrl+o`: which file to open.
    OpenPath,
}

impl PromptKind {
    /// The label drawn at the left of the prompt.
    pub fn label(self) -> &'static str {
        match self {
            Self::GoToLine => "Go to line:",
            Self::Commands => "Command:",
            Self::Files => "Open:",
            Self::SearchResults => "Result:",
            Self::Symbols => "Go to symbol:",
            Self::Languages => "Select language mode:",
            Self::Themes => "Color theme:",
            Self::SaveAs => "Save as:",
            Self::OpenPath => "Open file:",
        }
    }

    /// What the choices are called, for the `3 commands` readout.
    ///
    /// Empty for a prompt with no list, which has nothing to count. Singular for
    /// one, because `1 matches` is the kind of detail that makes a careful reader
    /// distrust everything else on the screen.
    pub fn noun(self, count: usize) -> &'static str {
        match (self, count) {
            (Self::GoToLine, _) => "",
            // Nothing to count: a path is typed, not chosen from a list.
            (Self::SaveAs, _) | (Self::OpenPath, _) => "",
            (Self::Commands, 1) => "command",
            (Self::Commands, _) => "commands",
            (Self::Files, 1) => "file",
            (Self::Files, _) => "files",
            (Self::SearchResults, 1) => "match",
            (Self::SearchResults, _) => "matches",
            (Self::Symbols, 1) => "symbol",
            (Self::Symbols, _) => "symbols",
            (Self::Languages, 1) => "language",
            (Self::Languages, _) => "languages",
            (Self::Themes, 1) => "theme",
            (Self::Themes, _) => "themes",
        }
    }

    /// What to call this prompt in a sentence.
    ///
    /// For a frontend that has to refuse it: "the command palette is only in the
    /// terminal frontend" is the wrong thing to say about a save-as prompt, and a
    /// message that names the wrong widget is worse than a vague one.
    pub fn describe(self) -> &'static str {
        match self {
            Self::GoToLine => "go to line",
            Self::Commands => "the command palette",
            Self::Files => "quick open",
            Self::SearchResults => "the results list",
            Self::Symbols => "the symbol list",
            Self::Languages => "the language picker",
            Self::Themes => "the theme picker",
            Self::SaveAs => "save as",
            Self::OpenPath => "open file",
        }
    }

    /// Whether the order the choices arrived in carries meaning.
    ///
    /// True for symbols, which arrive in document order — the order the file
    /// reads in, and the order VS Code's own picker shows. False for a command,
    /// whose order is however the registry happened to be written, and for a file
    /// or a search result, whose title is a path and so sorts to the same place
    /// either way. Where it is false, equal matches are ordered by title, so the
    /// list is stable rather than incidental.
    pub fn keeps_source_order(self) -> bool {
        // Themes as well as symbols: the built-in ones are listed first because
        // they are the ones that always work, and a title sort would bury them
        // under whatever is installed.
        matches!(self, Self::Symbols | Self::Themes)
    }
}

/// Compares two titles the way a reader scans a list: without regard to case.
///
/// Character by character rather than by lowercasing both first, which would
/// allocate a pair of strings for every comparison in every sort.
fn compare_titles(left: &str, right: &str) -> std::cmp::Ordering {
    left.chars()
        .flat_map(char::to_lowercase)
        .cmp(right.chars().flat_map(char::to_lowercase))
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

    /// A prompt with no list, pre-filled with `text` and the caret at its end.
    ///
    /// For the prompts that stand in for a file dialog: typing a whole path from
    /// nothing is worse than editing the one you are already in.
    pub fn seeded(kind: PromptKind, text: String) -> Self {
        let mut input = Input::new();
        input.set(text);
        Self {
            kind,
            input,
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
        // By rank first, always. The tie-break depends on whether the supplied
        // order means anything — see `PromptKind::keeps_source_order`. The sort is
        // stable, so leaving the comparison at the rank keeps that order.
        if self.kind.keeps_source_order() {
            scored.sort_by_key(|(rank, _)| *rank);
        } else {
            scored.sort_by(|a, b| {
                let (left, right) = (&self.choices[a.1].title, &self.choices[b.1].title);
                a.0.cmp(&b.0)
                    // Case-insensitively, because byte order puts `JSON` before
                    // `Java` — every capital sorts below every lowercase letter —
                    // and a list nobody can predict the order of is one nobody can
                    // scan. The byte comparison remains as the tie-break so the
                    // order is still total, and so two titles differing only in
                    // case do not swap between presses.
                    .then_with(|| compare_titles(left, right))
                    .then_with(|| left.cmp(right))
            });
        }

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
        PaletteEntry::new(id, title)
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
    #[test]
    fn symbols_keep_document_order_while_commands_sort_by_title() {
        // A symbol list arrives in the order the file reads in, and re-sorting it
        // alphabetically would put `bump` above `new` in a file where `new` comes
        // first — an outline that does not match the outline.
        let choices = vec![
            PaletteEntry::at("/w/a.rs", "Counter", deco_core::Position::new(0, 7)),
            PaletteEntry::at("/w/a.rs", "Counter.value", deco_core::Position::new(1, 4)),
            PaletteEntry::at("/w/a.rs", "Counter.new", deco_core::Position::new(5, 7)),
            PaletteEntry::at("/w/a.rs", "Counter.bump", deco_core::Position::new(9, 7)),
        ];
        let symbols = Prompt::list(PromptKind::Symbols, choices.clone());
        assert_eq!(
            titles(&symbols),
            ["Counter", "Counter.value", "Counter.new", "Counter.bump"]
        );

        // The same list as commands sorts, because there the order is however the
        // registry happened to be written.
        let commands = Prompt::list(PromptKind::Commands, choices);
        assert_eq!(
            titles(&commands),
            ["Counter", "Counter.bump", "Counter.new", "Counter.value"]
        );
    }

    fn titles(prompt: &Prompt) -> Vec<String> {
        prompt
            .visible()
            .iter()
            .map(|entry| entry.title.clone())
            .collect()
    }
}
