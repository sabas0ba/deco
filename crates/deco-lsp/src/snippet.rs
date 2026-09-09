//! The supported, flat numeric subset of LSP snippets.

use deco_core::{Position, Range};

/// Expanded text and its ordered, non-overlapping tab stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// Text to insert; all ranges are relative to its beginning, in UTF-16.
    pub text: String,
    /// Numeric order, with the final stop last (implicit at EOF when absent).
    pub stops: Vec<Range>,
}

impl Snippet {
    /// Parses unique `$1`, `${1}`, `${1:default}` and `$0` placeholders.
    ///
    /// Unsupported syntax returns `None` before any document edit. In particular,
    /// repeated indices require linked editing, which this subset cannot provide.
    pub fn parse(source: &str) -> Option<Self> {
        if source.chars().any(|c| {
            matches!(
                c,
                '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        }) {
            return None;
        }
        let mut chars = source.chars().peekable();
        let mut text = String::new();
        let mut position = Position::ZERO;
        let mut stops = std::collections::BTreeMap::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    let escaped = chars.next()?;
                    if !matches!(escaped, '$' | '}' | '\\') {
                        return None;
                    }
                    append(&mut text, &mut position, escaped);
                }
                '$' => {
                    let braced = chars.peek() == Some(&'{');
                    if braced {
                        chars.next();
                    }
                    let mut digits = String::new();
                    while chars.peek().is_some_and(char::is_ascii_digit) {
                        digits.push(chars.next()?);
                    }
                    let index: u32 = digits.parse().ok()?;
                    if stops.contains_key(&index) {
                        return None;
                    }
                    let start = position;
                    if braced {
                        match chars.next()? {
                            '}' => {}
                            ':' if index != 0 => loop {
                                match chars.next()? {
                                    '}' => break,
                                    '$' | '{' => return None,
                                    '\\' => {
                                        let escaped = chars.next()?;
                                        if !matches!(escaped, '$' | '}' | '\\') {
                                            return None;
                                        }
                                        append(&mut text, &mut position, escaped);
                                    }
                                    c => append(&mut text, &mut position, c),
                                }
                            },
                            _ => return None,
                        }
                    }
                    stops.insert(index, Range::new(start, position));
                }
                c => append(&mut text, &mut position, c),
            }
        }
        if stops.is_empty() {
            return None;
        }
        let final_stop = stops.remove(&0).unwrap_or(Range::empty(position));
        let mut ordered: Vec<_> = stops.into_values().collect();
        ordered.push(final_stop);
        // Coincident empty fields have ambiguous insertion affinity. Leave such
        // snippets to the fallback until linked/nested stops are implemented.
        let mut starts = std::collections::BTreeSet::new();
        if ordered.iter().any(|range| !starts.insert(range.start)) {
            return None;
        }
        Some(Self {
            text,
            stops: ordered,
        })
    }
}

fn append(text: &mut String, position: &mut Position, c: char) {
    text.push(c);
    if c == '\n' {
        position.line += 1;
        position.character = 0;
    } else {
        position.character += c.len_utf16() as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_numbers_and_places_zero_last() {
        let snippet = Snippet::parse("${2:b} ${1:a}$0").unwrap();
        assert_eq!(snippet.text, "b a");
        assert_eq!(
            snippet.stops[0],
            Range::new(Position::new(0, 2), Position::new(0, 3))
        );
        assert_eq!(
            snippet.stops[1],
            Range::new(Position::ZERO, Position::new(0, 1))
        );
        assert_eq!(snippet.stops[2], Range::empty(Position::new(0, 3)));
    }

    #[test]
    fn counts_utf16_and_multiline_defaults() {
        let snippet = Snippet::parse("😀${1:日本\n語}\\$\\}\\\\").unwrap();
        assert_eq!(snippet.text, "😀日本\n語$}\\");
        assert_eq!(
            snippet.stops[0],
            Range::new(Position::new(0, 2), Position::new(1, 1))
        );
        assert_eq!(snippet.stops[1], Range::empty(Position::new(1, 4)));
    }

    #[test]
    fn refuses_unsupported_or_malformed_syntax() {
        for text in [
            "${1:x} $1",
            "${1:${2:x}}",
            "${TM_FILENAME}",
            "${1|a,b|}",
            "${1/x/y/}",
            "${1:open",
            "$9999999999999999",
            "$1$2",
            "${0:text}",
        ] {
            assert!(Snippet::parse(text).is_none(), "{text}");
        }
    }

    #[test]
    fn rejects_non_lf_line_separators_before_producing_ranges() {
        for c in ['\r', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}'] {
            assert!(Snippet::parse(&format!("${{1:a{c}b}}!")).is_none());
        }
    }
}
