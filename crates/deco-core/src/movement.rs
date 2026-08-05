//! Caret motion primitives: graphemes, words, lines and display columns.
//!
//! Everything here is a pure function of `(buffer, position)`, which is what
//! lets the same motions drive the terminal frontend, the GPU frontend and the
//! headless tests without duplication.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::buffer::Buffer;
use crate::position::{Position, Range};
use crate::selection::Selection;

/// VS Code's default `editor.wordSeparators`.
pub const DEFAULT_WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

/// Horizontal direction of a motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalDirection {
    /// Towards the start of the document.
    Left,
    /// Towards the end of the document.
    Right,
}

/// Vertical direction of a motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalDirection {
    /// Towards line 0.
    Up,
    /// Towards the last line.
    Down,
}

/// How much a horizontal motion covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// One user-perceived character (grapheme cluster).
    Grapheme,
    /// One word, using `editor.wordSeparators`.
    Word,
    /// To the start or end of the line.
    Line,
}

/// The three character classes VS Code's word motions distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordCategory {
    /// Spaces and tabs.
    Whitespace,
    /// A character listed in `editor.wordSeparators`.
    Separator,
    /// Everything else — letters, digits, CJK, underscores.
    Word,
}

/// Classifies `c` against `separators`.
pub fn categorize(c: char, separators: &str) -> WordCategory {
    if c.is_whitespace() {
        WordCategory::Whitespace
    } else if separators.contains(c) {
        WordCategory::Separator
    } else {
        WordCategory::Word
    }
}

/// Converts a UTF-16 column into a byte offset within `text`.
fn utf16_to_byte(text: &str, utf16_col: u32) -> usize {
    let mut remaining = utf16_col as usize;
    for (byte_idx, c) in text.char_indices() {
        if remaining == 0 {
            return byte_idx;
        }
        let units = c.len_utf16();
        if units > remaining {
            // Landing inside a surrogate pair: snap to the character start
            // rather than producing a byte offset that isn't a char boundary.
            return byte_idx;
        }
        remaining -= units;
    }
    text.len()
}

/// Converts a byte offset within `text` into a UTF-16 column.
fn byte_to_utf16(text: &str, byte_idx: usize) -> u32 {
    text[..byte_idx.min(text.len())]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// The rendered column of `utf16_col`, expanding tabs to the next multiple of
/// `tab_size` and counting East Asian wide characters as two columns.
pub fn display_column(text: &str, utf16_col: u32, tab_size: usize) -> usize {
    let tab_size = tab_size.max(1);
    let end = utf16_to_byte(text, utf16_col);
    let mut col = 0usize;
    for c in text[..end].chars() {
        if c == '\t' {
            col += tab_size - (col % tab_size);
        } else {
            col += c.to_string().width().max(1);
        }
    }
    col
}

/// The UTF-16 column whose rendered position is closest to `display_col`.
///
/// When `display_col` falls in the middle of a tab or a wide character the
/// caret snaps to the nearer edge, which is what makes vertical motion through
/// indented or CJK text feel stable.
pub fn utf16_col_at_display(text: &str, display_col: usize, tab_size: usize) -> u32 {
    let tab_size = tab_size.max(1);
    let mut col = 0usize;
    let mut utf16 = 0u32;
    for c in text.chars() {
        if col >= display_col {
            return utf16;
        }
        let width = if c == '\t' {
            tab_size - (col % tab_size)
        } else {
            c.to_string().width().max(1)
        };
        // If the target lands inside this character, pick the closer edge.
        if col + width > display_col {
            return if display_col - col >= width.div_ceil(2) {
                utf16 + c.len_utf16() as u32
            } else {
                utf16
            };
        }
        col += width;
        utf16 += c.len_utf16() as u32;
    }
    utf16
}

/// Moves one grapheme cluster left, crossing to the previous line at column 0.
pub fn grapheme_left(buffer: &Buffer, pos: Position) -> Position {
    let pos = buffer.clamp_position(pos);
    if pos.character == 0 {
        if pos.line == 0 {
            return pos;
        }
        let prev = pos.line - 1;
        return Position::new(prev, buffer.line_len_utf16(prev as usize));
    }
    let line = line_string(buffer, pos.line);
    let byte = utf16_to_byte(&line, pos.character);
    let prev_byte = line[..byte]
        .grapheme_indices(true)
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0);
    Position::new(pos.line, byte_to_utf16(&line, prev_byte))
}

/// Moves one grapheme cluster right, crossing to the next line at the end.
pub fn grapheme_right(buffer: &Buffer, pos: Position) -> Position {
    let pos = buffer.clamp_position(pos);
    let line = line_string(buffer, pos.line);
    let byte = utf16_to_byte(&line, pos.character);
    if byte >= line.len() {
        if (pos.line as usize) + 1 >= buffer.line_count() {
            return pos;
        }
        return Position::new(pos.line + 1, 0);
    }
    let next_byte = line[byte..]
        .grapheme_indices(true)
        .next()
        .map(|(_, g)| byte + g.len())
        .unwrap_or(line.len());
    Position::new(pos.line, byte_to_utf16(&line, next_byte))
}

/// The line's text without its terminator.
fn line_string(buffer: &Buffer, line: u32) -> String {
    buffer
        .line_content(line as usize)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Moves to the start of the previous word (`cursorWordStartLeft`).
///
/// Mirrors VS Code's `WordOperations._moveWordLeft`: a caret in column 0 first
/// hops to the end of the previous line, and the word scan then happens
/// entirely within that one line. Scanning line-locally (rather than treating
/// `\n` as ordinary whitespace and running on) is what makes the caret stop on
/// blank lines instead of skipping over them.
pub fn word_start_left(buffer: &Buffer, pos: Position, separators: &str) -> Position {
    let mut pos = buffer.clamp_position(pos);
    if pos.character == 0 {
        if pos.line == 0 {
            return pos;
        }
        let prev = pos.line - 1;
        pos = Position::new(prev, buffer.line_len_utf16(prev as usize));
    }

    let line = line_string(buffer, pos.line);
    let mut byte = utf16_to_byte(&line, pos.character);

    while let Some(c) = line[..byte].chars().next_back() {
        if categorize(c, separators) == WordCategory::Whitespace {
            byte -= c.len_utf8();
        } else {
            break;
        }
    }

    let Some(anchor) = line[..byte].chars().next_back() else {
        return Position::new(pos.line, 0);
    };
    let category = categorize(anchor, separators);

    while let Some(c) = line[..byte].chars().next_back() {
        if categorize(c, separators) == category {
            byte -= c.len_utf8();
        } else {
            break;
        }
    }
    Position::new(pos.line, byte_to_utf16(&line, byte))
}

/// Moves to the end of the next word (`cursorWordEndRight`).
///
/// The mirror image of [`word_start_left`]: a caret at the end of a line first
/// hops to column 0 of the next line, then scans within that line.
pub fn word_end_right(buffer: &Buffer, pos: Position, separators: &str) -> Position {
    let mut pos = buffer.clamp_position(pos);
    if pos.character == buffer.line_len_utf16(pos.line as usize) {
        if (pos.line as usize) + 1 >= buffer.line_count() {
            return pos;
        }
        pos = Position::new(pos.line + 1, 0);
    }

    let line = line_string(buffer, pos.line);
    let mut byte = utf16_to_byte(&line, pos.character);

    while let Some(c) = line[byte..].chars().next() {
        if categorize(c, separators) == WordCategory::Whitespace {
            byte += c.len_utf8();
        } else {
            break;
        }
    }

    let Some(anchor) = line[byte..].chars().next() else {
        return Position::new(pos.line, byte_to_utf16(&line, line.len()));
    };
    let category = categorize(anchor, separators);

    while let Some(c) = line[byte..].chars().next() {
        if categorize(c, separators) == category {
            byte += c.len_utf8();
        } else {
            break;
        }
    }
    Position::new(pos.line, byte_to_utf16(&line, byte))
}

/// The word surrounding `pos`, used by double-click and `Ctrl+D`.
///
/// A position inside whitespace yields the whitespace run, matching how VS Code
/// selects when you double-click a gap.
pub fn word_range_at(buffer: &Buffer, pos: Position, separators: &str) -> Range {
    let pos = buffer.clamp_position(pos);
    let line = line_string(buffer, pos.line);
    if line.is_empty() {
        return Range::empty(pos);
    }
    let byte = utf16_to_byte(&line, pos.character).min(line.len());

    // Prefer the character to the left when sitting exactly between two runs,
    // so a caret at the end of a word selects that word.
    let probe = if byte >= line.len() {
        line[..byte].chars().next_back()
    } else {
        let here = line[byte..].chars().next();
        let left = line[..byte].chars().next_back();
        match (left, here) {
            (Some(l), Some(h))
                if categorize(l, separators) == WordCategory::Word
                    && categorize(h, separators) != WordCategory::Word =>
            {
                Some(l)
            }
            _ => here,
        }
    };
    let Some(probe) = probe else {
        return Range::empty(pos);
    };
    let category = categorize(probe, separators);

    let mut start = byte;
    while let Some(c) = line[..start].chars().next_back() {
        if categorize(c, separators) != category {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = byte;
    // If we probed leftwards the caret may already sit past the run's end.
    if end > 0
        && line[..end]
            .chars()
            .next_back()
            .map(|c| categorize(c, separators))
            == Some(category)
        && line[end..]
            .chars()
            .next()
            .map(|c| categorize(c, separators))
            != Some(category)
    {
        // `end` is already correct.
    } else {
        while let Some(c) = line[end..].chars().next() {
            if categorize(c, separators) != category {
                break;
            }
            end += c.len_utf8();
        }
    }

    Range::new(
        Position::new(pos.line, byte_to_utf16(&line, start)),
        Position::new(pos.line, byte_to_utf16(&line, end)),
    )
}

/// Column 0 of `pos`'s line.
pub fn line_start(pos: Position) -> Position {
    pos.with_character(0)
}

/// The first non-whitespace column of `pos`'s line.
pub fn first_non_whitespace(buffer: &Buffer, line: u32) -> Position {
    let text = line_string(buffer, line);
    let byte = text
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(text.len());
    Position::new(line, byte_to_utf16(&text, byte))
}

/// `Home` with VS Code's toggle: jump to the first non-whitespace column, and
/// only to column 0 if already there.
pub fn smart_home(buffer: &Buffer, pos: Position) -> Position {
    let indent_end = first_non_whitespace(buffer, pos.line);
    if pos.character == indent_end.character {
        line_start(pos)
    } else {
        indent_end
    }
}

/// The end of `pos`'s line.
pub fn line_end(buffer: &Buffer, pos: Position) -> Position {
    Position::new(pos.line, buffer.line_len_utf16(pos.line as usize))
}

/// Moves `selection` horizontally, returning the new active position.
pub fn horizontal(
    buffer: &Buffer,
    pos: Position,
    direction: HorizontalDirection,
    granularity: Granularity,
    separators: &str,
) -> Position {
    match (granularity, direction) {
        (Granularity::Grapheme, HorizontalDirection::Left) => grapheme_left(buffer, pos),
        (Granularity::Grapheme, HorizontalDirection::Right) => grapheme_right(buffer, pos),
        (Granularity::Word, HorizontalDirection::Left) => word_start_left(buffer, pos, separators),
        (Granularity::Word, HorizontalDirection::Right) => word_end_right(buffer, pos, separators),
        (Granularity::Line, HorizontalDirection::Left) => smart_home(buffer, pos),
        (Granularity::Line, HorizontalDirection::Right) => line_end(buffer, pos),
    }
}

/// Moves `selection`'s active end vertically by `count` lines, maintaining the
/// sticky goal column.
///
/// Returns the updated selection so the caller does not have to thread
/// `goal_column` through by hand — forgetting to do so is the usual cause of
/// "the caret drifts left when I scroll through short lines".
pub fn vertical(
    buffer: &Buffer,
    selection: Selection,
    direction: VerticalDirection,
    count: u32,
    tab_size: usize,
    extend: bool,
) -> Selection {
    let pos = buffer.clamp_position(selection.active);
    let current_line_text = line_string(buffer, pos.line);
    let goal = selection
        .goal_column
        .unwrap_or_else(|| display_column(&current_line_text, pos.character, tab_size) as u32);

    let target_line = match direction {
        VerticalDirection::Up => pos.line.saturating_sub(count),
        VerticalDirection::Down => {
            let max = (buffer.line_count() - 1) as u32;
            pos.line.saturating_add(count).min(max)
        }
    };

    let target_text = line_string(buffer, target_line);
    let character = utf16_col_at_display(&target_text, goal as usize, tab_size);
    let new_pos = Position::new(target_line, character);

    let mut next = if extend {
        selection.extended_to(new_pos)
    } else {
        selection.moved_to(new_pos)
    };
    next.goal_column = Some(goal);
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEP: &str = DEFAULT_WORD_SEPARATORS;

    fn buf(text: &str) -> Buffer {
        Buffer::from_text(text)
    }

    fn p(line: u32, ch: u32) -> Position {
        Position::new(line, ch)
    }

    #[test]
    fn categorizes_like_vscode() {
        assert_eq!(categorize(' ', SEP), WordCategory::Whitespace);
        assert_eq!(categorize('\t', SEP), WordCategory::Whitespace);
        assert_eq!(categorize('.', SEP), WordCategory::Separator);
        assert_eq!(categorize('(', SEP), WordCategory::Separator);
        assert_eq!(categorize('a', SEP), WordCategory::Word);
        assert_eq!(categorize('_', SEP), WordCategory::Word);
        assert_eq!(categorize('漢', SEP), WordCategory::Word);
    }

    #[test]
    fn grapheme_motion_skips_combining_marks() {
        // "e" + combining acute is one grapheme, two chars, two UTF-16 units.
        let b = buf("e\u{301}x");
        assert_eq!(grapheme_right(&b, p(0, 0)), p(0, 2));
        assert_eq!(grapheme_left(&b, p(0, 2)), p(0, 0));
    }

    #[test]
    fn grapheme_motion_skips_surrogate_pairs_atomically() {
        let b = buf("a😀b");
        assert_eq!(grapheme_right(&b, p(0, 1)), p(0, 3));
        assert_eq!(grapheme_left(&b, p(0, 3)), p(0, 1));
    }

    #[test]
    fn grapheme_motion_crosses_lines() {
        let b = buf("ab\ncd");
        assert_eq!(grapheme_right(&b, p(0, 2)), p(1, 0));
        assert_eq!(grapheme_left(&b, p(1, 0)), p(0, 2));
    }

    #[test]
    fn grapheme_motion_stops_at_document_bounds() {
        let b = buf("ab");
        assert_eq!(grapheme_left(&b, p(0, 0)), p(0, 0));
        assert_eq!(grapheme_right(&b, p(0, 2)), p(0, 2));
    }

    #[test]
    fn word_end_right_stops_at_word_boundaries() {
        let b = buf("let foo_bar = baz();");
        assert_eq!(word_end_right(&b, p(0, 0), SEP), p(0, 3)); // "let"
        assert_eq!(word_end_right(&b, p(0, 3), SEP), p(0, 11)); // "foo_bar"
        assert_eq!(word_end_right(&b, p(0, 11), SEP), p(0, 13)); // "="
    }

    #[test]
    fn word_start_left_stops_at_word_boundaries() {
        let b = buf("let foo_bar = baz");
        assert_eq!(word_start_left(&b, p(0, 17), SEP), p(0, 14)); // start of "baz"
        assert_eq!(word_start_left(&b, p(0, 14), SEP), p(0, 12)); // the "="
        assert_eq!(word_start_left(&b, p(0, 11), SEP), p(0, 4)); // start of "foo_bar"
    }

    #[test]
    fn word_motion_treats_separator_runs_as_one_unit() {
        let b = buf("a->b");
        assert_eq!(word_end_right(&b, p(0, 1), SEP), p(0, 3)); // "->"
        assert_eq!(word_start_left(&b, p(0, 3), SEP), p(0, 1));
    }

    #[test]
    fn word_motion_crosses_lines() {
        let b = buf("one\ntwo");
        // Right from the end of line 0 hops to line 1 and then completes a word.
        assert_eq!(word_end_right(&b, p(0, 3), SEP), p(1, 3));
        // Left from column 0 hops to the end of line 0 and then completes a
        // word — VS Code lands on the *start* of "one", not its end.
        assert_eq!(word_start_left(&b, p(1, 0), SEP), p(0, 0));
    }

    #[test]
    fn word_motion_stops_on_blank_lines() {
        // Scanning is line-local, so a blank line is a stop rather than
        // something the caret skims over.
        let b = buf("one\n\ntwo");
        assert_eq!(word_start_left(&b, p(2, 0), SEP), p(1, 0));
        assert_eq!(word_end_right(&b, p(0, 3), SEP), p(1, 0));
    }

    #[test]
    fn word_motion_skips_trailing_whitespace_when_hopping_lines() {
        let b = buf("one   \ntwo");
        assert_eq!(word_start_left(&b, p(1, 0), SEP), p(0, 0));
    }

    #[test]
    fn word_range_at_selects_the_surrounding_word() {
        let b = buf("let foo_bar = 1");
        assert_eq!(
            word_range_at(&b, p(0, 6), SEP),
            Range::new(p(0, 4), p(0, 11))
        );
        // Caret at the end of a word still selects that word.
        assert_eq!(
            word_range_at(&b, p(0, 11), SEP),
            Range::new(p(0, 4), p(0, 11))
        );
        // Caret at the start selects it too.
        assert_eq!(
            word_range_at(&b, p(0, 4), SEP),
            Range::new(p(0, 4), p(0, 11))
        );
    }

    #[test]
    fn word_range_on_empty_line_is_empty() {
        let b = buf("\nx");
        assert!(word_range_at(&b, p(0, 0), SEP).is_empty());
    }

    #[test]
    fn smart_home_toggles_between_indent_and_column_zero() {
        let b = buf("    indented");
        assert_eq!(smart_home(&b, p(0, 8)), p(0, 4));
        assert_eq!(smart_home(&b, p(0, 4)), p(0, 0));
        assert_eq!(smart_home(&b, p(0, 0)), p(0, 4));
    }

    #[test]
    fn line_end_is_before_the_terminator() {
        let b = buf("hello\nworld\n");
        assert_eq!(line_end(&b, p(0, 0)), p(0, 5));
    }

    #[test]
    fn display_column_expands_tabs_to_tab_stops() {
        assert_eq!(display_column("\tx", 1, 4), 4);
        assert_eq!(display_column("ab\tx", 3, 4), 4);
        assert_eq!(display_column("abcd\tx", 5, 4), 8);
    }

    #[test]
    fn display_column_counts_wide_characters_as_two() {
        assert_eq!(display_column("漢字", 1, 4), 2);
        assert_eq!(display_column("漢字", 2, 4), 4);
    }

    #[test]
    fn display_column_round_trips() {
        let text = "\tfn 漢字(😀) {";
        for col in 0..=text.chars().map(|c| c.len_utf16() as u32).sum::<u32>() {
            let display = display_column(text, col, 4);
            let back = utf16_col_at_display(text, display, 4);
            assert_eq!(
                display_column(text, back, 4),
                display,
                "column {col} did not round trip"
            );
        }
    }

    #[test]
    fn vertical_motion_keeps_the_goal_column_through_short_lines() {
        let b = buf("aaaaaaaaaa\nbb\ncccccccccc");
        let start = Selection::caret(p(0, 8));

        let down = vertical(&b, start, VerticalDirection::Down, 1, 4, false);
        assert_eq!(down.active, p(1, 2)); // clamped to the short line
        assert_eq!(down.goal_column, Some(8));

        let down2 = vertical(&b, down, VerticalDirection::Down, 1, 4, false);
        assert_eq!(down2.active, p(2, 8)); // restored on the long line
    }

    #[test]
    fn vertical_motion_clamps_at_document_bounds() {
        let b = buf("a\nb\nc");
        let up = vertical(
            &b,
            Selection::caret(p(0, 1)),
            VerticalDirection::Up,
            5,
            4,
            false,
        );
        assert_eq!(up.active.line, 0);
        let down = vertical(
            &b,
            Selection::caret(p(0, 1)),
            VerticalDirection::Down,
            99,
            4,
            false,
        );
        assert_eq!(down.active.line, 2);
    }

    #[test]
    fn vertical_motion_can_extend_the_selection() {
        let b = buf("aaa\nbbb");
        let sel = vertical(
            &b,
            Selection::caret(p(0, 1)),
            VerticalDirection::Down,
            1,
            4,
            true,
        );
        assert_eq!(sel.anchor, p(0, 1));
        assert_eq!(sel.active, p(1, 1));
        assert!(!sel.is_empty());
    }

    #[test]
    fn vertical_motion_aligns_across_tab_indentation() {
        // Line 0 column 4 is rendered at display column 4; on line 1 the tab
        // occupies display columns 0..4, so the caret must land after it.
        let b = buf("    x\n\ty");
        let sel = vertical(
            &b,
            Selection::caret(p(0, 4)),
            VerticalDirection::Down,
            1,
            4,
            false,
        );
        assert_eq!(sel.active, p(1, 1));
    }
}
