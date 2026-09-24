//! Building language-feature requests and reading their answers.
//!
//! The request *parameters* are simple, usually a URI and a position. The
//! *results* are among the most polymorphic shapes in the specification. Each
//! one gained alternative forms across protocol versions, and servers implement
//! the form of the version they were written against. A client that handles
//! only the newest form loses the feature with many servers.
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
//! them, an array of `LocationLink` (which names its range `targetRange` and its
//! URI `targetUri`), or `null`.
//!
//! All of these forms are accepted here, and each has a test. `null` is a
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
    /// The range the hover describes, if the server provided one. Used to decide
    /// whether a cached hover still applies after the cursor moves.
    pub range: Option<Range>,
}

impl Hover {
    /// Reads a `textDocument/hover` result.
    ///
    /// `None` for `null`, for a missing `contents`, and for contents that
    /// flatten to empty text, so that no empty hover box is shown.
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

/// Flattens every form of `contents` into plain text.
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
            // `value`; only the first has `kind`. The text is in `value` for both.
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
/// The client requests `plaintext` in its capabilities, but some servers send
/// Markdown anyway because it is the only format they produce. Fenced code
/// blocks are the common case, and their fences are not useful in unstyled
/// text, so they are removed. Other Markdown is left unchanged rather than
/// partly rendered, because a corrupted signature is worse than a literal
/// asterisk.
fn strip_markup(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            continue;
        }
        // A horizontal rule separates a signature from its documentation in
        // rust-analyzer's output. As plain text it is a line of dashes.
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
    /// Reads every form of a location result into a flat list.
    ///
    /// Accepts a single `Location`, an array of them, an array of
    /// `LocationLink`, and `null`. An empty list means "nothing here", which is
    /// a successful answer.
    pub fn list_from_json(value: &serde_json::Value) -> Vec<Self> {
        match value {
            serde_json::Value::Array(items) => items.iter().filter_map(Self::one).collect(),
            serde_json::Value::Object(_) => Self::one(value).into_iter().collect(),
            // Including `null`, which means there is no definition at this
            // position.
            _ => Vec::new(),
        }
    }

    fn one(value: &serde_json::Value) -> Option<Self> {
        // `LocationLink` first: it also has no plain `uri`, so checking for
        // `targetUri` up front avoids reading a link as a malformed location.
        if let (Some(uri), Some(range)) = (
            value.get("targetUri").and_then(|v| v.as_str()),
            value
                // `targetSelectionRange` is the identifier, where a user
                // expects the cursor. `targetRange` is the whole definition,
                // which would place the cursor on the doc comment.
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

/// [`read_range`], for the one caller outside this module.
///
/// The supervisor filters raw diagnostics by range without parsing them. A
/// second implementation of range reading would have to be kept consistent
/// with this one.
pub(crate) fn read_range_public(value: &serde_json::Value) -> Option<Range> {
    read_range(value)
}

/// Reads a `{ start, end }` range, clamping rather than rejecting.
///
/// A server that miscounts then causes one misplaced jump instead of disabling
/// the feature.
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

/// Why a completion list was requested.
///
/// This is sent to the server because it affects the suggestions: typing `.`
/// should suggest members, while `ctrl+space` on an empty line should suggest
/// everything in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionTrigger {
    /// The user requested it, e.g. with `ctrl+space`.
    Invoked,
    /// One of the server's trigger characters was typed.
    Character(String),
}

impl CompletionTrigger {
    fn to_json(&self) -> serde_json::Value {
        match self {
            // 1 and 2 are the protocol's `CompletionTriggerKind`. deco does not
            // use 3 ("for incomplete completions"), because it sends a new
            // request rather than refining a partial list.
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

/// The kind of a completion item, shown as a marker in place of an icon.
///
/// Only the distinctions useful in a terminal are kept. The protocol has 25
/// kinds, and a single-character marker cannot represent them all.
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
    /// Takes precedence when present, because the server knows where a
    /// completion begins. `rust-analyzer` completing `HashMap` after `Hash`
    /// replaces the typed prefix. Guessing that range from the document can
    /// produce `HashHashMap`.
    pub replace: Option<Range>,
    /// What to match the user's typing against, if it differs from the label.
    pub filter: String,
    /// The server's preferred ordering key, if it gave one.
    pub sort: Option<String>,
    /// Whether the server wants this selected when the list opens.
    pub preselect: bool,
    /// Whether the server supplied snippet syntax, declared or detected.
    ///
    /// deco advertises `snippetSupport: false`, so a conforming server sends
    /// plain text, but several servers send snippets anyway. Inserting
    /// `foo(${1:arg})` literally is worse than inserting nothing, so the
    /// placeholders are stripped for the fallback. `snippet` separately holds
    /// the text and tab stops when no insertion context is needed. Variables
    /// in `snippet_source` are resolved when the completion is accepted.
    pub was_snippet: bool,
    /// Parsed numeric tab stops, when the completion uses the supported subset.
    pub snippet: Option<crate::snippet::Snippet>,
    /// Original snippet text, resolved against the document when accepted.
    pub snippet_source: Option<String>,
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

        // `textEdit` takes precedence over `insertText`, which takes precedence
        // over the label. This is the protocol's precedence. Reversing it
        // inserts the display text, which for a function is often `foo(…)`
        // with an ellipsis.
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
        let snippet = if declared_snippet || stripped {
            crate::snippet::Snippet::parse(&raw_insert)
        } else {
            None
        };

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
            snippet,
            snippet_source: (declared_snippet || stripped).then_some(raw_insert),
            label,
        })
    }

    /// Reads a `textDocument/completion` result into a list.
    ///
    /// Accepts a `CompletionList` (`{ isIncomplete, items }`), a bare array of
    /// items, and `null`. Returns the items and whether the list was marked
    /// incomplete. deco reports that flag but does not use it, because it sends
    /// a new request rather than refining the list.
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
    /// Servers use `sortText` to put likely matches first, for example a prefix
    /// match before a fuzzy match. Ignoring it makes the order look arbitrary.
    pub fn sort_key(&self) -> &str {
        self.sort.as_deref().unwrap_or(&self.label)
    }
}

/// Removes snippet placeholders, returning the text and whether any were found.
///
/// `${1:name}` becomes `name`, `${1}`, `$1` and `$0` are removed, and `\$`
/// becomes a literal `$`. This is the fallback for unsupported syntax; supported snippets
/// are expanded separately when accepted.
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
                // `${1:default}` keeps the default. `${1}` and choice syntax
                // `${1|a,b|}` keep nothing, because choosing an option would be
                // a guess.
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
/// These are sent so that the server formats with *these* settings instead of
/// its own defaults. Otherwise a server may use four-space indentation in a
/// project that uses two, and the result changes every line.
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
            // The two required members. The others are optional and a server
            // may ignore them. A server that applies them produces output that
            // matches the user's whitespace settings.
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
    /// Every range refers to the document *as the server saw it*. The
    /// specification requires that edits do not overlap but does not define
    /// their order. Applying them front to back therefore corrupts the
    /// document, because the first edit shifts every later position. deco
    /// passes the whole set to [`deco_core::Transaction`], which sorts them and
    /// applies them back to front. This function therefore keeps the server's
    /// order.
    pub fn list_from_json(value: &serde_json::Value) -> Vec<Self> {
        let Some(items) = value.as_array() else {
            return Vec::new();
        };
        items.iter().filter_map(Self::one).collect()
    }

    fn one(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            range: read_range(value.get("range")?)?,
            // An absent `newText` is a deletion. A missing range is different:
            // the edit has no position and cannot be applied.
            new_text: value
                .get("newText")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// Whether this edit changes nothing.
    ///
    /// Servers often return a no-op edit for an already formatted document.
    /// Applying it would mark the file dirty and add an empty undo step.
    pub fn is_noop(&self) -> bool {
        self.range.is_empty() && self.new_text.is_empty()
    }
}

/// Parameters for `textDocument/rename`.
pub fn rename_params(uri: &Uri, position: Position, new_name: &str) -> serde_json::Value {
    let mut params = text_document_position(uri, position);
    params["newName"] = serde_json::Value::String(new_name.to_owned());
    params
}

/// All edits to one document within a [`WorkspaceEdit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentEdits {
    /// Which document.
    pub uri: Uri,
    /// The document version the server used to compute these, if provided.
    ///
    /// Only `documentChanges` includes it. `None` means the server did not
    /// state which version it used; it does not mean the current version. The
    /// code that applies the edit must handle this, because it decides what to
    /// do with an edit computed for text that has since changed.
    pub version: Option<i64>,
    /// What to change, in the coordinates of that version.
    pub edits: Vec<TextEdit>,
}

/// All changes a server requests, across any number of documents.
///
/// Renames, code actions and replace-across-files all use this type.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceEdit {
    /// One entry per document, in the order the server listed them.
    pub changes: Vec<DocumentEdits>,
}

/// Why a `WorkspaceEdit` could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceEditError {
    /// The server wants a file created, renamed or deleted as part of the edit.
    ///
    /// Contains the `kind` the server sent. Reported rather than skipped,
    /// because these operations come with text edits that depend on them:
    /// renaming a Rust module renames its file *and* rewrites the paths that
    /// refer to it. Applying only the text edits would leave a project that no
    /// longer builds and an undo history that cannot restore it.
    FileOperation(String),
}

impl std::fmt::Display for WorkspaceEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileOperation(kind) => {
                write!(
                    f,
                    "the server wants to {kind} a file, which deco cannot do yet"
                )
            }
        }
    }
}

impl std::error::Error for WorkspaceEditError {}

impl WorkspaceEdit {
    /// Reads a `WorkspaceEdit` result.
    ///
    /// `null`, which a server returns to decline a rename, is read as an empty
    /// edit. The caller reports it as "nothing to do", not as a failure.
    ///
    /// # The two spellings
    ///
    /// The protocol has two fields for the same information.
    /// `documentChanges` is newer and preferred: it is ordered and includes
    /// the document version each edit was computed for. `changes` is a plain
    /// `uri -> edits` map without either. When a server sends both, as several
    /// do for older clients, `documentChanges` is used, as the specification
    /// requires. Discarding the versions would mean applying edits without
    /// checking that the text is still the text they were computed for.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, WorkspaceEditError> {
        if let Some(changes) = value.get("documentChanges").and_then(|v| v.as_array()) {
            return Self::from_document_changes(changes);
        }
        let Some(map) = value.get("changes").and_then(|v| v.as_object()) else {
            return Ok(Self::default());
        };
        Ok(Self {
            changes: map
                .iter()
                .map(|(uri, edits)| DocumentEdits {
                    uri: Uri::from_string(uri.as_str()),
                    version: None,
                    edits: TextEdit::list_from_json(edits),
                })
                .filter(|document| !document.edits.is_empty())
                .collect(),
        })
    }

    fn from_document_changes(changes: &[serde_json::Value]) -> Result<Self, WorkspaceEditError> {
        let mut documents: Vec<DocumentEdits> = Vec::new();
        for change in changes {
            // A file operation is the only member of this array that is not a
            // `TextDocumentEdit`. It is identified by `kind`.
            if let Some(kind) = change.get("kind").and_then(|v| v.as_str()) {
                return Err(WorkspaceEditError::FileOperation(kind.to_owned()));
            }
            let Some(document) = change.get("textDocument") else {
                continue;
            };
            let Some(uri) = document.get("uri").and_then(|v| v.as_str()) else {
                continue;
            };
            let edits = change
                .get("edits")
                .map(TextEdit::list_from_json)
                .unwrap_or_default();
            if edits.is_empty() {
                continue;
            }
            // `version` can be null here too. Null means the server did not
            // track a version, which differs from the field being absent.
            let version = document.get("version").and_then(|v| v.as_i64());

            // A document may appear more than once, and the entries are ordered.
            // Concatenating them in arrival order keeps that order. The code that
            // applies the result decides whether it can be applied, and it
            // already rejects overlapping edits.
            match documents.iter_mut().find(|seen| seen.uri.as_str() == uri) {
                Some(seen) => seen.edits.extend(edits),
                None => documents.push(DocumentEdits {
                    uri: Uri::from_string(uri),
                    version,
                    edits,
                }),
            }
        }
        Ok(Self { changes: documents })
    }

    /// Whether the server asked for no changes at all.
    pub fn is_empty(&self) -> bool {
        self.changes
            .iter()
            .all(|document| document.edits.is_empty())
    }

    /// How many documents take part.
    pub fn documents(&self) -> usize {
        self.changes.len()
    }

    /// How many replacements there are in total.
    pub fn edits(&self) -> usize {
        self.changes.iter().map(|d| d.edits.len()).sum()
    }
}

/// Parameters for `textDocument/codeAction`.
///
/// `diagnostics` are the server's own, **as it sent them**. A quick fix is
/// computed from the diagnostic it fixes, and a diagnostic has fields a client
/// must not interpret. `data` is opaque by specification, and several servers
/// store the information needed to build the fix there. If anything other than
/// the original object is sent, the server does not recognise the diagnostic.
pub fn code_action_params(
    uri: &Uri,
    range: Range,
    diagnostics: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": range.start.line, "character": range.start.character },
            "end": { "line": range.end.line, "character": range.end.character },
        },
        "context": { "diagnostics": diagnostics },
    })
}

/// An action a server offers for a location in a document.
///
/// Covers both shapes the result array can contain. A bare `Command`, the
/// older form that several servers still send, becomes a value with no edit
/// and a command name. deco cannot run such an action and reports that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeAction {
    /// What to show in the list.
    pub title: String,
    /// `quickfix`, `refactor.extract`, `source.organizeImports`, and so on.
    ///
    /// A hierarchy of dot-separated segments, so `refactor` is the prefix of
    /// every refactoring. `None` for a bare `Command`, which has no kind.
    pub kind: Option<String>,
    /// The server's reason why this cannot run now, if it provided one.
    ///
    /// Kept rather than filtered out. VS Code lists a disabled action with its
    /// reason, which is more useful to the user than a missing action.
    pub disabled: Option<String>,
    /// Whether the server marked this as the preferred action.
    pub preferred: bool,
    /// The command it would run, if any.
    pub command: Option<String>,
    /// Whether this entry was a bare `Command` rather than a `CodeAction`.
    ///
    /// The two are not interchangeable: `codeAction/resolve` takes a
    /// `CodeAction`. A `Command` without an edit is not waiting to be resolved.
    /// It is the complete offer, and running it requires a different request.
    pub is_command: bool,
    /// The edit it would make, **unparsed**.
    ///
    /// Kept as JSON until the action is chosen. A `WorkspaceEdit` can be
    /// rejected, for example for a file operation deco cannot perform.
    /// Rejecting it while *listing* would let one invalid entry empty the whole
    /// menu, although the other entries are valid.
    pub edit: Option<serde_json::Value>,
    /// The action exactly as the server sent it, for `codeAction/resolve`.
    ///
    /// Resolve takes the action and returns it with its edit filled in, so the
    /// request must contain the original action, including `data`.
    pub raw: serde_json::Value,
}

impl CodeAction {
    /// Reads a `(Command | CodeAction)[]` result.
    ///
    /// `null`, which means the server has no actions here, is read as an empty
    /// list. This is a successful answer.
    pub fn list_from_json(value: &serde_json::Value) -> Vec<Self> {
        let Some(items) = value.as_array() else {
            return Vec::new();
        };
        items.iter().filter_map(Self::one).collect()
    }

    fn one(value: &serde_json::Value) -> Option<Self> {
        let title = value.get("title")?.as_str()?.to_owned();
        // The two shapes are distinguished by the *type* of `command`. On a
        // `Command` it is the identifier, and on a `CodeAction` it is a nested
        // object. No other field distinguishes them reliably, because `kind`
        // and `edit` are both optional on a `CodeAction`.
        let (command, is_command) = match value.get("command") {
            Some(serde_json::Value::String(id)) => (Some(id.clone()), true),
            Some(nested) => (
                nested
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                false,
            ),
            None => (None, false),
        };
        Some(Self {
            title,
            kind: value
                .get("kind")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            // `disabled` is `{ reason }`; only the reason is kept.
            disabled: value
                .get("disabled")
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            preferred: value
                .get("isPreferred")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            command,
            is_command,
            edit: value.get("edit").cloned(),
            raw: value.clone(),
        })
    }

    /// Whether choosing this would need a `codeAction/resolve` first.
    ///
    /// False for a bare `Command`, because resolve takes a `CodeAction`. Also
    /// false for a disabled action, because the server has already stated that
    /// it cannot run.
    pub fn needs_resolving(&self) -> bool {
        self.edit.is_none() && self.disabled.is_none() && !self.is_command
    }

    /// The kind, in the form a list shows it.
    ///
    /// The last segment, because the leading segments are often the same for
    /// the whole menu. Entries such as `refactor.extract` differ only after the
    /// last dot, and a column of identical prefixes is not useful.
    pub fn short_kind(&self) -> Option<&str> {
        self.kind.as_deref().map(|kind| {
            kind.rsplit('.')
                .next()
                .filter(|last| !last.is_empty())
                .unwrap_or(kind)
        })
    }
}

/// Parameters for `codeAction/resolve`: the action, exactly as it arrived.
pub fn code_action_resolve_params(action: &CodeAction) -> serde_json::Value {
    action.raw.clone()
}

/// One semantically classified run of text.
///
/// Positions are absolute and in the negotiated encoding, so a caller can
/// compare them with a cursor without knowing the wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticSpan {
    /// Where the run is. A token never spans lines, per the specification.
    pub range: Range,
    /// The token type, resolved through the server's legend — `variable`,
    /// `function`, `namespace`.
    pub token_type: String,
    /// The modifiers, resolved through the legend — `readonly`, `declaration`.
    pub modifiers: Vec<String>,
}

/// Parameters for `textDocument/semanticTokens/full`.
pub fn semantic_tokens_params(uri: &crate::uri::Uri) -> serde_json::Value {
    serde_json::json!({ "textDocument": { "uri": uri.as_str() } })
}

/// Reads a `SemanticTokens` result into absolute spans.
///
/// # The encoding, and why this is worth testing carefully
///
/// The wire format is one flat array of integers in groups of five:
/// `deltaLine`, `deltaStart`, `length`, `tokenType`, `tokenModifiers`. Each
/// position is relative to the previous token. `deltaStart` counts from the
/// previous token's start *when both are on the same line*, and from the start
/// of the line otherwise. Ignoring that distinction shifts every token after
/// the first on each line, which colours the wrong words without a visible
/// error.
///
/// `tokenModifiers` is a bitset over the legend's modifier list, not an index.
///
/// A group whose type index is not in the legend is dropped. Colouring by
/// index would depend on the server's arbitrary ordering.
pub fn semantic_spans_from_json(
    value: &serde_json::Value,
    legend: &crate::capabilities::SemanticTokensOptions,
) -> Vec<SemanticSpan> {
    let Some(data) = value.get("data").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    let mut line = 0u32;
    let mut start = 0u32;
    // `chunks_exact` rather than `chunks`. A trailing partial group is an
    // invalid response, and filling in its missing fields would create a token.
    for group in data.chunks_exact(5) {
        let numbers: Vec<u32> = group
            .iter()
            .map(|n| n.as_u64().unwrap_or(0) as u32)
            .collect();
        let (delta_line, delta_start, length, kind, modifiers) =
            (numbers[0], numbers[1], numbers[2], numbers[3], numbers[4]);

        line += delta_line;
        start = if delta_line == 0 {
            start + delta_start
        } else {
            delta_start
        };

        let Some(token_type) = legend.token_types.get(kind as usize) else {
            continue;
        };
        // A zero-length token has nothing to colour, and the renderer would drop
        // a range whose end equals its start.
        if length == 0 {
            continue;
        }
        spans.push(SemanticSpan {
            range: Range::new(
                Position::new(line, start),
                Position::new(line, start + length),
            ),
            token_type: token_type.clone(),
            modifiers: legend
                .token_modifiers
                .iter()
                .enumerate()
                .filter(|(bit, _)| modifiers & (1 << bit) != 0)
                .map(|(_, name)| name.clone())
                .collect(),
        });
    }
    spans
}

/// A name the server found in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSymbol {
    /// The name, exactly as the server sent it.
    pub name: String,
    /// The kind, such as `function`, `struct` or `field`, resolved from the
    /// protocol's `SymbolKind` integer. `None` for a number outside the
    /// enumeration, which means the server uses a newer protocol version. It is
    /// not an error.
    pub kind: Option<&'static str>,
    /// The enclosing symbol: the class for a method, or the parent path for a
    /// nested symbol. `None` at the top level.
    pub container: Option<String>,
    /// Where the *name* is, not where the definition starts.
    pub position: Position,
}

impl DocumentSymbol {
    /// The display name: `Counter.bump` for a method, `bump` at the top level.
    ///
    /// Qualified so that filtering can find a method by its class, and so that
    /// two `new`s in one file can be distinguished in the list.
    pub fn qualified(&self) -> String {
        match &self.container {
            Some(container) => format!("{container}.{}", self.name),
            None => self.name.clone(),
        }
    }

    /// Reads a `textDocument/documentSymbol` result, in either of its shapes.
    ///
    /// The protocol has two shapes, and the server chooses which one to send:
    ///
    /// ```jsonc
    /// // DocumentSymbol[]: a tree, with the nesting the file has
    /// [{ "name": "Counter", "kind": 23, "range": {…}, "selectionRange": {…},
    ///    "children": [{ "name": "bump", "kind": 6, … }] }]
    ///
    /// // SymbolInformation[]: flat, each with a whole location and a container name
    /// [{ "name": "bump", "kind": 6, "containerName": "Counter",
    ///    "location": { "uri": "file:///…", "range": {…} } }]
    /// ```
    ///
    /// Both are flattened to one list in document order, with each parent before
    /// its children. The picker shows this order, and it matches the file.
    ///
    /// `SymbolInformation`'s `location.uri` is ignored. The request named one
    /// document, so a response about another document violates the
    /// specification, and using it would let the symbol list navigate to an
    /// unrelated file.
    pub fn list_from_json(value: &serde_json::Value) -> Vec<Self> {
        let mut out = Vec::new();
        if let Some(items) = value.as_array() {
            collect_symbols(items, None, &mut out);
        }
        out
    }
}

/// Appends `items` and their children to `out`, depth first.
fn collect_symbols(
    items: &[serde_json::Value],
    container: Option<&str>,
    out: &mut Vec<DocumentSymbol>,
) {
    for item in items {
        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        // A symbol without a name cannot be selected from a list.
        if name.is_empty() {
            continue;
        }

        // Check for `SymbolInformation` first. It is the shape with a
        // `location`, and its `containerName` is a plain string, whereas a
        // `DocumentSymbol` expresses nesting with `children`.
        let flat = item.get("location");
        let position = match flat {
            Some(location) => location.get("range").and_then(read_range).map(|r| r.start),
            // `selectionRange` is the name. `range` covers the whole definition
            // and would place the cursor on a doc comment.
            None => item
                .get("selectionRange")
                .or_else(|| item.get("range"))
                .and_then(read_range)
                .map(|r| r.start),
        };
        let Some(position) = position else {
            // Without a position there is no navigation target, and the row
            // would do nothing.
            continue;
        };

        let container = match flat {
            Some(_) => item
                .get("containerName")
                .and_then(|v| v.as_str())
                .filter(|c| !c.is_empty())
                .map(str::to_owned),
            None => container.map(str::to_owned),
        };

        out.push(DocumentSymbol {
            name: name.to_owned(),
            kind: item
                .get("kind")
                .and_then(|v| v.as_u64())
                .and_then(symbol_kind_name),
            container,
            position,
        });

        // Children use the qualified name of the symbol just added as their
        // container, so a method three levels down shows its full path.
        if let Some(children) = item.get("children").and_then(|v| v.as_array()) {
            let qualified = out
                .last()
                .map(DocumentSymbol::qualified)
                .unwrap_or_default();
            collect_symbols(children, Some(&qualified), out);
        }
    }
}

/// Names a `SymbolKind`.
///
/// The protocol's numbers are stable, and new ones are only added. An
/// unrecognised number comes from a newer specification, so the symbol is
/// listed without a kind instead of being dropped.
fn symbol_kind_name(kind: u64) -> Option<&'static str> {
    Some(match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type parameter",
        _ => return None,
    })
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
    fn code_action_params_echo_the_diagnostics_verbatim() {
        // `data` is opaque to a client, and servers store the information
        // needed to build the fix there. If it is changed, the server does not
        // recognise the diagnostic.
        let diagnostic = json!({
            "range": range(1, 0, 1, 4),
            "message": "unused",
            "data": {"fix": "remove", "id": 91},
        });
        let params = code_action_params(
            &uri(),
            Range::new(Position::new(1, 0), Position::new(1, 4)),
            vec![diagnostic.clone()],
        );

        assert_eq!(params["context"]["diagnostics"][0], diagnostic);
        assert_eq!(params["range"]["end"], json!({"line": 1, "character": 4}));
    }

    #[test]
    fn a_code_action_and_a_bare_command_both_read() {
        // Both shapes can appear in one array, and several servers still send
        // the older one.
        let actions = CodeAction::list_from_json(&json!([
            {
                "title": "Remove unused import",
                "kind": "quickfix",
                "isPreferred": true,
                "edit": {"changes": {"file:///w/a.rs": []}},
            },
            {"title": "Organize imports", "command": "rust-analyzer.organizeImports"},
        ]));

        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].kind.as_deref(), Some("quickfix"));
        assert!(actions[0].preferred);
        assert!(actions[0].edit.is_some());
        assert_eq!(actions[0].command, None);

        assert_eq!(actions[1].kind, None, "a bare Command has no kind");
        assert_eq!(
            actions[1].command.as_deref(),
            Some("rust-analyzer.organizeImports")
        );
        assert!(actions[1].edit.is_none());
    }

    #[test]
    fn a_nested_command_is_read_off_the_object() {
        // A `CodeAction` may have a command *as well as* an edit, and its
        // `command` is an object rather than the identifier.
        let actions = CodeAction::list_from_json(&json!([{
            "title": "Extract into function",
            "kind": "refactor.extract",
            "command": {"title": "Rename it", "command": "editor.action.rename"},
        }]));

        assert_eq!(actions[0].command.as_deref(), Some("editor.action.rename"));
        assert_eq!(actions[0].short_kind(), Some("extract"));
    }

    #[test]
    fn an_action_with_no_edit_is_one_to_resolve() {
        let actions = CodeAction::list_from_json(&json!([
            {"title": "Expensive refactor", "kind": "refactor"},
            {"title": "Cheap fix", "kind": "quickfix", "edit": {"changes": {}}},
            {"title": "Not here", "kind": "quickfix", "disabled": {"reason": "not in a function"}},
        ]));

        assert!(actions[0].needs_resolving());
        assert!(!actions[1].needs_resolving(), "it already has its edit");
        assert!(
            !actions[2].needs_resolving(),
            "a disabled action is not one to go and ask about"
        );
        assert_eq!(actions[2].disabled.as_deref(), Some("not in a function"));

        // A bare `Command` also has no edit, but is not resolvable:
        // `codeAction/resolve` takes a `CodeAction`, and the server did not
        // offer one.
        let command = CodeAction::list_from_json(&json!([
            {"title": "Organize imports", "command": "example.organizeImports"},
        ]));
        assert!(command[0].is_command);
        assert!(!command[0].needs_resolving());
    }

    #[test]
    fn resolving_sends_the_action_back_exactly_as_it_arrived() {
        // Including `data`, which the server uses to identify the action.
        let sent = json!({
            "title": "Expensive refactor",
            "kind": "refactor",
            "data": {"file": "a.rs", "assist": 3},
        });
        let action = CodeAction::list_from_json(&json!([sent.clone()]))
            .pop()
            .expect("one action");
        assert_eq!(code_action_resolve_params(&action), sent);
    }

    #[test]
    fn nothing_to_offer_reads_as_an_empty_list() {
        assert!(CodeAction::list_from_json(&json!(null)).is_empty());
        assert!(CodeAction::list_from_json(&json!([])).is_empty());
        // An entry without a title cannot be listed; the others still are.
        let actions = CodeAction::list_from_json(&json!([{"kind": "quickfix"}, {"title": "Fix"}]));
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].title, "Fix");
    }

    #[test]
    fn the_kind_shown_is_the_part_that_differs() {
        // In a menu of `refactor.extract`, `refactor.inline` and
        // `refactor.rewrite`, only the last segment differs.
        let actions = CodeAction::list_from_json(&json!([
            {"title": "a", "kind": "refactor.extract.function"},
            {"title": "b", "kind": "quickfix"},
            {"title": "c"},
            {"title": "d", "kind": "trailing."},
        ]));
        assert_eq!(actions[0].short_kind(), Some("function"));
        assert_eq!(actions[1].short_kind(), Some("quickfix"));
        assert_eq!(actions[2].short_kind(), None);
        assert_eq!(
            actions[3].short_kind(),
            Some("trailing."),
            "a trailing dot leaves nothing to shorten to, so nothing is taken"
        );
    }

    #[test]
    fn rename_params_carry_the_new_name() {
        let params = rename_params(&uri(), Position::new(2, 4), "widget");
        assert_eq!(params["newName"], json!("widget"));
        assert_eq!(params["position"], json!({"line": 2, "character": 4}));
        assert_eq!(params["textDocument"]["uri"], json!("file:///w/a.rs"));
    }

    #[test]
    fn a_declined_rename_reads_as_nothing_to_do() {
        // `null` means there is nothing to rename here. This is not a failure:
        // the caller reports it and changes nothing.
        let edit = WorkspaceEdit::from_json(&json!(null)).expect("null is not an error");
        assert!(edit.is_empty());
        assert_eq!(edit.documents(), 0);
    }

    #[test]
    fn the_changes_map_reads_one_entry_per_document() {
        let edit = WorkspaceEdit::from_json(&json!({
            "changes": {
                "file:///w/a.rs": [
                    {"range": range(1, 0, 1, 3), "newText": "new"},
                    {"range": range(9, 0, 9, 3), "newText": "new"},
                ],
                "file:///w/b.rs": [{"range": range(0, 4, 0, 7), "newText": "new"}],
            }
        }))
        .expect("no file operations");

        assert_eq!(edit.documents(), 2);
        assert_eq!(edit.edits(), 3);
        // This form has no versions. The code that applies the edit must know
        // this, so that it does not treat "not stated" as "current".
        assert!(edit.changes.iter().all(|d| d.version.is_none()));
    }

    #[test]
    fn document_changes_win_over_changes() {
        // Servers send both for older clients. Reading `changes` would discard
        // the versions, which are the reason to prefer `documentChanges`.
        let edit = WorkspaceEdit::from_json(&json!({
            "changes": {
                "file:///w/stale.rs": [{"range": range(0, 0, 0, 1), "newText": "x"}],
            },
            "documentChanges": [{
                "textDocument": {"uri": "file:///w/a.rs", "version": 4},
                "edits": [{"range": range(1, 0, 1, 3), "newText": "new"}],
            }],
        }))
        .expect("no file operations");

        assert_eq!(edit.documents(), 1);
        assert_eq!(edit.changes[0].uri.as_str(), "file:///w/a.rs");
        assert_eq!(edit.changes[0].version, Some(4));
    }

    #[test]
    fn a_document_listed_twice_keeps_the_servers_order() {
        let edit = WorkspaceEdit::from_json(&json!({
            "documentChanges": [
                {
                    "textDocument": {"uri": "file:///w/a.rs", "version": 1},
                    "edits": [{"range": range(0, 0, 0, 1), "newText": "first"}],
                },
                {
                    "textDocument": {"uri": "file:///w/a.rs", "version": 1},
                    "edits": [{"range": range(5, 0, 5, 1), "newText": "second"}],
                },
            ],
        }))
        .expect("no file operations");

        assert_eq!(edit.documents(), 1, "one document, not two");
        let texts: Vec<&str> = edit.changes[0]
            .edits
            .iter()
            .map(|e| e.new_text.as_str())
            .collect();
        assert_eq!(texts, ["first", "second"]);
    }

    #[test]
    fn a_file_operation_refuses_the_whole_edit() {
        // rust-analyzer sends this when renaming a module: the file rename and
        // the text edits that refer to it. Applying only part of it is worse
        // than applying none of it.
        let error = WorkspaceEdit::from_json(&json!({
            "documentChanges": [
                {
                    "textDocument": {"uri": "file:///w/a.rs", "version": 1},
                    "edits": [{"range": range(0, 0, 0, 1), "newText": "new"}],
                },
                {"kind": "rename", "oldUri": "file:///w/a.rs", "newUri": "file:///w/b.rs"},
            ],
        }))
        .expect_err("a rename of a file cannot be honoured");

        assert_eq!(
            error,
            WorkspaceEditError::FileOperation("rename".to_owned())
        );
        assert!(
            error.to_string().contains("rename a file"),
            "the message names what it cannot do: {error}"
        );
    }

    #[test]
    fn a_document_with_no_edits_is_left_out() {
        let edit = WorkspaceEdit::from_json(&json!({
            "changes": {"file:///w/a.rs": []},
        }))
        .expect("no file operations");
        assert!(edit.is_empty());
        assert_eq!(edit.documents(), 0, "nothing to open and nothing to change");
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
        // Handling only the newest form would lose hover with many servers in
        // common use.
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
        // No empty hover box is shown under the cursor.
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
        // In unstyled text a fence is not useful, but the signature inside it
        // is the main content of the hover.
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
        // A corrupted signature is worse than a literal asterisk.
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
        // A different form, with `targetUri` and `targetRange`. rust-analyzer
        // and gopls both use it.
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
        // `jdt:` and similar schemes. The editor cannot open them, but dropping
        // the answer here would make it look as if the server returned nothing.
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
        // The trigger affects the suggestions: typing `.` should suggest
        // members, and ctrl+space on a blank line should suggest everything in
        // scope.
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
        // It can be neither shown nor chosen.
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
        // The protocol's precedence. Reversing it inserts the display text,
        // which for a function is often `foo(…)` with the ellipsis.
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
        // Completing over an existing word should replace it. Users expect
        // this, and `replace` exists for it.
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
        // deco advertises snippetSupport: false, but several servers send
        // snippets anyway. Inserting `foo(${1:arg})` literally is worse than
        // inserting nothing.
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
        // `${1|a,b,c|}` offers alternatives, and choosing one for the user would
        // be a guess.
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
        // There are no placeholders to strip, but the item is still flagged as
        // a snippet: the server may send tab stops in another item.
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
        // rust-analyzer labels an item `foo(…)` and filters on `foo`. Matching
        // against the label would fail as soon as the user typed `f`.
        let item =
            CompletionItem::from_json(&json!({"label": "foo(…)", "filterText": "foo"})).unwrap();
        assert_eq!(item.filter, "foo");

        let without = CompletionItem::from_json(&json!({"label": "bar"})).unwrap();
        assert_eq!(without.filter, "bar", "the label is the fallback");
    }

    #[test]
    fn sort_text_orders_ahead_of_the_label() {
        // Servers use it to put likely matches first. Ignoring it makes the
        // order look arbitrary.
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
        // Without these settings a server may indent with four spaces in a
        // two-space project, and the result changes every line.
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
        // Preserved rather than sorted here. `Transaction` already sorts and
        // applies back to front, and sorting in two places could produce
        // inconsistent results.
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
        // It has no position, so it cannot be applied. Dropping the whole set
        // would discard a formatting result because of one malformed entry.
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
        // Servers return these for an already formatted document. Applying one
        // marks the file dirty and adds an empty undo step.
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

    // ---- Semantic tokens --------------------------------------------------

    fn legend() -> crate::capabilities::SemanticTokensOptions {
        crate::capabilities::SemanticTokensOptions {
            token_types: vec![
                "namespace".to_owned(),
                "function".to_owned(),
                "variable".to_owned(),
            ],
            token_modifiers: vec![
                "declaration".to_owned(),
                "readonly".to_owned(),
                "static".to_owned(),
            ],
        }
    }

    #[test]
    fn a_single_token_decodes_to_an_absolute_range() {
        let spans = semantic_spans_from_json(&json!({ "data": [2, 4, 6, 1, 0] }), &legend());
        assert_eq!(
            spans,
            vec![SemanticSpan {
                range: Range::new(Position::new(2, 4), Position::new(2, 10)),
                token_type: "function".to_owned(),
                modifiers: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_second_token_on_the_same_line_counts_from_the_first() {
        // `deltaStart` is relative to the previous token when both are on the
        // same line. Reading it as relative to the line start would put this
        // token at column 3 instead of 8.
        let spans = semantic_spans_from_json(
            &json!({ "data": [0, 5, 2, 2, 0, 0, 3, 4, 2, 0] }),
            &legend(),
        );
        assert_eq!(spans[0].range.start, Position::new(0, 5));
        assert_eq!(spans[1].range.start, Position::new(0, 8));
        assert_eq!(spans[1].range.end, Position::new(0, 12));
    }

    #[test]
    fn a_token_on_a_later_line_counts_from_the_line_start() {
        let spans = semantic_spans_from_json(
            &json!({ "data": [0, 9, 2, 2, 0, 1, 3, 2, 2, 0] }),
            &legend(),
        );
        assert_eq!(spans[0].range.start, Position::new(0, 9));
        // Not 12: a new line resets the column origin.
        assert_eq!(spans[1].range.start, Position::new(1, 3));
    }

    #[test]
    fn lines_accumulate_across_tokens() {
        let spans = semantic_spans_from_json(
            &json!({ "data": [1, 0, 1, 2, 0, 2, 0, 1, 2, 0, 3, 0, 1, 2, 0] }),
            &legend(),
        );
        let lines: Vec<u32> = spans.iter().map(|s| s.range.start.line).collect();
        assert_eq!(lines, vec![1, 3, 6]);
    }

    #[test]
    fn modifiers_are_read_as_a_bitset_not_an_index() {
        // `5` is bits 0 and 2 (`declaration` and `static`), not the legend's
        // fifth entry, which does not exist.
        let spans = semantic_spans_from_json(&json!({ "data": [0, 0, 3, 2, 5] }), &legend());
        assert_eq!(spans[0].modifiers, vec!["declaration", "static"]);
    }

    #[test]
    fn no_modifier_bits_means_no_modifiers() {
        let spans = semantic_spans_from_json(&json!({ "data": [0, 0, 3, 2, 0] }), &legend());
        assert!(spans[0].modifiers.is_empty());
    }

    #[test]
    fn a_modifier_bit_with_no_name_is_ignored() {
        // Bit 7 is outside this legend. It is dropped instead of being given a
        // name, and the token itself is still usable.
        let spans = semantic_spans_from_json(&json!({ "data": [0, 0, 3, 2, 128] }), &legend());
        assert!(spans[0].modifiers.is_empty());
        assert_eq!(spans[0].token_type, "variable");
    }

    #[test]
    fn a_type_outside_the_legend_is_dropped_rather_than_guessed() {
        let spans = semantic_spans_from_json(&json!({ "data": [0, 0, 3, 99, 0] }), &legend());
        assert!(spans.is_empty());
    }

    #[test]
    fn a_dropped_token_still_advances_the_position() {
        // The deltas are relative to the previous *group*, not the previous
        // accepted span, so skipping one must not shift the next.
        let spans = semantic_spans_from_json(
            &json!({ "data": [0, 2, 3, 99, 0, 0, 4, 3, 2, 0] }),
            &legend(),
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].range.start, Position::new(0, 6));
    }

    #[test]
    fn a_zero_length_token_is_dropped() {
        let spans = semantic_spans_from_json(&json!({ "data": [0, 4, 0, 2, 0] }), &legend());
        assert!(spans.is_empty());
    }

    #[test]
    fn a_trailing_partial_group_is_ignored() {
        // An invalid response. Filling in the missing fields would create a
        // token.
        let spans = semantic_spans_from_json(&json!({ "data": [0, 0, 3, 2, 0, 0, 4] }), &legend());
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn an_answer_with_no_data_is_empty_rather_than_an_error() {
        for value in [json!({}), json!({ "data": [] }), json!(null), json!("no")] {
            assert!(
                semantic_spans_from_json(&value, &legend()).is_empty(),
                "{value}"
            );
        }
    }
    // ---- Document symbols ------------------------------------------------

    #[test]
    fn a_hierarchy_flattens_to_document_order() {
        let symbols = DocumentSymbol::list_from_json(&json!([
            {
                "name": "Counter",
                "kind": 23,
                "range": range(0, 0, 8, 1),
                "selectionRange": range(0, 11, 0, 18),
                "children": [
                    {
                        "name": "value",
                        "kind": 8,
                        "range": range(1, 4, 1, 20),
                        "selectionRange": range(1, 4, 1, 9),
                    },
                    {
                        "name": "bump",
                        "kind": 6,
                        "range": range(3, 4, 6, 5),
                        "selectionRange": range(3, 11, 3, 15),
                    },
                ],
            },
        ]));

        let names: Vec<String> = symbols.iter().map(DocumentSymbol::qualified).collect();
        assert_eq!(names, ["Counter", "Counter.value", "Counter.bump"]);
        assert_eq!(
            symbols.iter().map(|s| s.kind).collect::<Vec<_>>(),
            [Some("struct"), Some("field"), Some("method")]
        );
    }

    #[test]
    fn nesting_deeper_than_one_level_keeps_the_whole_path() {
        let symbols = DocumentSymbol::list_from_json(&json!([
            {
                "name": "outer",
                "kind": 3,
                "selectionRange": range(0, 0, 0, 5),
                "children": [{
                    "name": "middle",
                    "kind": 5,
                    "selectionRange": range(1, 0, 1, 6),
                    "children": [{
                        "name": "leaf",
                        "kind": 6,
                        "selectionRange": range(2, 0, 2, 4),
                    }],
                }],
            },
        ]));
        assert_eq!(
            symbols.last().map(DocumentSymbol::qualified).as_deref(),
            Some("outer.middle.leaf")
        );
    }

    #[test]
    fn a_symbol_is_positioned_on_its_name_not_its_definition() {
        // `range` covers the doc comment and the body; `selectionRange` is the
        // identifier. Using `range` would put the cursor on a comment.
        let symbols = DocumentSymbol::list_from_json(&json!([{
            "name": "scale",
            "kind": 12,
            "range": range(4, 0, 9, 1),
            "selectionRange": range(6, 3, 6, 8),
        }]));
        assert_eq!(symbols[0].position, Position::new(6, 3));
    }

    #[test]
    fn a_symbol_with_only_a_range_uses_it() {
        // The specification requires `selectionRange`, so this server does not
        // conform. The whole range still points to the correct region.
        let symbols = DocumentSymbol::list_from_json(&json!([{
            "name": "loose",
            "kind": 12,
            "range": range(2, 4, 3, 0),
        }]));
        assert_eq!(symbols[0].position, Position::new(2, 4));
    }

    #[test]
    fn the_flat_shape_reads_its_location_and_container() {
        let symbols = DocumentSymbol::list_from_json(&json!([
            {
                "name": "bump",
                "kind": 6,
                "containerName": "Counter",
                "location": {
                    "uri": "file:///w/counter.rs",
                    "range": range(3, 11, 3, 15),
                },
            },
            {
                "name": "free",
                "kind": 12,
                "containerName": "",
                "location": {
                    "uri": "file:///w/counter.rs",
                    "range": range(9, 3, 9, 7),
                },
            },
        ]));
        assert_eq!(
            symbols
                .iter()
                .map(DocumentSymbol::qualified)
                .collect::<Vec<_>>(),
            ["Counter.bump", "free"],
            "an empty containerName is no container, not a leading dot"
        );
        assert_eq!(symbols[0].position, Position::new(3, 11));
    }

    #[test]
    fn a_symbol_with_no_position_is_dropped() {
        // There is no navigation target, so the row would do nothing.
        let symbols = DocumentSymbol::list_from_json(&json!([
            { "name": "nowhere", "kind": 12 },
            { "name": "somewhere", "kind": 12, "selectionRange": range(1, 0, 1, 9) },
        ]));
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "somewhere");
    }

    #[test]
    fn a_nameless_symbol_is_dropped() {
        let symbols = DocumentSymbol::list_from_json(&json!([
            { "name": "", "kind": 12, "selectionRange": range(0, 0, 0, 1) },
            { "kind": 12, "selectionRange": range(1, 0, 1, 1) },
        ]));
        assert!(symbols.is_empty());
    }

    #[test]
    fn an_unknown_kind_still_lists_the_symbol() {
        // The server uses a newer specification than this client. The name is
        // still useful.
        let symbols = DocumentSymbol::list_from_json(&json!([
            { "name": "novel", "kind": 99, "selectionRange": range(0, 0, 0, 5) },
            { "name": "kindless", "selectionRange": range(1, 0, 1, 8) },
        ]));
        assert_eq!(symbols.len(), 2);
        assert!(symbols.iter().all(|s| s.kind.is_none()));
    }

    #[test]
    fn no_symbols_is_an_answer_rather_than_an_error() {
        for value in [json!(null), json!([]), json!({}), json!("nope")] {
            assert!(DocumentSymbol::list_from_json(&value).is_empty(), "{value}");
        }
    }

    /// A `{ start, end }` range, since every symbol test needs several.
    fn range(
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
    ) -> serde_json::Value {
        json!({
            "start": { "line": start_line, "character": start_character },
            "end": { "line": end_line, "character": end_character },
        })
    }
}
