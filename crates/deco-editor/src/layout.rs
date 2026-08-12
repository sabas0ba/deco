//! How the screen is divided between the gutter and the editor groups.
//!
//! Here rather than in a frontend because two things now need the same answer.
//! The renderer needs it to draw, and the core needs it to wrap: where a line
//! breaks depends on how many columns are left for text, so a session that did
//! not know its own layout could only wrap by asking a frontend — and then the
//! two would be free to disagree about the width, which is the sort of
//! disagreement that shows up as a caret one column off the text it belongs to.
//!
//! Pure arithmetic over state the session already has. Nothing here touches a
//! terminal.

use deco_config::LineNumbers;

use crate::Document;

/// Columns the line-number gutter needs for `document`.
///
/// Per document rather than per session: two groups side by side can be showing
/// files of very different lengths, and each gutter has to fit its own.
pub fn gutter_width(document: &Document) -> usize {
    if document.settings.line_numbers == LineNumbers::Off {
        return 0;
    }
    let digits = document.buffer.line_count().to_string().len();
    // One space of padding on each side keeps the text off the numbers.
    digits.max(2) + 2
}

/// How wide each editor group's column is, left to right.
///
/// The remainder goes to the leftmost columns, a cell each, so the widths differ
/// by at most one and no column is left a cell short of the others for no reason.
/// One separator column sits between each pair, which is why the divisor counts
/// them out first.
pub fn column_widths(width: usize, groups: usize) -> Vec<usize> {
    if groups <= 1 {
        return vec![width];
    }
    let separators = groups - 1;
    let usable = width.saturating_sub(separators);
    let each = usable / groups;
    let extra = usable % groups;
    (0..groups)
        .map(|index| each + usize::from(index < extra))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_config::EditorSettings;

    fn document(lines: usize) -> Document {
        Document::from_file(
            std::path::PathBuf::from("/w/a.txt"),
            &"x\n".repeat(lines),
            EditorSettings::default(),
        )
    }

    #[test]
    fn the_gutter_fits_the_longest_line_number() {
        // `line_count` counts the empty line after a trailing newline, so ten
        // `x\n` lines are eleven lines and still two digits.
        assert_eq!(gutter_width(&document(9)), 4, "10 lines: two digits");
        assert_eq!(gutter_width(&document(99)), 5, "100 lines: three digits");
    }

    #[test]
    fn a_short_file_still_gets_two_digits_of_gutter() {
        // Otherwise the text shifts left as the file grows past nine lines, which
        // is a redraw of everything for no reason.
        assert_eq!(gutter_width(&document(1)), 4);
    }

    #[test]
    fn line_numbers_off_costs_no_columns() {
        let mut document = document(10);
        document.settings.line_numbers = LineNumbers::Off;
        assert_eq!(gutter_width(&document), 0);
    }

    #[test]
    fn one_group_gets_the_whole_width() {
        assert_eq!(column_widths(80, 1), [80]);
    }

    #[test]
    fn two_groups_split_the_width_minus_a_separator() {
        assert_eq!(column_widths(81, 2), [40, 40]);
    }

    #[test]
    fn the_remainder_goes_to_the_left() {
        // 80 less one separator is 79, so one column is a cell wider. Nobody is
        // left a cell short of the others for no reason.
        assert_eq!(column_widths(80, 2), [40, 39]);
        assert_eq!(column_widths(80, 3), [26, 26, 26]);
        assert_eq!(column_widths(81, 3), [27, 26, 26]);
    }

    #[test]
    fn a_width_too_small_to_divide_does_not_underflow() {
        assert_eq!(column_widths(1, 2), [0, 0]);
        assert_eq!(column_widths(0, 3), [0, 0, 0]);
    }
}
