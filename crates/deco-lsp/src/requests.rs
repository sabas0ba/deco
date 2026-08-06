//! Building language-feature requests and reading their answers.
//!
//! The protocol's request *parameters* are dull — a URI and a position — but its
//! *results* are among the most polymorphic shapes in the specification, because
//! each one accumulated alternatives across versions and servers implement
//! whichever they were written against. A client that handles only the newest
//! spelling silently loses the feature against half the servers in use.
//!
//! `textDocument/hover` can answer with any of:
//!
//! ```jsonc
//! { "contents": { "kind": "markdown", "value": "…" } }   // MarkupContent
//! { "contents": "plain string" }                          // MarkedString
//! { "contents": { "language": "rust", "value": "…" } }    // MarkedString, object form
//! { "contents": [ "a", { "language": "rust", "value": "b" } ] }  // an array of either
//! null                                                    // nothing here
//! ```
//!
//! `textDocument/definition` can answer with a single `Location`, an array of
//! them, an array of `LocationLink` (which spells its range `targetRange` and its
//! URI `targetUri`), or `null`.
//!
//! Every one of those is accepted here, and every one has a test. `null` is a
//! successful answer meaning "nothing at this position", not an error.

use deco_core::position::{Position, Range};

use crate::uri::Uri;

/// The `{ textDocument: { uri }, position }` shape every positional request takes.
pub fn text_document_position(uri: &Uri, position: Position) -> serde_json::Value {
    serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": position.line, "character": position.character },
    })
}

/// Parameters for `textDocument/references`, which needs one field more.
pub fn reference_params(
    uri: &Uri,
    position: Position,
    include_declaration: bool,
) -> serde_json::Value {
    let mut params = text_document_position(uri, position);
    params["context"] = serde_json::json!({ "includeDeclaration": include_declaration });
    params
}

/// What a server said about a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    /// The text to show, already flattened to plain lines.
    pub contents: String,
    /// The range the hover describes, if the server said. Used to decide whether
    /// a cached hover still applies after the cursor moves.
    pub range: Option<Range>,
}

impl Hover {
    /// Reads a `textDocument/hover` result.
    ///
    /// `None` for `null`, for a missing `contents`, and for contents that
    /// flatten to nothing — an empty hover box under the cursor is worse than
    /// no hover at all.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let contents = flatten_contents(value.get("contents")?);
        if contents.trim().is_empty() {
            return None;
        }
        Some(Self {
            contents,
            range: value.get("range").and_then(read_range),
        })
    }

    /// The first line, for a status bar that has one row.
    pub fn summary(&self) -> &str {
        self.contents.lines().next().unwrap_or_default()
    }
}

/// Flattens every spelling of `contents` into plain text.
fn flatten_contents(value: &serde_json::Value) -> String {
    match value {
        // A bare string is the oldest `MarkedString` form.
        serde_json::Value::String(text) => strip_markup(text),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(flatten_contents)
                .filter(|part| !part.trim().is_empty())
                .collect();
            parts.join("\n")
        }
        serde_json::Value::Object(map) => {
            // `MarkupContent` and the object form of `MarkedString` both carry
            // `value`; only the first has `kind`. Either way the text is there.
            match map.get("value").and_then(|v| v.as_str()) {
                Some(text) => strip_markup(text),
                None => String::new(),
            }
        }
        _ => String::new(),
    }
}

/// Reduces Markdown to something readable in a terminal.
///
/// The client asks for `plaintext` in its capabilities, but servers send
/// Markdown regardless — it is the only format several of them produce. Fenced
/// code blocks are the common case and their fences are pure noise once the text
/// is displayed unstyled, so they go; the rest is left alone rather than
/// half-rendered, because a mangled signature is worse than a literal asterisk.
fn strip_markup(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            continue;
        }
        // A horizontal rule separates a signature from its documentation in
        // rust-analyzer's output. It renders as a stray run of dashes.
        if matches!(trimmed.trim(), "---" | "***" | "___") {
            continue;
        }
        out.push(trimmed);
    }
    // Collapse the blank runs the removed fences leave behind.
    let mut collapsed: Vec<&str> = Vec::new();
    for line in out {
        if line.is_empty() && collapsed.last().is_some_and(|last: &&str| last.is_empty()) {
            continue;
        }
        collapsed.push(line);
    }
    collapsed.join("\n").trim().to_owned()
}

/// A place in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Which document.
    pub uri: Uri,
    /// Where in it.
    pub range: Range,
}

impl Location {
    /// Reads every spelling of a location result into a flat list.
    ///
    /// Accepts a single `Location`, an array of them, an array of
    /// `LocationLink`, and `null`. An empty list means "nothing here", which is
    /// a successful answer.
    pub fn list_from_json(value: &serde_json::Value) -> Vec<Self> {
        match value {
            serde_json::Value::Array(items) => items.iter().filter_map(Self::one).collect(),
            serde_json::Value::Object(_) => Self::one(value).into_iter().collect(),
            // Including `null`, which is how a server says there is no
            // definition at this position.
            _ => Vec::new(),
        }
    }

    fn one(value: &serde_json::Value) -> Option<Self> {
        // `LocationLink` first: it also has no plain `uri`, so checking for
        // `targetUri` up front avoids reading a link as a malformed location.
        if let (Some(uri), Some(range)) = (
            value.get("targetUri").and_then(|v| v.as_str()),
            value
                // `targetSelectionRange` is the identifier itself, which is
                // where a user expects the cursor to land; `targetRange` is the
                // whole definition, which would put it on the doc comment.
                .get("targetSelectionRange")
                .or_else(|| value.get("targetRange"))
                .and_then(read_range),
        ) {
            return Some(Self {
                uri: Uri::from_string(uri),
                range,
            });
        }

        Some(Self {
            uri: Uri::from_string(value.get("uri")?.as_str()?),
            range: value.get("range").and_then(read_range)?,
        })
    }
}

/// Reads a `{ start, end }` range, clamping rather than rejecting.
///
/// A server that miscounts should cost one misplaced jump, not the feature.
fn read_range(value: &serde_json::Value) -> Option<Range> {
    Some(Range::new(
        read_position(value.get("start")?),
        read_position(value.get("end")?),
    ))
}

fn read_position(value: &serde_json::Value) -> Position {
    let read = |field: &str| {
        value
            .get(field)
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .clamp(0, u32::MAX as i64) as u32
    };
    Position::new(read("line"), read("character"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uri() -> Uri {
        Uri::from_string("file:///w/a.rs")
    }

    #[test]
    fn positional_params_use_the_protocols_shape() {
        assert_eq!(
            text_document_position(&uri(), Position::new(3, 7)),
            json!({
                "textDocument": {"uri": "file:///w/a.rs"},
                "position": {"line": 3, "character": 7},
            })
        );
    }

    #[test]
    fn reference_params_carry_the_context() {
        let params = reference_params(&uri(), Position::ZERO, true);
        assert_eq!(params["context"]["includeDeclaration"], json!(true));
        assert_eq!(params["textDocument"]["uri"], json!("file:///w/a.rs"));
    }

    #[test]
    fn hover_reads_markup_content() {
        let hover = Hover::from_json(&json!({
            "contents": {"kind": "markdown", "value": "fn main()"},
            "range": {
                "start": {"line": 1, "character": 2},
                "end": {"line": 1, "character": 6},
            },
        }))
        .expect("a hover");
        assert_eq!(hover.contents, "fn main()");
        assert_eq!(
            hover.range,
            Some(Range::new(Position::new(1, 2), Position::new(1, 6)))
        );
    }

    #[test]
    fn hover_reads_a_bare_string() {
        // The oldest `MarkedString` form, still emitted by several servers.
        let hover = Hover::from_json(&json!({"contents": "just text"})).expect("a hover");
        assert_eq!(hover.contents, "just text");
        assert_eq!(hover.range, None);
    }

    #[test]
    fn hover_reads_the_object_form_of_marked_string() {
        let hover =
            Hover::from_json(&json!({"contents": {"language": "rust", "value": "let x: u8"}}))
                .expect("a hover");
        assert_eq!(hover.contents, "let x: u8");
    }

    #[test]
    fn hover_reads_an_array_of_mixed_forms() {
        // Handling only the newest spelling loses hover against a large share of
        // the servers people actually run.
        let hover = Hover::from_json(&json!({
            "contents": [
                "first",
                {"language": "rust", "value": "second"},
                {"kind": "plaintext", "value": "third"},
            ]
        }))
        .expect("a hover");
        assert_eq!(hover.contents, "first\nsecond\nthird");
    }

    #[test]
    fn a_null_hover_is_nothing_rather_than_an_error() {
        assert_eq!(Hover::from_json(&json!(null)), None);
        assert_eq!(Hover::from_json(&json!({})), None);
    }

    #[test]
    fn a_hover_that_flattens_to_nothing_is_not_shown() {
        // An empty box under the cursor is worse than no box.
        for contents in [json!(""), json!("   \n  "), json!([]), json!(["", "  "])] {
            assert_eq!(
                Hover::from_json(&json!({"contents": contents.clone()})),
                None,
                "{contents} should produce no hover"
            );
        }
    }

    #[test]
    fn code_fences_are_removed_but_the_code_is_kept() {
        // Rendered unstyled, a fence is pure noise; the signature inside it is
        // the entire point of the hover.
        let hover = Hover::from_json(&json!({
            "contents": {
                "kind": "markdown",
                "value": "```rust\nfn main()\n```\n\n---\n\nThe entry point.",
            }
        }))
        .expect("a hover");
        assert_eq!(hover.contents, "fn main()\n\nThe entry point.");
        assert_eq!(hover.summary(), "fn main()");
    }

    #[test]
    fn other_markdown_is_left_alone_rather_than_half_rendered() {
        // A mangled signature is worse than a literal asterisk.
        let hover = Hover::from_json(&json!({
            "contents": {"kind": "markdown", "value": "see *`Vec<T>`* for details"}
        }))
        .expect("a hover");
        assert_eq!(hover.contents, "see *`Vec<T>`* for details");
    }

    #[test]
    fn blank_runs_left_by_removed_fences_are_collapsed() {
        let hover = Hover::from_json(&json!({
            "contents": "```\na\n```\n\n\n\n```\nb\n```"
        }))
        .expect("a hover");
        assert_eq!(hover.contents, "a\n\nb");
    }

    #[test]
    fn a_definition_reads_a_single_location() {
        let locations = Location::list_from_json(&json!({
            "uri": "file:///w/b.rs",
            "range": {
                "start": {"line": 10, "character": 4},
                "end": {"line": 10, "character": 8},
            },
        }));
        assert_eq!(
            locations,
            vec![Location {
                uri: Uri::from_string("file:///w/b.rs"),
                range: Range::new(Position::new(10, 4), Position::new(10, 8)),
            }]
        );
    }

    #[test]
    fn a_definition_reads_an_array_of_locations() {
        let locations = Location::list_from_json(&json!([
            {"uri": "file:///w/a.rs", "range": {"start": {"line": 1, "character": 0},
                                                "end": {"line": 1, "character": 1}}},
            {"uri": "file:///w/b.rs", "range": {"start": {"line": 2, "character": 0},
                                                "end": {"line": 2, "character": 1}}},
        ]));
        assert_eq!(locations.len(), 2);
        assert_eq!(locations[1].uri.as_str(), "file:///w/b.rs");
    }

    #[test]
    fn a_definition_reads_location_links() {
        // A different spelling entirely — `targetUri` and `targetRange` — and
        // the one rust-analyzer and gopls both use.
        let locations = Location::list_from_json(&json!([{
            "originSelectionRange": {"start": {"line": 0, "character": 0},
                                     "end": {"line": 0, "character": 3}},
            "targetUri": "file:///w/b.rs",
            "targetRange": {"start": {"line": 5, "character": 0},
                            "end": {"line": 9, "character": 1}},
            "targetSelectionRange": {"start": {"line": 6, "character": 3},
                                     "end": {"line": 6, "character": 7}},
        }]));

        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].uri.as_str(), "file:///w/b.rs");
        assert_eq!(
            locations[0].range.start,
            Position::new(6, 3),
            "the cursor belongs on the identifier, not on the doc comment above it"
        );
    }

    #[test]
    fn a_location_link_without_a_selection_range_falls_back_to_the_target() {
        let locations = Location::list_from_json(&json!([{
            "targetUri": "file:///w/b.rs",
            "targetRange": {"start": {"line": 5, "character": 2},
                            "end": {"line": 9, "character": 1}},
        }]));
        assert_eq!(locations[0].range.start, Position::new(5, 2));
    }

    #[test]
    fn a_null_definition_is_an_empty_list() {
        // A successful answer meaning "nothing here", not a failure.
        assert!(Location::list_from_json(&json!(null)).is_empty());
        assert!(Location::list_from_json(&json!([])).is_empty());
    }

    #[test]
    fn a_malformed_location_is_skipped_and_its_siblings_survive() {
        let locations = Location::list_from_json(&json!([
            {"uri": "file:///w/a.rs"},
            {"range": {"start": {"line": 1, "character": 0},
                       "end": {"line": 1, "character": 1}}},
            {"uri": "file:///w/good.rs", "range": {"start": {"line": 3, "character": 0},
                                                   "end": {"line": 3, "character": 1}}},
        ]));
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].uri.as_str(), "file:///w/good.rs");
    }

    #[test]
    fn a_negative_coordinate_is_clamped_rather_than_dropping_the_location() {
        let locations = Location::list_from_json(&json!({
            "uri": "file:///w/a.rs",
            "range": {"start": {"line": -1, "character": -4},
                      "end": {"line": 0, "character": 1}},
        }));
        assert_eq!(locations[0].range.start, Position::ZERO);
    }

    #[test]
    fn a_non_file_uri_survives_being_read() {
        // `jdt:` and friends. The editor cannot open one, but losing the answer
        // here would make the failure look like the server said nothing.
        let locations = Location::list_from_json(&json!({
            "uri": "jdt://contents/rt.jar/java.lang/String.class",
            "range": {"start": {"line": 1, "character": 0},
                      "end": {"line": 1, "character": 1}},
        }));
        assert_eq!(locations.len(), 1);
        assert!(!locations[0].uri.is_file());
    }
}
