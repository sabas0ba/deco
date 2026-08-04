//! A JSONC (JSON with Comments) reader.
//!
//! Every configuration file VS Code owns — `settings.json`, `keybindings.json`,
//! `*.code-workspace`, theme files, `launch.json` — is JSONC: JSON plus `//`
//! and `/* */` comments and tolerated trailing commas. Reading them with a
//! plain JSON parser fails on the very first user file, so this module strips
//! the extensions before handing off to `serde_json`.
//!
//! The stripper rewrites in place, replacing every removed byte with a space
//! and preserving newlines. That keeps byte offsets identical between the
//! original text and the text `serde_json` sees, so parse errors still point at
//! the right line and column of the file the user actually wrote.

use serde_json::Value;

/// Failure to read a JSONC document.
#[derive(Debug, thiserror::Error)]
pub enum JsoncError {
    /// A comment was opened but never closed.
    #[error("unterminated block comment starting at line {line}, column {column}")]
    UnterminatedComment {
        /// One-based line of the `/*`.
        line: usize,
        /// One-based column of the `/*`.
        column: usize,
    },
    /// The document was not valid JSON once comments were removed.
    #[error("invalid JSON at line {line}, column {column}: {message}")]
    Syntax {
        /// One-based line.
        line: usize,
        /// One-based column.
        column: usize,
        /// The underlying parser message.
        message: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    String,
    StringEscape,
    LineComment,
    BlockComment,
    /// Inside a block comment having just seen `*`.
    BlockCommentStar,
}

/// Rewrites `input` into equivalent strict JSON of the same byte length.
///
/// Comments become spaces and trailing commas become spaces. Exposed because
/// tests and error reporting both benefit from seeing the intermediate form.
pub fn strip(input: &str) -> Result<String, JsoncError> {
    let bytes = input.as_bytes();
    let mut out = bytes.to_vec();
    let mut state = State::Normal;
    let mut comment_start = 0usize;
    // Byte indices of the two most recent structurally significant characters.
    // Two are needed, not one: `[1, 2,]` has a trailing comma to strip, but
    // `{"a": ,}` has a *missing value*, and blanking that comma would push the
    // resulting parse error onto the wrong line.
    let mut last_significant: Option<usize> = None;
    let mut prev_significant: Option<usize> = None;
    let mut i = 0usize;

    macro_rules! mark_significant {
        ($idx:expr) => {{
            prev_significant = last_significant;
            last_significant = Some($idx);
        }};
    }

    while i < bytes.len() {
        let b = bytes[i];
        match state {
            State::Normal => match b {
                b'"' => {
                    state = State::String;
                    mark_significant!(i);
                    i += 1;
                }
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                    state = State::LineComment;
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                }
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                    state = State::BlockComment;
                    comment_start = i;
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                }
                b'}' | b']' => {
                    // A trailing comma is legal in JSONC and fatal in JSON — but
                    // only when it actually follows a value. A comma preceded by
                    // `:`, `,`, `{` or `[` is a genuine syntax error, so it is
                    // left in place for `serde_json` to report where it is.
                    if let Some(comma) = last_significant {
                        let follows_a_value = prev_significant
                            .map(|p| !matches!(out[p], b':' | b',' | b'{' | b'['))
                            .unwrap_or(false);
                        if out[comma] == b',' && follows_a_value {
                            out[comma] = b' ';
                        }
                    }
                    mark_significant!(i);
                    i += 1;
                }
                b if b.is_ascii_whitespace() => i += 1,
                _ => {
                    mark_significant!(i);
                    i += 1;
                }
            },
            State::String => {
                match b {
                    b'\\' => state = State::StringEscape,
                    b'"' => {
                        state = State::Normal;
                        mark_significant!(i);
                    }
                    _ => {}
                }
                i += 1;
            }
            State::StringEscape => {
                state = State::String;
                i += 1;
            }
            State::LineComment => {
                if b == b'\n' {
                    state = State::Normal;
                } else {
                    // Keep \r so CRLF files keep their byte layout.
                    if b != b'\r' {
                        out[i] = b' ';
                    }
                }
                i += 1;
            }
            State::BlockComment | State::BlockCommentStar => {
                if state == State::BlockCommentStar && b == b'/' {
                    out[i] = b' ';
                    state = State::Normal;
                    i += 1;
                    continue;
                }
                state = if b == b'*' {
                    State::BlockCommentStar
                } else {
                    State::BlockComment
                };
                if b != b'\n' && b != b'\r' {
                    out[i] = b' ';
                }
                i += 1;
            }
        }
    }

    if matches!(state, State::BlockComment | State::BlockCommentStar) {
        let (line, column) = line_column(input, comment_start);
        return Err(JsoncError::UnterminatedComment { line, column });
    }

    // Every byte we rewrote was ASCII and replaced by ASCII, so the result is
    // still valid UTF-8 with the original byte offsets intact.
    Ok(String::from_utf8(out).expect("only ASCII bytes are rewritten"))
}

/// One-based line and column of `byte_idx` within `text`.
fn line_column(text: &str, byte_idx: usize) -> (usize, usize) {
    let upto = &text[..byte_idx.min(text.len())];
    let line = upto.matches('\n').count() + 1;
    let column = upto
        .rsplit('\n')
        .next()
        .map(|l| l.chars().count())
        .unwrap_or(0)
        + 1;
    (line, column)
}

/// Parses a JSONC document into a [`Value`].
///
/// A leading UTF-8 BOM is accepted: Windows editors write them and VS Code
/// tolerates them, so refusing would reject files the user considers fine.
pub fn parse(input: &str) -> Result<Value, JsoncError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let stripped = strip(input)?;
    // An empty (or comment-only) file means "no settings", not a syntax error.
    if stripped.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(&stripped).map_err(|e| JsoncError::Syntax {
        line: e.line(),
        column: e.column(),
        message: e.to_string(),
    })
}

/// Parses a JSONC document and deserializes it into `T`.
pub fn from_str<T: serde::de::DeserializeOwned>(input: &str) -> Result<T, JsoncError> {
    let value = parse(input)?;
    serde_json::from_value(value).map_err(|e| JsoncError::Syntax {
        line: 0,
        column: 0,
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_plain_json() {
        assert_eq!(parse(r#"{"a":1}"#).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn strips_line_comments() {
        let src = r#"{
            // the tab width
            "editor.tabSize": 4 // trailing
        }"#;
        assert_eq!(parse(src).unwrap(), json!({"editor.tabSize": 4}));
    }

    #[test]
    fn strips_block_comments() {
        let src = r#"{
            /* a
               multi-line
               comment */
            "a": 1, /* inline */ "b": 2
        }"#;
        assert_eq!(parse(src).unwrap(), json!({"a": 1, "b": 2}));
    }

    #[test]
    fn keeps_comment_markers_inside_strings() {
        let src = r#"{"url": "https://example.com/a", "glob": "/* not a comment */"}"#;
        assert_eq!(
            parse(src).unwrap(),
            json!({"url": "https://example.com/a", "glob": "/* not a comment */"})
        );
    }

    #[test]
    fn handles_escaped_quotes_in_strings() {
        let src = r#"{"a": "he said \"hi\" // not a comment"}"#;
        assert_eq!(
            parse(src).unwrap(),
            json!({"a": "he said \"hi\" // not a comment"})
        );
    }

    #[test]
    fn handles_escaped_backslash_before_quote() {
        // The string ends at the quote after the escaped backslash; if the
        // escape state were wrong the rest of the file would be swallowed.
        let src = r#"{"path": "C:\\", "b": 1}"#;
        assert_eq!(parse(src).unwrap(), json!({"path": "C:\\", "b": 1}));
    }

    #[test]
    fn removes_trailing_comma_in_objects() {
        assert_eq!(
            parse(r#"{"a": 1, "b": 2,}"#).unwrap(),
            json!({"a": 1, "b": 2})
        );
    }

    #[test]
    fn removes_trailing_comma_in_arrays() {
        assert_eq!(
            parse(r#"{"a": [1, 2, 3,]}"#).unwrap(),
            json!({"a": [1, 2, 3]})
        );
    }

    #[test]
    fn removes_trailing_comma_followed_by_a_comment() {
        let src = r#"{
            "a": 1,
            // why the setting exists
        }"#;
        assert_eq!(parse(src).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn removes_nested_trailing_commas() {
        let src = r#"{"a": {"b": [1,],}, "c": 2,}"#;
        assert_eq!(parse(src).unwrap(), json!({"a": {"b": [1]}, "c": 2}));
    }

    #[test]
    fn accepts_a_utf8_bom() {
        let src = "\u{feff}{\"a\": 1}";
        assert_eq!(parse(src).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn empty_and_comment_only_files_are_empty_objects() {
        assert_eq!(parse("").unwrap(), json!({}));
        assert_eq!(parse("// nothing here\n").unwrap(), json!({}));
    }

    #[test]
    fn stripping_preserves_byte_offsets() {
        let src = "{\n  // comment\n  \"a\": 1\n}";
        let stripped = strip(src).unwrap();
        assert_eq!(stripped.len(), src.len());
        assert_eq!(stripped.lines().count(), src.lines().count());
    }

    #[test]
    fn stripping_preserves_offsets_with_multibyte_comments() {
        let src = "{\n  // 日本語のコメント\n  \"a\": 1\n}";
        let stripped = strip(src).unwrap();
        assert_eq!(stripped.len(), src.len());
        assert_eq!(parse(src).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn stripping_preserves_offsets_with_crlf() {
        let src = "{\r\n  // c\r\n  \"a\": 1\r\n}";
        let stripped = strip(src).unwrap();
        assert_eq!(stripped.len(), src.len());
        assert_eq!(parse(src).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn a_comma_that_is_not_a_trailing_comma_is_left_alone() {
        // `{,}` and `[,]` are malformed, not JSONC niceties.
        assert!(parse("{,}").is_err());
        assert!(parse("[,]").is_err());
        assert!(parse(r#"{"a": 1,, }"#).is_err());
    }

    #[test]
    fn syntax_errors_report_the_original_line() {
        let src = "{\n  // comment\n  \"a\": ,\n}";
        let err = parse(src).unwrap_err();
        match err {
            JsoncError::Syntax { line, .. } => assert_eq!(line, 3),
            other => panic!("expected a syntax error, got {other:?}"),
        }
    }

    #[test]
    fn unterminated_block_comment_is_reported_with_its_position() {
        let src = "{\n  /* never closed\n  \"a\": 1\n}";
        match parse(src).unwrap_err() {
            JsoncError::UnterminatedComment { line, column } => {
                assert_eq!((line, column), (2, 3));
            }
            other => panic!("expected an unterminated comment error, got {other:?}"),
        }
    }

    #[test]
    fn block_comment_with_stars_inside_terminates_correctly() {
        let src = "{ /** doc ** style */ \"a\": 1 }";
        assert_eq!(parse(src).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn division_like_slashes_in_strings_are_untouched() {
        let src = r#"{"pattern": "**/node_modules/**"}"#;
        assert_eq!(
            parse(src).unwrap(),
            json!({"pattern": "**/node_modules/**"})
        );
    }

    #[test]
    fn deserializes_into_a_typed_struct() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Cfg {
            name: String,
            size: u32,
        }
        let cfg: Cfg = from_str("{\n // c\n \"name\": \"x\", \"size\": 3,\n}").unwrap();
        assert_eq!(
            cfg,
            Cfg {
                name: "x".into(),
                size: 3
            }
        );
    }
}
