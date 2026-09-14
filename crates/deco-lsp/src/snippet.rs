//! Flat numeric fields and explicitly resolved variables in LSP snippets.

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
        Self::parse_with_variables(source, |_| None)
    }

    /// Expands variables using values from the insertion context.
    ///
    /// `None` from the resolver means unsupported; `Some("")` means supported
    /// but unset, so a literal default is used when present. Resolved values
    /// are inserted as text and are never parsed as snippet syntax.
    pub fn parse_with_variables(
        source: &str,
        mut resolve: impl FnMut(&str) -> Option<String>,
    ) -> Option<Self> {
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
        let mut has_variable = false;
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
                    if chars
                        .peek()
                        .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
                    {
                        let mut name = String::new();
                        while chars
                            .peek()
                            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                        {
                            name.push(chars.next()?);
                        }
                        let mut value = resolve(&name)?;
                        if braced {
                            match chars.next()? {
                                '}' => {}
                                ':' => {
                                    let default = literal_default(&mut chars)?;
                                    if value.is_empty() {
                                        value = default;
                                    }
                                }
                                _ => return None,
                            }
                        }
                        for c in value.chars() {
                            if matches!(
                                c,
                                '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
                            ) {
                                return None;
                            }
                            append(&mut text, &mut position, c);
                        }
                        has_variable = true;
                        continue;
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
                            ':' if index != 0 => {
                                for c in literal_default(&mut chars)?.chars() {
                                    append(&mut text, &mut position, c);
                                }
                            }
                            _ => return None,
                        }
                    }
                    stops.insert(index, Range::new(start, position));
                }
                c => append(&mut text, &mut position, c),
            }
        }
        if stops.is_empty() && !has_variable {
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

/// Reads a literal default, including the closing brace. Nested syntax is not
/// supported, even when the resolver supplies a value and the default is unused.
fn literal_default(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    let mut text = String::new();
    loop {
        match chars.next()? {
            '}' => return Some(text),
            '$' | '{' => return None,
            '\\' => {
                let escaped = chars.next()?;
                if !matches!(escaped, '$' | '}' | '\\') {
                    return None;
                }
                text.push(escaped);
            }
            c => text.push(c),
        }
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

    #[test]
    fn variables_are_literal_and_tab_stops_follow_their_utf16_length() {
        let snippet =
            Snippet::parse_with_variables("$TM_SELECTED_TEXT ${1:arg} ${TM_FILENAME}$0", |name| {
                match name {
                    "TM_SELECTED_TEXT" => Some("😀\n$1".to_owned()),
                    "TM_FILENAME" => Some("日本.rs".to_owned()),
                    _ => None,
                }
            })
            .unwrap();
        assert_eq!(snippet.text, "😀\n$1 arg 日本.rs");
        assert_eq!(
            snippet.stops,
            vec![
                Range::new(Position::new(1, 3), Position::new(1, 6)),
                Range::empty(Position::new(1, 12)),
            ]
        );
    }

    #[test]
    fn unset_variables_use_literal_defaults_and_can_repeat() {
        let snippet = Snippet::parse_with_variables(
            "${TM_FILENAME:untitled} $TM_FILENAME ${TM_FILENAME:\\$file\\}}",
            |_| Some(String::new()),
        )
        .unwrap();
        assert_eq!(snippet.text, "untitled  $file}");
        assert_eq!(snippet.stops, vec![Range::empty(Position::new(0, 16))]);
        let snippet =
            Snippet::parse_with_variables("${TM_FILENAME:unused}", |_| Some("main.rs".to_owned()))
                .unwrap();
        assert_eq!(snippet.text, "main.rs");
    }

    #[test]
    fn unsupported_variables_and_syntax_do_not_produce_partial_expansions() {
        for source in [
            "${1:ok} $UNKNOWN",
            "${UNKNOWN:default}",
            "${TM_FILENAME/${1}/x/}",
            "${TM_FILENAME:${1:nested}}",
            "${TM_FILENAME:unclosed",
        ] {
            assert!(
                Snippet::parse_with_variables(source, |name| {
                    (name == "TM_FILENAME").then(|| "main.rs".to_owned())
                })
                .is_none(),
                "{source}"
            );
        }
        for value in ["a\r\nb", "a\u{2028}b"] {
            assert!(Snippet::parse_with_variables("$TM_SELECTED_TEXT", |_| {
                Some(value.to_owned())
            })
            .is_none());
        }
    }
}
