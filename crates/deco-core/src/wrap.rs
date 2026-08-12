//! Where a line is broken when it is wider than the text area.
//!
//! One line of the document becomes one or more rows on screen. Everything that
//! reads or writes a position still speaks in document lines and UTF-16 columns —
//! wrapping is a fact about the *display*, and letting it into the text model
//! would mean every edit had to know how wide the window is.
//!
//! # What this module answers
//!
//! [`row_starts`] gives the UTF-16 column each row of a line begins at, and
//! [`row_of`] says which of those rows a column falls on. Those two are enough to
//! draw a wrapped line, to scroll by rows, and to move a caret down one row
//! rather than down one line.
//!
//! # Where it breaks
//!
//! After whitespace, at the last opportunity that fits — so a word is kept whole
//! where it can be. Two rules qualify that, and both exist because the obvious
//! version of the rule produces a layout nobody wants:
//!
//! - **Whitespace itself never forces a break.** Trailing spaces hang past the
//!   right edge instead, where they are invisible. Breaking *before* the space
//!   that overflows would start the next row with it, and a continuation row
//!   beginning with a space reads as indentation the file does not have.
//! - **Whitespace before the row's first word is not an opportunity.** Otherwise
//!   an indented line breaks immediately after its indent, spending a row on a
//!   lone tab and starting the text at column zero — which loses the one visual
//!   cue that says how deep the line is.
//!
//! A run with no whitespace in it breaks at the width instead. That is not a
//! compromise for code, where a hundred-character run is a URL or a base64 blob
//! and any break is arbitrary; and it is the *right* answer for Chinese, Japanese
//! and Korean, which put no spaces between words and are broken between
//! characters by every editor that handles them.
//!
//! Line breaking proper — Unicode UAX #14, which knows that a closing bracket
//! may not start a row and that `,` may not be separated from what precedes it —
//! needs a table this crate does not carry and would be the first dependency
//! added for cosmetics.
//!
//! # The continuation indent
//!
//! Every function here takes the display column a continuation row's text starts
//! at, which `editor.wrappingIndent` decides. It is not cosmetic: a row pushed in by
//! four columns has four fewer to fill, and its tab stops land differently. Left out
//! of the measurement, the text would be drawn one place and broken in another.

use unicode_width::UnicodeWidthChar as _;

/// The UTF-16 column each visual row of `text` starts at.
///
/// The first is always `0`, so the result is never empty: an empty line still
/// occupies one row, because a caret has to sit somewhere.
///
/// `width` is the columns available for text, so the caller subtracts its gutter
/// first. A width below two disables wrapping — one column cannot hold a wide
/// character, and a layout that breaks every character is not readable at any
/// width, so refusing is better than looping.
pub fn row_starts(text: &str, width: usize, tab_size: usize, indent: usize) -> Vec<u32> {
    let mut starts = vec![0u32];
    if width < 2 {
        return starts;
    }
    let tab_size = tab_size.max(1);
    // Never so wide that a continuation row has no room; the caller caps it, and
    // this is the backstop that keeps the loop finite if one does not.
    let indent = indent.min(width.saturating_sub(2));

    // Display columns used by the row being filled, and where it started. `used`
    // counts from the left edge of the text area, so a continuation row starts at
    // `indent` — which is both how its tab stops line up and how it runs out of
    // room sooner than the first row does.
    let mut used = 0usize;
    let mut row_start = 0u32;
    // The column just after the most recent whitespace run on this row, and
    // `None` until one is seen — a row of solid text has nowhere better to break
    // than the width.
    let mut opportunity: Option<u32> = None;
    // Whether this row has any non-whitespace on it yet, which is what makes a
    // following space a break opportunity rather than part of the indent.
    let mut word_on_row = false;
    let mut column = 0u32;

    for c in text.chars() {
        let whitespace = c.is_whitespace();
        let advance = if c == '\t' {
            tab_size - (used % tab_size)
        } else {
            c.width().unwrap_or(0).max(1)
        };

        // Break *before* this character when it would overflow, so a wide
        // character is never split down the middle. Whitespace is exempt: it
        // hangs past the edge instead of starting the next row.
        if !whitespace && used + advance > width {
            // The opportunity has to be past the row's own start, or the break
            // makes no progress and the loop never ends.
            let at = match opportunity {
                Some(at) if at > row_start => at,
                _ => column.max(row_start + 1),
            };
            starts.push(at);
            row_start = at;
            opportunity = None;
            word_on_row = false;
            // Re-measured from the break, because tab stops are counted from the
            // start of the row a tab lands on and not of the document line — and
            // from `indent`, which is where a continuation row's text begins.
            used = indent + display_width_from(text, at, column, tab_size, indent);
        }

        used += advance;
        column += c.len_utf16() as u32;
        if whitespace {
            // Recorded after the character, so a break keeps the space on the row
            // that ended with it rather than starting the next one with it.
            if word_on_row {
                opportunity = Some(column);
            }
        } else {
            word_on_row = true;
        }
    }

    starts
}

/// The display width of `text` between two UTF-16 columns.
///
/// Its own function because the measurement restarts at a break: tab stops are
/// counted from the start of the row, and a tab in the middle of a wrapped line
/// advances to the next stop on the row it lands on.
///
/// Public because a renderer needs the same answer to place a caret on a wrapped
/// row — and if it computed the width its own way the two could disagree, which
/// is a caret sitting a column away from the character it is on.
pub fn width_between(text: &str, from: u32, to: u32, tab_size: usize) -> usize {
    display_width_from(text, from, to, tab_size.max(1), 0)
}

/// The same, for text drawn starting at display column `at`.
///
/// Only tabs care: their stops are counted from the left edge of the text area, so a
/// row pushed in by `editor.wrappingIndent` reaches a different one. The answer is
/// still relative — how many columns the text occupies — because that is what a
/// caller placing a caret within the row needs.
pub fn width_between_from(text: &str, from: u32, to: u32, tab_size: usize, at: usize) -> usize {
    display_width_from(text, from, to, tab_size.max(1), at)
}

fn display_width_from(text: &str, from: u32, to: u32, tab_size: usize, at: usize) -> usize {
    let mut used = at;
    let mut column = 0u32;
    for c in text.chars() {
        if column >= to {
            break;
        }
        if column >= from {
            used += if c == '\t' {
                tab_size - (used % tab_size)
            } else {
                c.width().unwrap_or(0).max(1)
            };
        }
        column += c.len_utf16() as u32;
    }
    used - at
}

/// The UTF-16 column `display` columns into the row `start..end`.
///
/// The counterpart to [`width_between`], and what vertical motion through a
/// wrapped line is built from: the caret keeps its column on screen, so the
/// question is which character sits under it on the row below.
///
/// Never past the row's own last character. A row that ends because the next
/// character would not fit is a column or two short of the width, and a caret
/// landing on `end` would be on the row below — one keypress moving two rows.
/// `end` is `None` for a line's last row, where there is no row below and the
/// caret may sit one past the final character.
pub fn column_in_row(
    text: &str,
    start: u32,
    end: Option<u32>,
    display: usize,
    tab_size: usize,
) -> u32 {
    column_in_row_from(text, start, end, display, tab_size, 0)
}

/// The same, for a row whose text begins at display column `at`.
///
/// `display` stays relative to the row's own text, so a caller keeping a caret's
/// column across rows does not have to know how far each is pushed in.
pub fn column_in_row_from(
    text: &str,
    start: u32,
    end: Option<u32>,
    display: usize,
    tab_size: usize,
    at: usize,
) -> u32 {
    let tab_size = tab_size.max(1);
    let mut used = at;
    let display = display + at;
    let mut column = 0u32;
    // The last position the caret may take on this row.
    let mut last = start;

    for c in text.chars() {
        if column < start {
            column += c.len_utf16() as u32;
            continue;
        }
        if end.is_some_and(|end| column >= end) {
            break;
        }
        if used >= display {
            return column;
        }
        let advance = if c == '\t' {
            tab_size - (used % tab_size)
        } else {
            c.width().unwrap_or(0).max(1)
        };
        // Landing inside a tab or a wide character snaps to the nearer edge, which
        // is what keeps vertical motion through indented or CJK text stable.
        if used + advance > display {
            return if display - used >= advance.div_ceil(2) {
                column + c.len_utf16() as u32
            } else {
                column
            };
        }
        used += advance;
        last = column;
        column += c.len_utf16() as u32;
    }

    // Ran out of row. A line's last row may hold the caret one past its text; any
    // other row hands it back to the last character it actually shows.
    match end {
        None => column,
        Some(_) => last.max(start),
    }
}

/// Which row of a line `column` falls on, given that line's [`row_starts`].
///
/// A column at a break belongs to the row it *starts*, which is what puts the
/// caret at the beginning of the second row rather than off the end of the first.
pub fn row_of(starts: &[u32], column: u32) -> usize {
    starts
        .partition_point(|start| *start <= column)
        .saturating_sub(1)
}

/// The UTF-16 columns row `row` covers, as `start..end`.
///
/// The end is the next row's start, or `None` for the last row — a caller
/// drawing it wants the rest of the line, and a caller measuring it wants to know
/// there is nothing after.
pub fn row_range(starts: &[u32], row: usize) -> (u32, Option<u32>) {
    let start = starts.get(row).copied().unwrap_or(0);
    (start, starts.get(row + 1).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows `text` is broken into, as strings, for tests that read better
    /// that way than as a list of columns.
    fn rows(text: &str, width: usize) -> Vec<String> {
        let starts = row_starts(text, width, 4, 0);
        let chars: Vec<char> = text.chars().collect();
        // UTF-16 columns to char indices. Sound here because the tests below use
        // no astral-plane characters; the surrogate case is tested through
        // `row_starts` directly.
        starts
            .iter()
            .enumerate()
            .map(|(index, start)| {
                let end = starts.get(index + 1).copied().unwrap_or(u32::MAX);
                chars
                    .iter()
                    .skip(*start as usize)
                    .take((end - start) as usize)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_line_that_fits_is_one_row() {
        assert_eq!(row_starts("short", 20, 4, 0), [0]);
    }

    #[test]
    fn an_empty_line_still_occupies_a_row() {
        // A caret has to sit somewhere.
        assert_eq!(row_starts("", 20, 4, 0), [0]);
    }

    #[test]
    fn it_breaks_after_the_last_space_that_fits() {
        assert_eq!(rows("the quick brown fox", 10), ["the quick ", "brown fox"]);
    }

    #[test]
    fn the_space_stays_on_the_row_it_ended() {
        // Otherwise a continuation row begins with a space that reads as indent
        // the file does not have.
        let rows = rows("aaa bbb ccc", 8);
        assert!(
            rows.iter().skip(1).all(|row| !row.starts_with(' ')),
            "{rows:?}"
        );
    }

    #[test]
    fn a_word_longer_than_the_width_breaks_at_the_width() {
        // A URL or a base64 blob. Any break is arbitrary, so the one that uses
        // the whole row is the least bad.
        assert_eq!(rows("aaaaaaaaaa", 4), ["aaaa", "aaaa", "aa"]);
    }

    #[test]
    fn a_long_word_after_a_space_starts_its_own_row() {
        assert_eq!(rows("ab cdefghij", 5), ["ab ", "cdefg", "hij"]);
    }

    #[test]
    fn a_wide_character_is_never_split() {
        // Two columns each, so an odd width leaves one unused rather than
        // drawing half a character.
        assert_eq!(rows("日本語です", 5), ["日本", "語で", "す"]);
    }

    #[test]
    fn cjk_breaks_between_characters_because_it_has_no_spaces() {
        let rows = rows("これは日本語の文章です", 8);
        assert_eq!(rows, ["これは日", "本語の文", "章です"]);
    }

    #[test]
    fn a_tab_is_measured_to_its_stop_on_the_row_it_lands_on() {
        // The first row is `\t` (4) + `ab` (2) = 6 of 8, and `cd` would make 8,
        // which fits exactly; `ef` overflows.
        assert_eq!(rows("\tabcdef", 8), ["\tabcd", "ef"]);
    }

    #[test]
    fn a_tab_after_a_break_counts_from_the_new_rows_start() {
        // Not from the document line's start: the row is what has tab stops on
        // screen. `xx` then a tab to column 4, then `yy` — six columns, so it
        // fits a width of six.
        assert_eq!(row_starts("aaaaaa xx\tyy", 6, 4, 0), [0, 7]);
    }

    #[test]
    fn a_width_below_two_does_not_wrap() {
        // One column cannot hold a wide character, and breaking every character
        // is not a layout. Refusing beats looping.
        assert_eq!(row_starts("abcdef", 1, 4, 0), [0]);
        assert_eq!(row_starts("abcdef", 0, 4, 0), [0]);
    }

    #[test]
    fn every_break_makes_progress() {
        // The loop's termination argument, stated as a test: whatever the text,
        // the starts strictly increase and none is past the end.
        let end = |text: &str| text.chars().map(|c| c.len_utf16() as u32).sum::<u32>();
        for text in [
            "   ",
            " a ",
            "\t\t\t\t\t\t",
            "aaaa aaaa",
            "日 本",
            " 日本語",
            "a\tb\tc\td\te",
        ] {
            for width in 2..12 {
                let starts = row_starts(text, width, 4, 0);
                assert!(
                    starts.windows(2).all(|w| w[1] > w[0]),
                    "{text:?} {starts:?}"
                );
                assert!(
                    starts.iter().all(|start| *start <= end(text)),
                    "{text:?} {starts:?}"
                );
            }
        }
    }

    #[test]
    fn a_run_of_spaces_wider_than_the_row_does_not_stall() {
        // The break opportunity is at the row's own start, which would make no
        // progress; the width has to win.
        let starts = row_starts("        x", 4, 4, 0);
        assert!(starts.len() > 1, "{starts:?}");
        assert!(starts.windows(2).all(|w| w[1] > w[0]), "{starts:?}");
    }

    #[test]
    fn astral_characters_advance_two_utf16_units() {
        // An emoji is one character, two UTF-16 code units, and two columns
        // wide — the three counts all differ, which is where off-by-ones live.
        let starts = row_starts("😀😀😀", 4, 4, 0);
        assert_eq!(starts, [0, 4], "two per row, two units each");
    }

    // ---- The continuation indent -----------------------------------------

    #[test]
    fn a_continuation_row_pushed_in_has_less_room() {
        // Four columns of indent leave six of ten for text, so the same sentence
        // takes an extra row.
        let text = "aaa bbb ccc ddd";
        assert_eq!(row_starts(text, 10, 4, 0), [0, 8]);
        assert_eq!(row_starts(text, 10, 4, 4), [0, 8, 12]);
    }

    #[test]
    fn the_first_row_keeps_the_whole_width() {
        // The indent is a continuation row's, not the line's: the first row starts
        // where the line starts.
        let text = "aaaaaaaa bb";
        assert_eq!(
            row_starts(text, 10, 4, 4)[1],
            9,
            "broke after the space at 9"
        );
    }

    #[test]
    fn a_tab_on_a_pushed_in_row_reaches_the_stop_the_screen_has() {
        // Its stops are counted from the left edge of the text area, not from the
        // row's own start, so the indent shifts which one it lands on.
        assert_eq!(width_between_from("\tx", 0, 1, 4, 0), 4, "from column 0");
        assert_eq!(width_between_from("\tx", 0, 1, 4, 2), 2, "from column 2");
        assert_eq!(width_between_from("\tx", 0, 1, 4, 4), 4, "from column 4");
    }

    #[test]
    fn a_goal_column_stays_relative_to_the_rows_own_text() {
        // So a caller keeping a caret's column across rows does not have to know how
        // far each of them is pushed in.
        let text = "abcdef";
        assert_eq!(column_in_row_from(text, 0, None, 3, 4, 0), 3);
        assert_eq!(column_in_row_from(text, 0, None, 3, 4, 4), 3);
    }

    #[test]
    fn an_indent_wider_than_the_width_cannot_stall_the_loop() {
        // The caller caps it; this is the backstop, and the property that matters is
        // that the breaks still make progress.
        for indent in [8, 20, 500] {
            let starts = row_starts("aaaa bbbb cccc", 10, 4, indent);
            assert!(
                starts.windows(2).all(|w| w[1] > w[0]),
                "{indent} {starts:?}"
            );
        }
    }

    #[test]
    fn a_column_is_found_by_its_offset_into_the_row() {
        // "the quick " then "brown fox": the second row starts at column 10, so
        // three columns into it is `w`, at column 13.
        let text = "the quick brown fox";
        assert_eq!(column_in_row(text, 10, None, 0, 4), 10);
        assert_eq!(column_in_row(text, 10, None, 3, 4), 13);
    }

    #[test]
    fn the_caret_may_sit_one_past_the_end_of_a_lines_last_row() {
        // There is no row below to hand it to.
        assert_eq!(column_in_row("abc", 0, None, 99, 4), 3);
    }

    #[test]
    fn a_wrapped_row_hands_the_caret_back_rather_than_to_the_row_below() {
        // Landing on `end` would put the caret on the next row, so one press of
        // `down` would move two rows.
        assert_eq!(
            column_in_row("abcdef", 0, Some(3), 99, 4),
            2,
            "the last character this row shows"
        );
    }

    #[test]
    fn a_goal_inside_a_tab_snaps_to_the_nearer_edge() {
        // Otherwise moving down a column of indented lines drifts left.
        let text = "\tx";
        assert_eq!(column_in_row(text, 0, None, 1, 4), 0, "nearer the start");
        assert_eq!(column_in_row(text, 0, None, 3, 4), 1, "nearer the end");
    }

    #[test]
    fn a_goal_beyond_an_empty_row_stays_at_its_start() {
        assert_eq!(column_in_row("", 0, None, 40, 4), 0);
    }

    #[test]
    fn a_column_at_a_break_belongs_to_the_row_it_starts() {
        // Otherwise the caret sits off the end of the row above instead of at the
        // start of the one below.
        let starts = [0u32, 10, 20];
        assert_eq!(row_of(&starts, 0), 0);
        assert_eq!(row_of(&starts, 9), 0);
        assert_eq!(row_of(&starts, 10), 1);
        assert_eq!(row_of(&starts, 19), 1);
        assert_eq!(row_of(&starts, 20), 2);
        assert_eq!(row_of(&starts, 999), 2, "past the end is the last row");
    }

    #[test]
    fn a_rows_range_ends_where_the_next_begins() {
        let starts = [0u32, 10, 20];
        assert_eq!(row_range(&starts, 0), (0, Some(10)));
        assert_eq!(row_range(&starts, 1), (10, Some(20)));
        assert_eq!(
            row_range(&starts, 2),
            (20, None),
            "the last row runs to the end of the line"
        );
    }
}
