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

/// Why a completion list was asked for.
///
/// The server is told, because it changes what it offers: typing `.` should
/// suggest members, while `ctrl+space` on an empty line should suggest
/// everything in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionTrigger {
    /// The user asked, e.g. with `ctrl+space`.
    Invoked,
    /// A character the server nominated was typed.
    Character(String),
}

impl CompletionTrigger {
    fn to_json(&self) -> serde_json::Value {
        match self {
            // 1 and 2 are the protocol's `CompletionTriggerKind`. There is a 3
            // — "for incomplete completions" — which deco does not use, because
            // it re-requests from scratch rather than refining a partial list.
            Self::Invoked => serde_json::json!({ "triggerKind": 1 }),
            Self::Character(c) => serde_json::json!({
                "triggerKind": 2,
                "triggerCharacter": c,
            }),
        }
    }
}

/// Parameters for `textDocument/completion`.
pub fn completion_params(
    uri: &Uri,
    position: Position,
    trigger: &CompletionTrigger,
) -> serde_json::Value {
    let mut params = text_document_position(uri, position);
    params["context"] = trigger.to_json();
    params
}

/// What kind of thing a completion item is, for the icon-shaped hint.
///
/// Only the distinctions worth drawing in a terminal are kept: the protocol has
/// 25 kinds and a single-character marker cannot honour them all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionKind {
    /// A local, parameter, field or constant.
    Value,
    /// A function, method or constructor.
    Function,
    /// A type, class, struct, enum or interface.
    Type,
    /// A module, namespace or file.
    Module,
    /// A keyword or operator.
    Keyword,
    /// A snippet or text fragment.
    Snippet,
    /// Anything else.
    #[default]
    Other,
}

impl CompletionKind {
    /// Reads the protocol's numeric `CompletionItemKind`.
    fn from_number(value: Option<i64>) -> Self {
        match value {
            // The numbers are the protocol's `CompletionItemKind`; the names
            // beside them are what each one is called there.
            Some(2..=4) => Self::Function, // Method, Function, Constructor
            Some(5 | 6 | 10 | 20..=22) => Self::Value, // Field, Variable, Property, EnumMember, Constant, Struct
            Some(7 | 8 | 13 | 25) => Self::Type,       // Class, Interface, Enum, TypeParameter
            Some(9 | 11) => Self::Module,              // Module, Unit
            Some(14 | 24) => Self::Keyword,            // Keyword, Operator
            Some(15) => Self::Snippet,
            _ => Self::Other,
        }
    }

    /// A one-character marker for a list with no room for words.
    pub fn marker(self) -> char {
        match self {
            Self::Value => 'v',
            Self::Function => 'f',
            Self::Type => 't',
            Self::Module => 'm',
            Self::Keyword => 'k',
            Self::Snippet => 's',
            Self::Other => '·',
        }
    }
}

/// One suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// What to show in the list.
    pub label: String,
    /// What kind of thing it is.
    pub kind: CompletionKind,
    /// A short signature or type, shown beside the label.
    pub detail: Option<String>,
    /// What to insert, already resolved from `textEdit` or `insertText`.
    pub insert: String,
    /// The range `insert` replaces, when the server specified one.
    ///
    /// Authoritative when present: the server knows better than the editor
    /// where a completion begins. `rust-analyzer` completing `HashMap` after
    /// `Hash` replaces the typed prefix, and guessing that range from the
    /// document is how a completion ends up as `HashHashMap`.
    pub replace: Option<Range>,
    /// What to match the user's typing against, if it differs from the label.
    pub filter: String,
    /// The server's preferred ordering key, if it gave one.
    pub sort: Option<String>,
    /// Whether the server wants this selected when the list opens.
    pub preselect: bool,
    /// Whether `insert` is snippet syntax the editor cannot expand.
    ///
    /// deco advertises `snippetSupport: false`, so a well-behaved server sends
    /// plain text — but several send snippets regardless. Inserting
    /// `foo(${1:arg})` literally is worse than inserting nothing, so the
    /// placeholders are stripped and this records that it happened.
    pub was_snippet: bool,
}

impl CompletionItem {
    /// Reads one item.
    ///
    /// `None` only when there is no label, since an unlabelled item cannot be
    /// shown or chosen.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let label = value.get("label")?.as_str()?.trim().to_owned();
        if label.is_empty() {
            return None;
        }

        // `textEdit` wins over `insertText`, which wins over the label. That is
        // the protocol's own precedence, and getting it backwards inserts the
        // display text — which for a function is often `foo(…)` with an ellipsis.
        let text_edit = value.get("textEdit").filter(|v| v.is_object());
        let (edit_text, replace) = match text_edit {
            Some(edit) => {
                // `InsertReplaceEdit` has `insert` and `replace` instead of
                // `range`. Preferring `replace` matches what a user expects
                // when completing over an existing word.
                let range = edit
                    .get("replace")
                    .or_else(|| edit.get("insert"))
                    .or_else(|| edit.get("range"))
                    .and_then(read_range);
                (
                    edit.get("newText")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    range,
                )
            }
            None => (None, None),
        };

        let raw_insert = edit_text
            .or_else(|| {
                value
                    .get("insertText")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| label.clone());

        // 2 is `Snippet` in the protocol's `InsertTextFormat`.
        let declared_snippet = value
            .get("insertTextFormat")
            .and_then(|v| v.as_i64())
            .is_some_and(|format| format == 2);
        let (insert, stripped) = strip_snippet(&raw_insert);

        Some(Self {
            kind: CompletionKind::from_number(value.get("kind").and_then(|v| v.as_i64())),
            detail: value
                .get("detail")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_owned),
            insert,
            replace,
            filter: value
                .get("filterText")
                .and_then(|v| v.as_str())
                .filter(|f| !f.is_empty())
                .unwrap_or(&label)
                .to_owned(),
            sort: value
                .get("sortText")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            preselect: value
                .get("preselect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            was_snippet: declared_snippet || stripped,
            label,
        })
    }

    /// Reads a `textDocument/completion` result into a list.
    ///
    /// Accepts a `CompletionList` (`{ isIncomplete, items }`), a bare array of
    /// items, and `null`. Returns the items and whether the list was marked
    /// incomplete — which deco reports but does not act on, since it re-requests
    /// from scratch rather than refining.
    pub fn list_from_json(value: &serde_json::Value) -> (Vec<Self>, bool) {
        let (items, incomplete) = match value {
            serde_json::Value::Array(items) => (items.as_slice(), false),
            serde_json::Value::Object(map) => (
                map.get("items")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                map.get("isIncomplete")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            ),
            _ => (&[][..], false),
        };
        (
            items.iter().filter_map(Self::from_json).collect(),
            incomplete,
        )
    }

    /// The key to sort by: the server's `sortText` if it gave one, else the label.
    ///
    /// Servers use `sortText` to put what you probably want first — a prefix
    /// match ahead of a fuzzy one — and ignoring it makes a good server's
    /// ordering look arbitrary.
    pub fn sort_key(&self) -> &str {
        self.sort.as_deref().unwrap_or(&self.label)
    }
}

/// Removes snippet placeholders, returning the text and whether any were found.
///
/// `${1:name}` becomes `name`, `${1}` and `$1` and `$0` vanish, `\$` becomes a
/// literal `$`. Not an expansion — deco has no tab stops — but it produces text
/// a person would have typed, which is the best available answer when a server
/// ignores `snippetSupport: false`.
fn strip_snippet(text: &str) -> (String, bool) {
    if !text.contains('$') {
        return (text.to_owned(), false);
    }

    let mut out = String::with_capacity(text.len());
    let mut found = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            // An escaped `$` is a literal one and not a placeholder.
            if let Some(&next) = chars.peek() {
                if next == '$' || next == '}' || next == '\\' {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
            out.push(c);
            continue;
        }
        if c != '$' {
            out.push(c);
            continue;
        }

        match chars.peek() {
            // `${…}` — a placeholder, possibly with a default to keep.
            Some('{') => {
                found = true;
                chars.next();
                let mut body = String::new();
                let mut depth = 1usize;
                for inner in chars.by_ref() {
                    match inner {
                        '{' => {
                            depth += 1;
                            body.push(inner);
                        }
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            body.push(inner);
                        }
                        _ => body.push(inner),
                    }
                }
                // `${1:default}` keeps the default; `${1}` and choice syntax
                // `${1|a,b|}` keep nothing, since picking one would be a guess.
                if let Some((_, default)) = body.split_once(':') {
                    if !default.contains('|') {
                        out.push_str(default);
                    }
                }
            }
            // `$1`, `$0` — a bare tab stop with nothing to keep.
            Some(d) if d.is_ascii_digit() => {
                found = true;
                while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    chars.next();
                }
            }
            // A `$` that is not a placeholder at all, e.g. a shell variable.
            _ => out.push('$'),
        }
    }

    (out, found)
}

/// How the user wants text laid out, as `textDocument/formatting` asks for it.
///
/// The server formats to *these*, not to its own defaults — which is the whole
/// reason to send them. A server told nothing will use four-space indentation
/// against a project that uses two, and the result is a diff touching every
/// line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormattingOptions {
    /// `editor.tabSize`.
    pub tab_size: u32,
    /// `editor.insertSpaces`.
    pub insert_spaces: bool,
    /// `files.trimTrailingWhitespace`.
    pub trim_trailing_whitespace: bool,
    /// `files.insertFinalNewline`.
    pub insert_final_newline: bool,
}

impl FormattingOptions {
    fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            // The two required members. The rest are optional and a server may
            // ignore them, which is fine — sending them costs nothing and a
            // server that honours them saves the user a fight with their linter.
            "tabSize": self.tab_size,
            "insertSpaces": self.insert_spaces,
            "trimTrailingWhitespace": self.trim_trailing_whitespace,
            "insertFinalNewline": self.insert_final_newline,
        })
    }
}

/// Parameters for `textDocument/formatting`.
pub fn formatting_params(uri: &Uri, options: FormattingOptions) -> serde_json::Value {
    serde_json::json!({
        "textDocument": { "uri": uri },
        "options": options.to_json(),
    })
}

/// Parameters for `textDocument/rangeFormatting`, which formats a selection.
pub fn range_formatting_params(
    uri: &Uri,
    range: Range,
    options: FormattingOptions,
) -> serde_json::Value {
    let mut params = formatting_params(uri, options);
    params["range"] = serde_json::json!({
        "start": { "line": range.start.line, "character": range.start.character },
        "end": { "line": range.end.line, "character": range.end.character },
    });
    params
}

/// One replacement the server wants made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    /// What to replace, in the coordinates of the document as the server saw it.
    pub range: Range,
    /// What to replace it with.
    pub new_text: String,
}

impl TextEdit {
    /// Reads a `TextEdit[]` result.
    ///
    /// `null` and an empty array both mean the document is already formatted,
    /// which is a successful answer.
    ///
    /// # Ordering
    ///
    /// Every range refers to the document *as the server saw it*, and the
    /// specification says edits must not overlap but says nothing about the
    /// order they arrive in. Applying them front to back therefore corrupts the
    /// document: the first edit shifts every position after it. deco hands the
    /// whole set to [`deco_core::Transaction`], which sorts them and applies
    /// back to front — so this function preserves the server's order and leaves
    /// the question to the one place that already answers it correctly.
    pub fn list_from_json(value: &serde_json::Value) -> Vec<Self> {
        let Some(items) = value.as_array() else {
            return Vec::new();
        };
        items.iter().filter_map(Self::one).collect()
    }

    fn one(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            range: read_range(value.get("range")?)?,
            // An absent `newText` is a deletion. Distinct from a missing range,
            // which cannot be placed and so cannot be applied.
            new_text: value
                .get("newText")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// Whether this edit changes nothing.
    ///
    /// Servers routinely return a no-op edit for an already-formatted document;
    /// applying one would mark the file dirty and add an undo step for nothing.
    pub fn is_noop(&self) -> bool {
        self.range.is_empty() && self.new_text.is_empty()
    }
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

    #[test]
    fn completion_params_carry_the_trigger() {
        // The server changes what it offers: typing `.` should suggest members,
        // ctrl+space on a blank line should suggest everything in scope.
        let invoked = completion_params(&uri(), Position::new(1, 2), &CompletionTrigger::Invoked);
        assert_eq!(invoked["context"], json!({"triggerKind": 1}));

        let typed = completion_params(
            &uri(),
            Position::new(1, 2),
            &CompletionTrigger::Character(".".into()),
        );
        assert_eq!(
            typed["context"],
            json!({"triggerKind": 2, "triggerCharacter": "."})
        );
    }

    #[test]
    fn a_completion_list_is_read_from_the_object_form() {
        let (items, incomplete) = CompletionItem::list_from_json(&json!({
            "isIncomplete": true,
            "items": [{"label": "push"}, {"label": "pop"}],
        }));
        assert_eq!(items.len(), 2);
        assert!(incomplete);
    }

    #[test]
    fn a_completion_list_is_read_from_a_bare_array() {
        let (items, incomplete) = CompletionItem::list_from_json(&json!([{"label": "push"}]));
        assert_eq!(items.len(), 1);
        assert!(!incomplete);
    }

    #[test]
    fn a_null_completion_is_an_empty_list() {
        let (items, incomplete) = CompletionItem::list_from_json(&json!(null));
        assert!(items.is_empty());
        assert!(!incomplete);
    }

    #[test]
    fn an_unlabelled_item_is_dropped_and_its_siblings_survive() {
        // It could be neither shown nor chosen.
        let (items, _) = CompletionItem::list_from_json(&json!([
            {"detail": "no label"},
            {"label": "   "},
            {"label": "good"},
        ]));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "good");
    }

    #[test]
    fn text_edit_beats_insert_text_which_beats_the_label() {
        // The protocol's own precedence. Backwards, it inserts the display text
        // — which for a function is often `foo(…)`, ellipsis included.
        let from_label = CompletionItem::from_json(&json!({"label": "foo"})).unwrap();
        assert_eq!(from_label.insert, "foo");

        let from_insert =
            CompletionItem::from_json(&json!({"label": "foo(…)", "insertText": "foo"})).unwrap();
        assert_eq!(from_insert.insert, "foo");

        let from_edit = CompletionItem::from_json(&json!({
            "label": "foo(…)",
            "insertText": "ignored",
            "textEdit": {
                "range": {"start": {"line": 1, "character": 0},
                          "end": {"line": 1, "character": 3}},
                "newText": "foo",
            },
        }))
        .unwrap();
        assert_eq!(from_edit.insert, "foo");
        assert_eq!(
            from_edit.replace,
            Some(Range::new(Position::new(1, 0), Position::new(1, 3)))
        );
    }

    #[test]
    fn an_insert_replace_edit_prefers_the_replace_range() {
        // Completing over an existing word should replace it, which is what a
        // user expects and what `replace` is for.
        let item = CompletionItem::from_json(&json!({
            "label": "HashMap",
            "textEdit": {
                "insert": {"start": {"line": 0, "character": 4},
                           "end": {"line": 0, "character": 4}},
                "replace": {"start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 4}},
                "newText": "HashMap",
            },
        }))
        .unwrap();
        assert_eq!(
            item.replace,
            Some(Range::new(Position::new(0, 0), Position::new(0, 4))),
            "guessing the range instead is how a completion becomes HashHashMap"
        );
    }

    #[test]
    fn snippet_placeholders_are_stripped_rather_than_inserted_literally() {
        // deco advertises snippetSupport: false and several servers send them
        // anyway. `foo(${1:arg})` inserted literally is worse than nothing.
        let item = CompletionItem::from_json(&json!({
            "label": "foo",
            "insertText": "foo(${1:arg}, ${2:other})$0",
            "insertTextFormat": 2,
        }))
        .unwrap();
        assert_eq!(item.insert, "foo(arg, other)");
        assert!(item.was_snippet);
    }

    #[test]
    fn a_bare_tab_stop_leaves_nothing_behind() {
        let item =
            CompletionItem::from_json(&json!({"label": "if", "insertText": "if $1 {\n\t$0\n}"}))
                .unwrap();
        assert_eq!(item.insert, "if  {\n\t\n}");
        assert!(item.was_snippet);
    }

    #[test]
    fn a_choice_placeholder_keeps_nothing_rather_than_guessing() {
        // `${1|a,b,c|}` offers alternatives; picking one for the user would be
        // an invention.
        let item = CompletionItem::from_json(&json!({
            "label": "vis",
            "insertText": "${1|pub,pub(crate)|} fn",
        }))
        .unwrap();
        assert_eq!(item.insert, " fn");
    }

    #[test]
    fn an_escaped_dollar_stays_literal() {
        let item = CompletionItem::from_json(&json!({
            "label": "shell",
            "insertText": "echo \\$HOME",
        }))
        .unwrap();
        assert_eq!(item.insert, "echo $HOME");
    }

    #[test]
    fn a_dollar_that_is_not_a_placeholder_survives() {
        // A shell variable in a plain-text completion is not snippet syntax.
        let item = CompletionItem::from_json(&json!({
            "label": "env",
            "insertText": "$PATH and $ alone",
        }))
        .unwrap();
        assert_eq!(item.insert, "$PATH and $ alone");
        assert!(!item.was_snippet, "nothing was actually a placeholder");
    }

    #[test]
    fn nested_braces_inside_a_placeholder_are_balanced() {
        let item = CompletionItem::from_json(&json!({
            "label": "closure",
            "insertText": "map(|x| ${1:{ x }})",
        }))
        .unwrap();
        assert_eq!(item.insert, "map(|x| { x })");
    }

    #[test]
    fn text_without_a_dollar_is_returned_untouched() {
        let item =
            CompletionItem::from_json(&json!({"label": "plain", "insertText": "plain_text"}))
                .unwrap();
        assert_eq!(item.insert, "plain_text");
        assert!(!item.was_snippet);
    }

    #[test]
    fn a_snippet_declared_by_format_alone_is_still_flagged() {
        // No placeholders to strip, but the editor should still know it was one:
        // the server may send tab stops in a sibling item.
        let item = CompletionItem::from_json(&json!({
            "label": "foo",
            "insertText": "foo",
            "insertTextFormat": 2,
        }))
        .unwrap();
        assert!(item.was_snippet);
        assert_eq!(item.insert, "foo");
    }

    #[test]
    fn filter_text_is_used_for_matching_when_it_differs_from_the_label() {
        // rust-analyzer labels an item `foo(…)` and filters on `foo`; matching
        // the label would fail the moment the user typed `f`.
        let item =
            CompletionItem::from_json(&json!({"label": "foo(…)", "filterText": "foo"})).unwrap();
        assert_eq!(item.filter, "foo");

        let without = CompletionItem::from_json(&json!({"label": "bar"})).unwrap();
        assert_eq!(without.filter, "bar", "the label is the fallback");
    }

    #[test]
    fn sort_text_orders_ahead_of_the_label() {
        // Servers use it to put the likely answer first; ignoring it makes a
        // good server's ordering look arbitrary.
        let with =
            CompletionItem::from_json(&json!({"label": "zebra", "sortText": "0000"})).unwrap();
        assert_eq!(with.sort_key(), "0000");

        let without = CompletionItem::from_json(&json!({"label": "apple"})).unwrap();
        assert_eq!(without.sort_key(), "apple");
    }

    #[test]
    fn every_completion_kind_maps_to_a_distinct_marker() {
        for (number, expected) in [
            (2, CompletionKind::Function),
            (3, CompletionKind::Function),
            (6, CompletionKind::Value),
            (7, CompletionKind::Type),
            (9, CompletionKind::Module),
            (14, CompletionKind::Keyword),
            (15, CompletionKind::Snippet),
            (99, CompletionKind::Other),
        ] {
            let item = CompletionItem::from_json(&json!({"label": "x", "kind": number})).unwrap();
            assert_eq!(item.kind, expected, "kind {number}");
            assert!(!item.kind.marker().is_whitespace());
        }
        assert_eq!(
            CompletionItem::from_json(&json!({"label": "x"}))
                .unwrap()
                .kind,
            CompletionKind::Other,
            "an absent kind is not a guess"
        );
    }

    #[test]
    fn an_empty_detail_is_dropped_rather_than_shown_as_a_gap() {
        for detail in [json!(""), json!("   ")] {
            let item = CompletionItem::from_json(&json!({"label": "x", "detail": detail})).unwrap();
            assert_eq!(item.detail, None);
        }
    }

    #[test]
    fn preselect_is_carried_through() {
        let item = CompletionItem::from_json(&json!({"label": "x", "preselect": true})).unwrap();
        assert!(item.preselect);
    }

    #[test]
    fn formatting_params_carry_the_users_own_settings() {
        // A server told nothing indents with four spaces against a two-space
        // project, and the result is a diff touching every line.
        let options = FormattingOptions {
            tab_size: 2,
            insert_spaces: true,
            trim_trailing_whitespace: true,
            insert_final_newline: false,
        };
        let params = formatting_params(&uri(), options);
        assert_eq!(params["options"]["tabSize"], json!(2));
        assert_eq!(params["options"]["insertSpaces"], json!(true));
        assert_eq!(params["options"]["trimTrailingWhitespace"], json!(true));
        assert_eq!(params["options"]["insertFinalNewline"], json!(false));
        assert_eq!(params["textDocument"]["uri"], json!("file:///w/a.rs"));
    }

    #[test]
    fn range_formatting_adds_the_range_to_the_same_params() {
        let params = range_formatting_params(
            &uri(),
            Range::new(Position::new(2, 0), Position::new(5, 4)),
            FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                trim_trailing_whitespace: false,
                insert_final_newline: true,
            },
        );
        assert_eq!(
            params["range"],
            json!({"start": {"line": 2, "character": 0}, "end": {"line": 5, "character": 4}})
        );
        assert_eq!(params["options"]["tabSize"], json!(4));
    }

    #[test]
    fn text_edits_are_read_in_the_order_the_server_sent_them() {
        // Preserved rather than sorted here: `Transaction` already sorts and
        // applies back to front, and doing it in two places invites the two from
        // disagreeing.
        let edits = TextEdit::list_from_json(&json!([
            {"range": {"start": {"line": 5, "character": 0},
                       "end": {"line": 5, "character": 4}}, "newText": "  "},
            {"range": {"start": {"line": 1, "character": 0},
                       "end": {"line": 1, "character": 4}}, "newText": ""},
        ]));
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].range.start.line, 5);
        assert_eq!(edits[1].range.start.line, 1);
    }

    #[test]
    fn a_null_or_empty_formatting_result_is_no_edits() {
        // Both mean the document is already formatted, which is a success.
        assert!(TextEdit::list_from_json(&json!(null)).is_empty());
        assert!(TextEdit::list_from_json(&json!([])).is_empty());
    }

    #[test]
    fn an_absent_new_text_is_a_deletion() {
        let edits = TextEdit::list_from_json(&json!([{
            "range": {"start": {"line": 0, "character": 0},
                      "end": {"line": 0, "character": 4}}
        }]));
        assert_eq!(edits[0].new_text, "");
        assert!(!edits[0].is_noop(), "it deletes four characters");
    }

    #[test]
    fn an_edit_without_a_range_is_skipped_and_its_siblings_survive() {
        // It cannot be placed, so it cannot be applied — but dropping the whole
        // set would lose a formatting run to one malformed entry.
        let edits = TextEdit::list_from_json(&json!([
            {"newText": "nowhere"},
            {"range": {"start": {"line": 1, "character": 0},
                       "end": {"line": 1, "character": 1}}, "newText": "x"},
        ]));
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "x");
    }

    #[test]
    fn a_no_op_edit_is_recognisable() {
        // Servers return these for an already-formatted document; applying one
        // marks the file dirty and adds an undo step for nothing.
        let edits = TextEdit::list_from_json(&json!([{
            "range": {"start": {"line": 3, "character": 2},
                      "end": {"line": 3, "character": 2}},
            "newText": "",
        }]));
        assert!(edits[0].is_noop());
    }

    #[test]
    fn a_non_array_formatting_result_is_no_edits_rather_than_a_panic() {
        for value in [json!({}), json!("nonsense"), json!(7)] {
            assert!(TextEdit::list_from_json(&value).is_empty(), "{value}");
        }
    }
}
