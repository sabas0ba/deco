//! Finding literal text in a buffer.
//!
//! Literal, not regular expressions. That is what `ctrl+d` and `ctrl+shift+l`
//! search for — the text you have selected, matched exactly — and it is what a
//! find bar defaults to. A regex mode would need its own escaping rules and its
//! own error reporting for an invalid pattern; this module deliberately does not
//! reach for either.
//!
//! # Positions, not byte offsets
//!
//! Every result is a [`Range`] in the same UTF-16 coordinates the rest of
//! `deco-core` uses, so a match can be turned into a selection with no
//! conversion. Internally the search walks characters, because a byte-offset
//! search would find a match starting in the middle of a multi-byte character
//! and produce a range that cannot be a valid position.
//!
//! # Word boundaries, and one deliberate divergence from VS Code
//!
//! [`SearchOptions::whole_word`] uses the editor's own word rule: a word
//! character is alphanumeric or `_`. The constraint applies **only to the ends of
//! the needle that are themselves word characters**:
//!
//! - `foo` — both ends are word characters, so both neighbours must not be. It
//!   matches `foo` and not `foobar`.
//! - `(` — neither end is a word character, so there is no boundary to violate
//!   and it matches everywhere.
//!
//! VS Code implements the option as the regex `\bneedle\b`, which is the same
//! thing for any needle beginning and ending in a word character — every needle
//! anyone types with the option on. It differs for a needle like `(`: `\b(`
//! requires a *transition*, so VS Code finds the bracket in `f(x)` and not the
//! one in ` ( `. That is inherited from `\b` rather than intended, and matching
//! it would mean "whole word" quietly excluding results for a needle that has no
//! words in it. deco does not copy it.

use crate::position::{Position, Range};
use crate::Buffer;

/// How to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchOptions {
    /// Whether `Foo` and `foo` are different.
    pub case_sensitive: bool,
    /// Whether a match must be bounded by non-word characters.
    pub whole_word: bool,
}

impl SearchOptions {
    /// Case-sensitive, matching anywhere.
    ///
    /// What `ctrl+d` uses: the user selected exactly this text, so matching a
    /// different case would be a surprise.
    pub const EXACT: Self = Self {
        case_sensitive: true,
        whole_word: false,
    };
}

/// Every match of `needle` in `buffer`, in document order.
///
/// An empty needle matches nothing. Returning a match at every position would be
/// technically defensible and useless: `ctrl+shift+l` on an empty selection would
/// put a cursor on every character in the file.
pub fn find_all(buffer: &Buffer, needle: &str, options: SearchOptions) -> Vec<Range> {
    if needle.is_empty() {
        return Vec::new();
    }

    // Both sides folded once, rather than per comparison. `to_lowercase` rather
    // than `to_ascii_lowercase`, so `Straße` and `STRASSE` behave the way the
    // user's language does — Unicode case folding can change length, which is
    // exactly why the *positions* come from the haystack's own characters below
    // and never from the folded copy.
    let haystack: Vec<char> = buffer.text().chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();

    let folded_haystack: Vec<char> = if options.case_sensitive {
        haystack.clone()
    } else {
        haystack.iter().flat_map(|c| c.to_lowercase()).collect()
    };
    let folded_needle: Vec<char> = if options.case_sensitive {
        needle_chars.clone()
    } else {
        needle_chars.iter().flat_map(|c| c.to_lowercase()).collect()
    };

    // Case folding that changes length would misalign the two, and a misaligned
    // position is a range in the wrong place — worse than a missed match. When
    // it happens, fall back to a case-sensitive search rather than reporting
    // something wrong.
    let aligned =
        folded_haystack.len() == haystack.len() && folded_needle.len() == needle_chars.len();
    let (folded_haystack, folded_needle) = if aligned {
        (folded_haystack, folded_needle)
    } else {
        (haystack.clone(), needle_chars.clone())
    };

    let mut matches = Vec::new();
    let mut index = 0usize;
    while index + folded_needle.len() <= folded_haystack.len() {
        if folded_haystack[index..index + folded_needle.len()] == folded_needle[..] {
            let end = index + folded_needle.len();
            let bounded =
                !options.whole_word || is_whole_word(&haystack, index, end, &needle_chars);
            if bounded {
                matches.push(Range::new(
                    buffer.char_to_position(index),
                    buffer.char_to_position(end),
                ));
                // Non-overlapping: searching for `aa` in `aaaa` finds two
                // matches, not three. Overlapping matches would put two cursors
                // on the same characters, which multi-cursor editing cannot
                // represent.
                index = end;
                continue;
            }
        }
        index += 1;
    }
    matches
}

/// Whether a match respects word boundaries at the ends that need them.
///
/// Only the ends of the *needle* that are word characters impose a constraint —
/// see the module docs for why, and for how that differs from VS Code.
fn is_whole_word(haystack: &[char], start: usize, end: usize, needle: &[char]) -> bool {
    let starts_with_word = needle.first().is_some_and(|c| is_word_char(*c));
    let ends_with_word = needle.last().is_some_and(|c| is_word_char(*c));

    let before_ok = !starts_with_word || start == 0 || !is_word_char(haystack[start - 1]);
    let after_ok = !ends_with_word || end >= haystack.len() || !is_word_char(haystack[end]);
    before_ok && after_ok
}

/// The editor's word rule, shared with word motion and completion.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The first match at or after `from`, wrapping to the start of the document.
///
/// Wrapping is what makes `ctrl+d` usable: reaching the last occurrence and
/// pressing again returns to the first rather than doing nothing.
pub fn find_next(
    buffer: &Buffer,
    needle: &str,
    from: Position,
    options: SearchOptions,
) -> Option<Range> {
    let matches = find_all(buffer, needle, options);
    matches
        .iter()
        .find(|range| range.start >= from)
        .or(matches.first())
        .copied()
}

/// The last match ending at or before `from`, wrapping to the end.
pub fn find_previous(
    buffer: &Buffer,
    needle: &str,
    from: Position,
    options: SearchOptions,
) -> Option<Range> {
    let matches = find_all(buffer, needle, options);
    matches
        .iter()
        .rev()
        .find(|range| range.end <= from)
        .or(matches.last())
        .copied()
}

/// The word around `pos`, or `None` if there is no word character there.
///
/// What `ctrl+d` selects when pressed with no selection: the identifier under
/// the caret. A caret just after a word counts as being on it, because that is
/// where the caret sits after typing one.
pub fn word_at(buffer: &Buffer, pos: Position) -> Option<Range> {
    let line = buffer.line_content(pos.line as usize)?.to_string();
    let chars: Vec<char> = line.chars().collect();

    // The column is in UTF-16 units and the scan is in characters, so the two
    // have to be reconciled rather than assumed equal — they differ on any line
    // containing an emoji.
    let mut column = 0usize;
    let mut units = 0u32;
    while column < chars.len() && units < pos.character {
        units += chars[column].len_utf16() as u32;
        column += 1;
    }

    // Prefer the word the caret is inside; failing that, the one it is just
    // after. `foo|` is the common case: the caret follows what was typed.
    let anchor = if column < chars.len() && is_word_char(chars[column]) {
        column
    } else if column > 0 && is_word_char(chars[column - 1]) {
        column - 1
    } else {
        return None;
    };

    let mut start = anchor;
    while start > 0 && is_word_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = anchor + 1;
    while end < chars.len() && is_word_char(chars[end]) {
        end += 1;
    }

    let to_units =
        |upto: usize| -> u32 { chars[..upto].iter().map(|c| c.len_utf16() as u32).sum() };
    Some(Range::new(
        Position::new(pos.line, to_units(start)),
        Position::new(pos.line, to_units(end)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> Buffer {
        Buffer::from_text(text)
    }

    fn at(line: u32, character: u32) -> Position {
        Position::new(line, character)
    }

    fn ranges(matches: &[Range]) -> Vec<(u32, u32, u32, u32)> {
        matches
            .iter()
            .map(|r| (r.start.line, r.start.character, r.end.line, r.end.character))
            .collect()
    }

    #[test]
    fn every_occurrence_is_found_in_document_order() {
        let b = buffer("foo bar\nbaz foo\n");
        assert_eq!(
            ranges(&find_all(&b, "foo", SearchOptions::EXACT)),
            vec![(0, 0, 0, 3), (1, 4, 1, 7)]
        );
    }

    #[test]
    fn a_match_spanning_a_line_break_is_found() {
        let b = buffer("one\ntwo\n");
        assert_eq!(
            ranges(&find_all(&b, "one\ntwo", SearchOptions::EXACT)),
            vec![(0, 0, 1, 3)]
        );
    }

    #[test]
    fn an_empty_needle_matches_nothing() {
        // A match at every position is technically defensible and useless:
        // ctrl+shift+l on an empty selection would put a cursor on every
        // character in the file.
        assert!(find_all(&buffer("anything"), "", SearchOptions::EXACT).is_empty());
    }

    #[test]
    fn matches_do_not_overlap() {
        // `aa` in `aaaa` is two matches, not three. Overlapping ones would put
        // two cursors on the same characters, which multi-cursor cannot express.
        assert_eq!(
            ranges(&find_all(&buffer("aaaa"), "aa", SearchOptions::EXACT)),
            vec![(0, 0, 0, 2), (0, 2, 0, 4)]
        );
    }

    #[test]
    fn case_sensitivity_is_honoured_in_both_directions() {
        let b = buffer("Foo foo FOO");
        assert_eq!(find_all(&b, "foo", SearchOptions::EXACT).len(), 1);
        assert_eq!(
            find_all(
                &b,
                "foo",
                SearchOptions {
                    case_sensitive: false,
                    whole_word: false
                }
            )
            .len(),
            3
        );
    }

    #[test]
    fn a_case_insensitive_match_reports_the_haystacks_own_positions() {
        // The folded copy is only for comparing; a position taken from it would
        // be in the wrong place the moment folding changed a length.
        let b = buffer("xxFOOxx");
        let matches = find_all(
            &b,
            "foo",
            SearchOptions {
                case_sensitive: false,
                whole_word: false,
            },
        );
        assert_eq!(ranges(&matches), vec![(0, 2, 0, 5)]);
        assert_eq!(b.text_in_range(matches[0]), "FOO");
    }

    #[test]
    fn whole_word_rejects_a_match_inside_a_longer_word() {
        let b = buffer("foo foobar barfoo _foo foo_");
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: true,
        };
        // Only the standalone `foo` at the start qualifies: `_` is a word
        // character, so `_foo` and `foo_` are parts of longer words.
        assert_eq!(ranges(&find_all(&b, "foo", options)), vec![(0, 0, 0, 3)]);
    }

    #[test]
    fn whole_word_accepts_a_match_at_either_end_of_the_document() {
        let b = buffer("foo");
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: true,
        };
        assert_eq!(find_all(&b, "foo", options).len(), 1);
    }

    #[test]
    fn whole_word_does_not_constrain_a_needle_with_no_word_characters() {
        // A bracket has no word boundary to violate, so every one matches. This
        // is the one place deco diverges from VS Code, which implements the
        // option as `\bneedle\b` and so finds the bracket in `f(x)` but not the
        // one in ` ( ` — inherited from `\b` rather than intended. See the
        // module docs.
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: true,
        };
        assert_eq!(find_all(&buffer("f(x) g(y)"), "(", options).len(), 2);
        assert_eq!(find_all(&buffer(" ( ) "), "(", options).len(), 1);
    }

    #[test]
    fn whole_word_constrains_only_the_ends_that_are_word_characters() {
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: true,
        };
        // `foo(` ends in a bracket, so only its left side needs a boundary:
        // it matches `foo(` and not `barfoo(`.
        let b = buffer("foo(x) barfoo(y)");
        assert_eq!(ranges(&find_all(&b, "foo(", options)), vec![(0, 0, 0, 4)]);
    }

    #[test]
    fn non_ascii_text_is_matched_by_character_not_by_byte() {
        // A byte search would find a match starting mid-character and produce a
        // range that is not a valid position.
        let b = buffer("日本語 foo 日本語\n");
        assert_eq!(
            ranges(&find_all(&b, "日本語", SearchOptions::EXACT)),
            vec![(0, 0, 0, 3), (0, 8, 0, 11)]
        );
    }

    #[test]
    fn a_match_after_an_emoji_reports_utf16_columns() {
        // An emoji is two UTF-16 units, so the column is not the character index.
        let b = buffer("🎉foo\n");
        assert_eq!(
            ranges(&find_all(&b, "foo", SearchOptions::EXACT)),
            vec![(0, 2, 0, 5)]
        );
    }

    #[test]
    fn find_next_wraps_to_the_start() {
        // What makes ctrl+d usable: past the last occurrence, return to the first.
        let b = buffer("foo bar foo\n");
        assert_eq!(
            find_next(&b, "foo", at(0, 9), SearchOptions::EXACT).map(|r| r.start),
            None.or(Some(at(0, 0))),
            "from past the last match, wrap"
        );
        assert_eq!(
            find_next(&b, "foo", at(0, 1), SearchOptions::EXACT).map(|r| r.start),
            Some(at(0, 8))
        );
    }

    #[test]
    fn find_next_from_a_matchs_own_start_returns_that_match() {
        // So "find the match at the cursor" and "find the next one" are
        // different calls rather than the same one with an off-by-one.
        let b = buffer("foo foo\n");
        assert_eq!(
            find_next(&b, "foo", at(0, 4), SearchOptions::EXACT).map(|r| r.start),
            Some(at(0, 4))
        );
    }

    #[test]
    fn find_previous_wraps_to_the_end() {
        let b = buffer("foo bar foo\n");
        assert_eq!(
            find_previous(&b, "foo", at(0, 0), SearchOptions::EXACT).map(|r| r.start),
            Some(at(0, 8)),
            "from before the first match, wrap to the last"
        );
        assert_eq!(
            find_previous(&b, "foo", at(0, 11), SearchOptions::EXACT).map(|r| r.start),
            Some(at(0, 8))
        );
    }

    #[test]
    fn searching_for_something_absent_finds_nothing() {
        let b = buffer("foo\n");
        assert!(find_all(&b, "zzz", SearchOptions::EXACT).is_empty());
        assert_eq!(
            find_next(&b, "zzz", Position::ZERO, SearchOptions::EXACT),
            None
        );
        assert_eq!(
            find_previous(&b, "zzz", Position::ZERO, SearchOptions::EXACT),
            None
        );
    }

    #[test]
    fn the_word_under_the_cursor_is_found() {
        let b = buffer("let value = other;\n");
        assert_eq!(
            word_at(&b, at(0, 6)).map(|r| b.text_in_range(r)),
            Some("value".to_owned())
        );
    }

    #[test]
    fn a_caret_just_after_a_word_is_on_that_word() {
        // The common case: the caret follows what was just typed.
        let b = buffer("value\n");
        assert_eq!(
            word_at(&b, at(0, 5)).map(|r| b.text_in_range(r)),
            Some("value".to_owned())
        );
    }

    #[test]
    fn a_caret_at_the_start_of_a_word_is_on_it() {
        let b = buffer("  value\n");
        assert_eq!(
            word_at(&b, at(0, 2)).map(|r| b.text_in_range(r)),
            Some("value".to_owned())
        );
    }

    #[test]
    fn a_caret_in_whitespace_is_on_no_word() {
        // Distinct from an empty word: ctrl+d has nothing to select here, and
        // guessing the nearest word would move the user's cursor unbidden.
        let b = buffer("a    b\n");
        assert_eq!(word_at(&b, at(0, 3)), None);
    }

    #[test]
    fn a_word_containing_an_underscore_or_a_digit_is_one_word() {
        let b = buffer("my_var2 = 1\n");
        assert_eq!(
            word_at(&b, at(0, 3)).map(|r| b.text_in_range(r)),
            Some("my_var2".to_owned())
        );
    }

    #[test]
    fn a_word_after_an_emoji_reports_utf16_columns() {
        let b = buffer("🎉 value\n");
        let range = word_at(&b, at(0, 4)).expect("a word");
        assert_eq!(range.start, at(0, 3));
        assert_eq!(b.text_in_range(range), "value");
    }

    #[test]
    fn a_non_ascii_identifier_is_a_word() {
        let b = buffer("let 日本語 = 1\n");
        assert_eq!(
            word_at(&b, at(0, 5)).map(|r| b.text_in_range(r)),
            Some("日本語".to_owned())
        );
    }

    #[test]
    fn a_position_past_the_end_of_the_document_finds_no_word() {
        let b = buffer("a\n");
        assert_eq!(word_at(&b, at(99, 0)), None);
    }
}
