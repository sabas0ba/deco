//! Finding text in a buffer, literally or by regular expression.
//!
//! `ctrl+d` and `ctrl+shift+l` search for the selected text exactly through
//! [`find_all`] with [`SearchOptions::EXACT`]. The find bar and search in files
//! build a [`Pattern`] from the user's query, because a regular expression can be
//! invalid and the error must be reported rather than shown as "no results".
//!
//! # Regular expressions
//!
//! With [`SearchOptions::regex`] set, the query uses the syntax of the `regex`
//! crate, which is close to the JavaScript syntax VS Code uses. Matching runs in
//! time linear in the length of the text, so a pattern cannot make a search hang.
//! The differences from VS Code are:
//!
//! - Look-around and backreferences are not supported and are reported as
//!   invalid patterns.
//! - `^` and `$` match at every line start and end, and `.` does not match a
//!   line break. A pattern containing `\n` matches across lines.
//! - A match of zero length, such as `^` or `a*` before a `b`, is skipped. An
//!   empty selection cannot be replaced or stepped through.
//!
//! In a replacement, `$1` to `$99` insert a capture group, `$0` and `$&` insert
//! the whole match, `$$` inserts `$`, and `\n`, `\t` and `\\` insert a line
//! break, a tab and a backslash. A reference to a group the pattern does not
//! have is inserted literally, as in JavaScript.
//!
//! # Positions, not byte offsets
//!
//! Every result is a [`Range`] in the same UTF-16 coordinates the rest of
//! `deco-core` uses, so a match can be turned into a selection with no
//! conversion. Internally the search walks characters, because a byte-offset
//! search would find a match starting in the middle of a multi-byte character
//! and produce a range that cannot be a valid position.
//!
//! # Word boundaries and a difference from VS Code
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
//! VS Code implements the option as the regex `\bneedle\b`. The result is the
//! same for any needle that begins and ends with a word character, which covers
//! typical use of the option. It differs for a needle like `(`: `\b(` requires a
//! *transition*, so VS Code finds the bracket in `f(x)` and not the one in
//! ` ( `. This is a side effect of `\b`. deco does not reproduce it, because
//! "whole word" would then exclude results for a needle that contains no word
//! characters.
//!
//! With a regular expression, the same rule applies to the ends of each match:
//! an end of the matched text that is a word character must not be next to
//! another word character.

use crate::position::{Position, Range};
use crate::Buffer;

/// How to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchOptions {
    /// Whether `Foo` and `foo` are different.
    pub case_sensitive: bool,
    /// Whether a match must be bounded by non-word characters.
    pub whole_word: bool,
    /// Whether the needle is a regular expression rather than literal text.
    pub regex: bool,
}

impl SearchOptions {
    /// Case-sensitive, literal, matching anywhere.
    ///
    /// Used by `ctrl+d`: the user selected exactly this text, so a match with
    /// different case is not expected.
    pub const EXACT: Self = Self {
        case_sensitive: true,
        whole_word: false,
        regex: false,
    };
}

/// A query that could not be compiled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid regular expression: {reason}")]
pub struct PatternError {
    /// The parser's description, including the position of the error.
    pub reason: String,
}

/// A query compiled once and run against any number of buffers.
///
/// Search in files runs one pattern over many files, so the regular expression
/// is compiled here rather than once per file.
#[derive(Debug, Clone)]
pub struct Pattern {
    needle: String,
    options: SearchOptions,
    regex: Option<regex::Regex>,
}

/// Upper bound on the compiled size of a user's regular expression.
///
/// The `regex` crate's own default is 10 MiB. A bounded repetition such as
/// `\w{1000}` over Unicode classes can exceed it; the error is then reported
/// as an invalid pattern.
const REGEX_SIZE_LIMIT: usize = 10 * (1 << 20);

impl Pattern {
    /// Compiles `needle` according to `options`.
    ///
    /// A literal needle always compiles. A regular expression can fail; the
    /// error names the problem and its position.
    pub fn new(needle: &str, options: SearchOptions) -> Result<Self, PatternError> {
        let regex = if options.regex && !needle.is_empty() {
            let compiled = regex::RegexBuilder::new(needle)
                .case_insensitive(!options.case_sensitive)
                .multi_line(true)
                .size_limit(REGEX_SIZE_LIMIT)
                .build()
                .map_err(|error| PatternError {
                    reason: error.to_string(),
                })?;
            Some(compiled)
        } else {
            None
        };
        Ok(Self {
            needle: needle.to_owned(),
            options,
            regex,
        })
    }

    /// The options this pattern was compiled with.
    pub fn options(&self) -> SearchOptions {
        self.options
    }

    /// Every match in `buffer`, in document order.
    pub fn find_all(&self, buffer: &Buffer) -> Vec<Range> {
        match &self.regex {
            Some(regex) => regex_matches(buffer, &buffer.text(), regex, self.options.whole_word)
                .into_iter()
                .map(|found| found.range)
                .collect(),
            None => find_literal(buffer, &self.needle, self.options),
        }
    }

    /// Every match in `buffer` with the text that replaces it.
    ///
    /// For a literal pattern the replacement is `template` unchanged. For a
    /// regular expression, capture references in `template` are expanded
    /// against each match; see the module docs for the syntax.
    pub fn replacements(&self, buffer: &Buffer, template: &str) -> Vec<(Range, String)> {
        match &self.regex {
            Some(regex) => {
                let text = buffer.text();
                regex_matches(buffer, &text, regex, self.options.whole_word)
                    .into_iter()
                    .map(|found| {
                        let captures = regex.captures_at(&text, found.start).filter(|captures| {
                            captures.get(0).is_some_and(|whole| {
                                whole.start() == found.start && whole.end() == found.end
                            })
                        });
                        let expanded = match captures {
                            Some(captures) => expand(template, &captures),
                            // Unreachable for a match `regex_matches` returned, but
                            // an unexpanded template is a safe result.
                            None => template.to_owned(),
                        };
                        (found.range, expanded)
                    })
                    .collect()
            }
            None => self
                .find_all(buffer)
                .into_iter()
                .map(|range| (range, template.to_owned()))
                .collect(),
        }
    }
}

/// `text` with every regular-expression metacharacter escaped.
///
/// Used to seed a regex-mode find bar from a selection, so the seed matches the
/// selected text literally.
pub fn escape(text: &str) -> String {
    regex::escape(text)
}

/// Every match of `needle` in `buffer`, in document order.
///
/// An empty needle matches nothing. Otherwise `ctrl+shift+l` on an empty
/// selection would put a cursor on every character in the file. An invalid
/// regular expression also matches nothing; callers that must report the error
/// use [`Pattern::new`].
pub fn find_all(buffer: &Buffer, needle: &str, options: SearchOptions) -> Vec<Range> {
    Pattern::new(needle, options)
        .map(|pattern| pattern.find_all(buffer))
        .unwrap_or_default()
}

/// A regular-expression match, as a position range and as byte offsets into
/// [`Buffer::text`].
struct RegexMatch {
    range: Range,
    start: usize,
    end: usize,
}

/// The non-empty matches of `regex` in `buffer` that satisfy `whole_word`.
///
/// `text` is `buffer.text()`, passed in so a caller that also needs it reads the
/// buffer once.
fn regex_matches(
    buffer: &Buffer,
    text: &str,
    regex: &regex::Regex,
    whole_word: bool,
) -> Vec<RegexMatch> {
    regex
        .find_iter(text)
        .filter(|found| !found.is_empty())
        .filter(|found| !whole_word || is_whole_word_bytes(text, found.start(), found.end()))
        .map(|found| RegexMatch {
            range: Range::new(
                buffer.byte_to_position(found.start()),
                buffer.byte_to_position(found.end()),
            ),
            start: found.start(),
            end: found.end(),
        })
        .collect()
}

/// [`is_whole_word`] for a match given as byte offsets into `text`.
///
/// The matched text itself stands in for the needle, because a regular
/// expression's ends are only known once it has matched.
fn is_whole_word_bytes(text: &str, start: usize, end: usize) -> bool {
    let matched = &text[start..end];
    let starts_with_word = matched.chars().next().is_some_and(is_word_char);
    let ends_with_word = matched.chars().next_back().is_some_and(is_word_char);
    let before_ok =
        !starts_with_word || !text[..start].chars().next_back().is_some_and(is_word_char);
    let after_ok = !ends_with_word || !text[end..].chars().next().is_some_and(is_word_char);
    before_ok && after_ok
}

/// Expands capture references in a replacement template.
fn expand(template: &str, captures: &regex::Captures<'_>) -> String {
    let group = |index: usize| captures.get(index).map_or("", |found| found.as_str());
    let groups = captures.len() - 1;
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let next = chars.get(index + 1).copied();
        match (c, next) {
            ('$', Some('$')) => {
                out.push('$');
                index += 2;
            }
            ('$', Some('&')) => {
                out.push_str(group(0));
                index += 2;
            }
            ('$', Some(first)) if first.is_ascii_digit() => {
                let one = first.to_digit(10).unwrap_or(0) as usize;
                let two = chars
                    .get(index + 2)
                    .and_then(|second| second.to_digit(10))
                    .map(|second| one * 10 + second as usize);
                // Two digits win when that group exists, as in JavaScript:
                // `$12` is group 12 if there is one, otherwise group 1 and `2`.
                if let Some(two) = two.filter(|two| (1..=groups).contains(two)) {
                    out.push_str(group(two));
                    index += 3;
                } else if one <= groups {
                    out.push_str(group(one));
                    index += 2;
                } else {
                    out.push('$');
                    index += 1;
                }
            }
            ('\\', Some('n')) => {
                out.push('\n');
                index += 2;
            }
            ('\\', Some('t')) => {
                out.push('\t');
                index += 2;
            }
            ('\\', Some('\\')) => {
                out.push('\\');
                index += 2;
            }
            _ => {
                out.push(c);
                index += 1;
            }
        }
    }
    out
}

/// Every match of a literal `needle`, in document order.
fn find_literal(buffer: &Buffer, needle: &str, options: SearchOptions) -> Vec<Range> {
    if needle.is_empty() {
        return Vec::new();
    }

    // Both sides are folded once, not per comparison. `to_lowercase` is used
    // instead of `to_ascii_lowercase` so that non-ASCII text such as `Straße`
    // and `STRASSE` is folded by Unicode rules. Unicode case folding can change
    // length, so the *positions* below come from the haystack's own characters,
    // not from the folded copy.
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

    // Case folding that changes length would misalign the two copies and
    // produce ranges in the wrong place, which is worse than a missed match. In
    // that case, fall back to a case-sensitive search.
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
/// With wrapping, pressing `ctrl+d` after the last occurrence returns to the
/// first occurrence instead of doing nothing.
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

    // The column is in UTF-16 units and the scan is in characters. The two
    // differ on any line containing an emoji, so convert between them.
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
        // Otherwise ctrl+shift+l on an empty selection would put a cursor on
        // every character in the file.
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
                    whole_word: false,
                    regex: false,
                }
            )
            .len(),
            3
        );
    }

    #[test]
    fn a_case_insensitive_match_reports_the_haystacks_own_positions() {
        // The folded copy is used only for comparison. A position taken from it
        // would be wrong whenever folding changed a length.
        let b = buffer("xxFOOxx");
        let matches = find_all(
            &b,
            "foo",
            SearchOptions {
                case_sensitive: false,
                whole_word: false,
                regex: false,
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
            regex: false,
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
            regex: false,
        };
        assert_eq!(find_all(&b, "foo", options).len(), 1);
    }

    #[test]
    fn whole_word_does_not_constrain_a_needle_with_no_word_characters() {
        // A bracket has no word-character end, so every bracket matches. VS
        // Code differs here: it implements the option as `\bneedle\b` and finds
        // the bracket in `f(x)` but not the one in ` ( `. See the module docs.
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: true,
            regex: false,
        };
        assert_eq!(find_all(&buffer("f(x) g(y)"), "(", options).len(), 2);
        assert_eq!(find_all(&buffer(" ( ) "), "(", options).len(), 1);
    }

    #[test]
    fn whole_word_constrains_only_the_ends_that_are_word_characters() {
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: true,
            regex: false,
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
        // Used by ctrl+d: after the last occurrence, return to the first.
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
        // Finding the match at the cursor and finding the next match are
        // separate calls, not one call with an offset of one.
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
        // Distinct from an empty word: ctrl+d has nothing to select here.
        // Selecting the nearest word would move the cursor unexpectedly.
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

    const REGEX: SearchOptions = SearchOptions {
        case_sensitive: true,
        whole_word: false,
        regex: true,
    };

    fn pattern(needle: &str, options: SearchOptions) -> Pattern {
        Pattern::new(needle, options).expect("a valid pattern")
    }

    #[test]
    fn a_regex_finds_every_match_in_document_order() {
        let b = buffer("let a1 = 1;\nlet b22 = 2;\n");
        assert_eq!(
            ranges(&pattern(r"[a-z]\d+", REGEX).find_all(&b)),
            vec![(0, 4, 0, 6), (1, 4, 1, 7)]
        );
    }

    #[test]
    fn regex_metacharacters_are_literal_without_regex_mode() {
        let b = buffer("a.c abc\n");
        assert_eq!(find_all(&b, "a.c", SearchOptions::EXACT).len(), 1);
        assert_eq!(pattern("a.c", REGEX).find_all(&b).len(), 2);
    }

    #[test]
    fn an_invalid_regex_is_an_error_naming_the_problem() {
        let error = Pattern::new("(foo", REGEX).expect_err("unclosed group");
        assert!(error.to_string().starts_with("invalid regular expression"));
        assert!(error.reason.contains("unclosed"), "{}", error.reason);
        assert!(find_all(&buffer("(foo"), "(foo", REGEX).is_empty());
    }

    #[test]
    fn look_around_is_reported_as_unsupported() {
        assert!(Pattern::new("foo(?=bar)", REGEX).is_err());
    }

    #[test]
    fn anchors_match_at_every_line() {
        let b = buffer("one\ntwo\n");
        assert_eq!(
            ranges(&pattern("^t|e$", REGEX).find_all(&b)),
            vec![(0, 2, 0, 3), (1, 0, 1, 1)]
        );
    }

    #[test]
    fn a_regex_with_a_line_break_matches_across_lines() {
        let b = buffer("one\ntwo\n");
        assert_eq!(
            ranges(&pattern(r"e\nt", REGEX).find_all(&b)),
            vec![(0, 2, 1, 1)]
        );
    }

    #[test]
    fn empty_matches_are_skipped() {
        let b = buffer("abc\n\nb\n");
        assert!(pattern("^", REGEX).find_all(&b).is_empty());
        assert_eq!(
            ranges(&pattern("a*", REGEX).find_all(&b)),
            vec![(0, 0, 0, 1)]
        );
    }

    #[test]
    fn a_case_insensitive_regex_ignores_case() {
        let options = SearchOptions {
            case_sensitive: false,
            ..REGEX
        };
        assert_eq!(
            pattern("fo+", options).find_all(&buffer("FOO foo")).len(),
            2
        );
        assert_eq!(pattern("fo+", REGEX).find_all(&buffer("FOO foo")).len(), 1);
    }

    #[test]
    fn whole_word_applies_to_the_ends_of_each_regex_match() {
        let options = SearchOptions {
            whole_word: true,
            ..REGEX
        };
        let b = buffer("foo1 xfoo2 foo3y (foo4)\n");
        assert_eq!(
            ranges(&pattern(r"foo\d", options).find_all(&b)),
            vec![(0, 0, 0, 4), (0, 18, 0, 22)]
        );
        // A match whose ends are not word characters is not constrained.
        assert_eq!(pattern(r"\(", options).find_all(&b).len(), 1);
    }

    #[test]
    fn a_regex_match_after_non_ascii_text_reports_utf16_columns() {
        let b = buffer("🎉日本 foo\n");
        assert_eq!(
            ranges(&pattern("f.o", REGEX).find_all(&b)),
            vec![(0, 5, 0, 8)]
        );
    }

    fn replaced(text: &str, needle: &str, template: &str) -> Vec<String> {
        pattern(needle, REGEX)
            .replacements(&buffer(text), template)
            .into_iter()
            .map(|(_, replacement)| replacement)
            .collect()
    }

    #[test]
    fn capture_groups_are_expanded_in_a_replacement() {
        assert_eq!(
            replaced("key=value\n", r"(\w+)=(\w+)", "$2: $1"),
            vec!["value: key"]
        );
    }

    #[test]
    fn the_whole_match_and_a_dollar_sign_can_be_inserted() {
        assert_eq!(replaced("ab\n", "ab", "[$&|$0|$$]"), vec!["[ab|ab|$]"]);
    }

    #[test]
    fn a_missing_group_is_inserted_literally_and_an_unmatched_one_is_empty() {
        assert_eq!(replaced("ab\n", "(a)(x)?b", "$3-$2-$1"), vec!["$3--a"]);
    }

    #[test]
    fn two_digit_references_fall_back_to_one_digit() {
        // With one group, `$12` is group 1 followed by `2`.
        assert_eq!(replaced("ab\n", "(a)b", "$12"), vec!["a2"]);
        let twelve = "(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)(l)";
        assert_eq!(replaced("abcdefghijkl\n", twelve, "$12"), vec!["l"]);
    }

    #[test]
    fn escapes_in_a_replacement_insert_control_characters() {
        assert_eq!(replaced("a b\n", " ", r"\n\t\\\x"), vec!["\n\t\\\\x"]);
    }

    #[test]
    fn a_literal_replacement_is_not_expanded() {
        let literal = pattern("a", SearchOptions::EXACT);
        let replacements = literal.replacements(&buffer("a\n"), "$0\\n");
        assert_eq!(replacements[0].1, "$0\\n");
    }

    #[test]
    fn each_replacement_uses_its_own_match() {
        assert_eq!(replaced("x1 y2\n", r"(\w)(\d)", "$2$1"), vec!["1x", "2y"]);
    }

    #[test]
    fn an_escaped_seed_matches_itself_literally() {
        let seed = "a.b(c)*";
        let b = buffer("a.b(c)* axb(c)\n");
        assert_eq!(
            ranges(&pattern(&escape(seed), REGEX).find_all(&b)),
            vec![(0, 0, 0, 7)]
        );
    }
}
