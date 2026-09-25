//! Guessing a file's indentation from the file.
//!
//! `editor.detectIndentation` is **on** in VS Code by default. With it, opening a
//! two-space project does not reindent it to four spaces the first time you press
//! `tab`. The settings apply to files without a consistent indentation; a file's
//! own indentation takes precedence.
//!
//! # The algorithm
//!
//! Tabs versus spaces is decided by count: the number of indented lines that begin
//! with a tab against the number that begin with a space.
//!
//! The width of a space indent comes from the **differences** between consecutive
//! lines' indents, not from the indents themselves. A file indented by four has
//! lines starting at 0, 4, 8 and 12 columns. Every one of those is a multiple of
//! two, so counting multiples would classify it as a two-space file. The
//! differences are all four, which is the correct width.
//!
//! Ties go to the smaller width, in VS Code's order: 2, 4, 6, 8, then the odd
//! sizes. Matching VS Code avoids reindenting a file differently from how VS Code
//! would treat it.

/// How many lines are examined before the guess settles.
///
/// VS Code's limit. A file's indentation is evident in its first few hundred lines,
/// so scanning a very large generated file in full gives no better result.
pub const MAX_LINES: usize = 10_000;

/// The widths considered, in the order ties are broken.
///
/// VS Code's order. Two comes before four, so a file where both score equally is
/// treated as two-space. That is the safer error: indenting by two in a four-space
/// file is visible, while indenting by four in a two-space file looks original.
const CANDIDATES: [usize; 7] = [2, 4, 6, 8, 3, 5, 7];

/// The indentation detected in a file.
///
/// Each field is `None` when the text gives no evidence for it: for example a
/// file with no indented line, or a tab-indented file, which gives no space
/// indent width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Guess {
    /// Whether it indents with spaces.
    pub insert_spaces: Option<bool>,
    /// How many columns one level of space indentation is.
    pub tab_size: Option<usize>,
}

/// Reads `text`'s indentation.
pub fn guess(text: &str) -> Guess {
    let mut tab_lines = 0usize;
    let mut space_lines = 0usize;
    // How often each width appears as the step between two lines' indents.
    let mut steps = [0usize; 9];
    // The previous non-blank line's leading spaces, in columns.
    let mut previous: Option<usize> = None;

    for line in text.lines().take(MAX_LINES) {
        let spaces = line.chars().take_while(|c| *c == ' ').count();
        let tabs = line.chars().take_while(|c| *c == '\t').count();
        // Skip whitespace-only lines. Counting them as an indent of zero would
        // make every blank line inside an indented block look like an outdent.
        if line.len() == spaces + tabs {
            continue;
        }

        if tabs > 0 {
            tab_lines += 1;
            // A tab line contributes no space width and breaks the run. The
            // space-indented lines on either side of it are not consecutive, so
            // the difference between them is not recorded as a step.
            previous = None;
            continue;
        }
        if spaces > 0 {
            space_lines += 1;
        }

        if let Some(before) = previous {
            let step = spaces.abs_diff(before);
            if (1..=8).contains(&step) {
                steps[step] += 1;
            }
        }
        previous = Some(spaces);
    }

    let insert_spaces = match (tab_lines, space_lines) {
        (0, 0) => None,
        // Equal counts give no result, and a mixed file is usually being
        // converted. Like VS Code, leave the choice to the setting.
        (tabs, spaces) if tabs == spaces => None,
        (tabs, spaces) => Some(spaces > tabs),
    };

    let tab_size = if insert_spaces == Some(true) {
        best_step(&steps)
    } else {
        // Tabs, or no result. Neither gives a space indent width, and the display
        // width of a tab is controlled by `editor.tabSize`.
        None
    };

    Guess {
        insert_spaces,
        tab_size,
    }
}

/// The most common step, ties going to the earlier candidate.
fn best_step(steps: &[usize; 9]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for candidate in CANDIDATES {
        let score = steps[candidate];
        if score == 0 {
            continue;
        }
        // Strictly greater, so the first candidate to reach a score keeps it.
        // This makes the order of `CANDIDATES` the tie-break order.
        if best.is_none_or(|(_, best)| score > best) {
            best = Some((candidate, score));
        }
    }
    best.map(|(candidate, _)| candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spaces(text: &str) -> Option<usize> {
        guess(text).tab_size
    }

    #[test]
    fn a_four_space_file_is_four_and_not_two() {
        // Every indent here is also a multiple of two, which is why steps are
        // counted instead of indents.
        let text = "fn a() {\n    if b {\n        c();\n    }\n}\n";
        assert_eq!(guess(text).insert_spaces, Some(true));
        assert_eq!(spaces(text), Some(4));
    }

    #[test]
    fn a_two_space_file_is_two() {
        let text = "const a = {\n  b: {\n    c: 1,\n  },\n};\n";
        assert_eq!(spaces(text), Some(2));
    }

    #[test]
    fn a_three_space_file_is_three() {
        // Odd widths occur in real files, and the candidates include them.
        let text = "a\n   b\n      c\n   d\n";
        assert_eq!(spaces(text), Some(3));
    }

    #[test]
    fn a_tab_indented_file_says_so_and_offers_no_width() {
        // The display width of a tab comes from `editor.tabSize`, not the file.
        let text = "fn a() {\n\tif b {\n\t\tc();\n\t}\n}\n";
        let guess = guess(text);
        assert_eq!(guess.insert_spaces, Some(false));
        assert_eq!(guess.tab_size, None);
    }

    #[test]
    fn a_file_with_no_indentation_says_nothing() {
        // The settings apply unchanged.
        let guess = guess("one\ntwo\nthree\n");
        assert_eq!(guess, Guess::default());
    }

    #[test]
    fn an_empty_file_says_nothing() {
        assert_eq!(guess(""), Guess::default());
    }

    #[test]
    fn a_blank_line_inside_a_block_is_not_an_outdent() {
        // Counting it as an indent of zero would make every paragraph break look
        // like a step of four out and another four back in.
        let text = "a\n  b\n\n  c\n    d\n";
        assert_eq!(spaces(text), Some(2));
    }

    #[test]
    fn a_line_of_only_whitespace_is_ignored_as_well() {
        let text = "a\n  b\n   \n  c\n";
        assert_eq!(guess(text).insert_spaces, Some(true));
        assert_eq!(spaces(text), Some(2));
    }

    #[test]
    fn a_file_split_evenly_between_tabs_and_spaces_says_nothing() {
        // Such a file is usually partway through a conversion. Guessing either
        // way would continue the conversion in an arbitrary direction.
        let text = "\tone\n  two\n";
        assert_eq!(guess(text).insert_spaces, None);
    }

    #[test]
    fn mostly_tabs_with_an_aligned_continuation_is_still_tabs() {
        let text = "\tone\n\ttwo\n  aligned\n\tthree\n";
        assert_eq!(guess(text).insert_spaces, Some(false));
    }

    #[test]
    fn a_tab_line_does_not_invent_a_step_across_itself() {
        // The space-indented lines on either side of a tab-indented one are not
        // consecutive. Comparing them would record a step that is not in the file.
        let text = "  a\n\tb\n      c\n";
        // `a` and `c` are not compared, so no step is recorded at all.
        assert_eq!(spaces(text), None);
    }

    #[test]
    fn ties_go_to_the_smaller_width() {
        // One step of two and one of four. Indenting by two in a four-space file is
        // visible, while indenting by four in a two-space file looks original.
        let text = "a\n  b\nc\n    d\n";
        assert_eq!(spaces(text), Some(2));
    }

    #[test]
    fn the_most_common_step_wins_over_an_earlier_candidate() {
        // Order only breaks ties. A higher count always wins.
        let text = "a\n    b\nc\n    d\ne\n  f\n";
        assert_eq!(spaces(text), Some(4));
    }

    #[test]
    fn a_step_wider_than_eight_is_not_a_candidate() {
        // Twelve columns is alignment or a wrapped argument list, not one level.
        let text = "a\n            b\n";
        assert_eq!(spaces(text), None);
    }

    #[test]
    fn only_the_first_lines_are_examined() {
        // The cost of the guess must not grow with the file length. Two-space
        // indentation up to the limit, then four-space after it.
        let mut text = "a\n  b\n".repeat(MAX_LINES);
        text.push_str(&"c\n    d\n".repeat(100));
        assert_eq!(spaces(&text), Some(2));
    }

    #[test]
    fn crlf_line_endings_do_not_become_indentation() {
        // `str::lines` strips them, which this relies on.
        let text = "a\r\n  b\r\n    c\r\n";
        assert_eq!(spaces(text), Some(2));
    }
}
