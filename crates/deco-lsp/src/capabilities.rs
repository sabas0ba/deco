//! What the editor tells a server it can do, and what the server answers.
//!
//! Two parts of this module need particular care.
//!
//! **Position encoding.** By default LSP counts a `character` in UTF-16 code
//! units, because VS Code is written in JavaScript. [`deco_core::Position`]
//! counts the same way, so the default needs no conversion. Servers may
//! negotiate UTF-8 instead. A mismatch is not visible at first: positions are
//! correct until the first line containing an emoji or a CJK character, after
//! which hovers are off by one character and edits are applied in the wrong
//! place. deco therefore advertises only the encodings it supports and treats
//! any other server choice as an error. See [`negotiate_encoding`].
//!
//! **Polymorphic capability fields.** The specification allows most provider
//! fields to be either a boolean or an options object, and `textDocumentSync`
//! to be either a number or a struct. Servers use every combination. Reading
//! `hoverProvider` only as a bool would disable hover for every server that
//! sends `{"workDoneProgress": true}`, which many servers do.

use serde::{Deserialize, Serialize};

/// How a server counts the `character` field of a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PositionEncoding {
    /// UTF-16 code units. The protocol default, and what deco uses internally.
    #[default]
    #[serde(rename = "utf-16")]
    Utf16,
    /// UTF-8 bytes.
    #[serde(rename = "utf-8")]
    Utf8,
    /// Unicode scalar values.
    #[serde(rename = "utf-32")]
    Utf32,
}

impl PositionEncoding {
    /// The spelling used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Utf16 => "utf-16",
            Self::Utf8 => "utf-8",
            Self::Utf32 => "utf-32",
        }
    }
}

/// Why negotiation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NegotiationError {
    /// The server chose an encoding the client never offered.
    ///
    /// This error is fatal. Otherwise every position sent to or received from
    /// this server would be wrong on any line containing a character the two
    /// encodings count differently: any non-ASCII character for UTF-8, or a
    /// character outside the Basic Multilingual Plane for UTF-32. Edits at
    /// those positions corrupt documents. The problem does not appear with
    /// ASCII-only text.
    #[error("server chose position encoding {chosen:?}, which was not offered ({offered})")]
    UnofferedEncoding {
        /// What the server asked for.
        chosen: String,
        /// What the client advertised, comma separated.
        offered: String,
    },
}

/// The encodings deco can speak, most preferred first.
///
/// One entry, because [`deco_core::Buffer`] indexes in UTF-16 and any other
/// encoding would need a conversion on every position in both directions. It
/// is still a list so that adding UTF-8 later only changes this constant, not
/// the negotiation logic.
pub const SUPPORTED_ENCODINGS: &[PositionEncoding] = &[PositionEncoding::Utf16];

/// Resolves the encoding from what a server put in its `initialize` result.
///
/// `None` means the server omitted the field, which the specification defines
/// as UTF-16 — not as "unknown".
pub fn negotiate_encoding(
    server_choice: Option<&str>,
) -> Result<PositionEncoding, NegotiationError> {
    let Some(chosen) = server_choice else {
        return Ok(PositionEncoding::Utf16);
    };
    SUPPORTED_ENCODINGS
        .iter()
        .copied()
        .find(|candidate| candidate.as_str() == chosen)
        .ok_or_else(|| NegotiationError::UnofferedEncoding {
            chosen: chosen.to_owned(),
            offered: SUPPORTED_ENCODINGS
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        })
}

/// How much of a document's text the server wants on each change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextDocumentSyncKind {
    /// The server wants no change notifications at all.
    None,
    /// The whole document on every change. This is the safe default when the
    /// server does not specify a kind.
    #[default]
    Full,
    /// Only the ranges that changed.
    Incremental,
}

impl TextDocumentSyncKind {
    fn from_number(value: i64) -> Option<Self> {
        Some(match value {
            0 => Self::None,
            1 => Self::Full,
            2 => Self::Incremental,
            _ => return None,
        })
    }
}

/// What a server said it can do, reduced to what deco acts on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerCapabilities {
    /// How the server counts positions.
    pub position_encoding: PositionEncoding,
    /// How much text to send on a change.
    pub sync_kind: TextDocumentSyncKind,
    /// Whether the server wants `didOpen`/`didClose` at all.
    pub open_close: bool,
    /// Whether the server wants `didSave`, and whether it wants the text with it.
    pub save: Option<SaveOptions>,
    /// `textDocument/hover`.
    pub hover: bool,
    /// `textDocument/definition`.
    pub definition: bool,
    /// `textDocument/references`.
    pub references: bool,
    /// `textDocument/completion`, and how it is triggered.
    pub completion: Option<CompletionOptions>,
    /// `textDocument/rename`, and whether `prepareRename` is available.
    pub rename: Option<RenameOptions>,
    /// `textDocument/formatting`.
    pub formatting: bool,
    /// `textDocument/documentSymbol`.
    pub document_symbol: bool,
    /// `textDocument/semanticTokens/full`, with the legend needed to read one.
    pub semantic_tokens: Option<SemanticTokensOptions>,
    /// `textDocument/codeAction`, and whether a chosen action can be resolved.
    pub code_action: Option<CodeActionOptions>,
}

/// How code actions are offered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodeActionOptions {
    /// Whether `codeAction/resolve` can fill in an action's edit.
    ///
    /// Servers that compute expensive refactorings send the titles first and
    /// the edit only for the chosen action. Without resolve support, deco
    /// cannot apply actions that arrive without an edit.
    pub resolve_provider: bool,
}

/// What a server offers for semantic tokens.
///
/// The legend is required in practice. The wire format uses integers, and the
/// server's lists are needed to map a `3` to `"function"`. If a server offers
/// the feature without a legend, the tokens cannot be interpreted, so the
/// feature is treated as not offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticTokensOptions {
    /// Token type names, indexed by the integer on the wire.
    pub token_types: Vec<String>,
    /// Modifier names, indexed by *bit position* in the modifier bitset.
    pub token_modifiers: Vec<String>,
}

/// What the server wants on save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SaveOptions {
    /// Whether the full text must accompany the notification.
    pub include_text: bool,
}

/// How completion is triggered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionOptions {
    /// Characters that open a completion list automatically, e.g. `.` and
    /// `::`.
    pub trigger_characters: Vec<String>,
    /// Whether a selected item must be sent back to `completionItem/resolve`
    /// before its documentation and edits are known.
    pub resolve_provider: bool,
}

/// How rename is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenameOptions {
    /// Whether `textDocument/prepareRename` can validate the cursor position
    /// before the user is asked for a new name.
    pub prepare_provider: bool,
}

impl ServerCapabilities {
    /// Reads the `capabilities` object of an `initialize` result.
    ///
    /// Never fails. An unrecognised or malformed field is treated as not
    /// offered, and a missing feature disables only that feature instead of
    /// rejecting the connection. The caller checks [`negotiate_encoding`], the
    /// only part that *can* fail, separately, because an encoding error
    /// corrupts documents instead of only disabling a feature.
    pub fn from_json(value: &serde_json::Value) -> Self {
        let sync = value.get("textDocumentSync");
        let (sync_kind, open_close, save) = read_sync(sync);

        Self {
            position_encoding: value
                .get("positionEncoding")
                .and_then(|v| v.as_str())
                .and_then(|s| negotiate_encoding(Some(s)).ok())
                .unwrap_or_default(),
            sync_kind,
            open_close,
            save,
            hover: is_provider(value.get("hoverProvider")),
            definition: is_provider(value.get("definitionProvider")),
            references: is_provider(value.get("referencesProvider")),
            // `.map` over the raw lookup would be wrong. `"completionProvider":
            // null` is present but disabled, and would otherwise be read as an
            // offer with no options.
            completion: value
                .get("completionProvider")
                .filter(|v| !v.is_null())
                .map(|options| CompletionOptions {
                    trigger_characters: options
                        .get("triggerCharacters")
                        .and_then(|v| v.as_array())
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                    resolve_provider: options
                        .get("resolveProvider")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                }),
            rename: read_rename(value.get("renameProvider")),
            formatting: is_provider(value.get("documentFormattingProvider")),
            document_symbol: is_provider(value.get("documentSymbolProvider")),
            semantic_tokens: read_semantic_tokens(value.get("semanticTokensProvider")),
            code_action: read_code_action(value.get("codeActionProvider")),
        }
    }
}

/// Reads `semanticTokensProvider`.
///
/// `None` unless the server offers the **full** document request and a legend.
/// deco does not use `range`-only support. It highlights the visible lines of a
/// document it has already lexed, and range requests would require a request
/// on every scroll.
fn read_semantic_tokens(value: Option<&serde_json::Value>) -> Option<SemanticTokensOptions> {
    let options = value.filter(|v| !v.is_null())?;
    // `full` is `boolean | { delta?: boolean }`, and absent means not offered.
    let full = match options.get("full") {
        Some(serde_json::Value::Bool(enabled)) => *enabled,
        Some(serde_json::Value::Object(_)) => true,
        _ => false,
    };
    if !full {
        return None;
    }
    let legend = options.get("legend")?;
    let names = |key: &str| -> Vec<String> {
        legend
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let token_types = names("tokenTypes");
    if token_types.is_empty() {
        // No type could be resolved, so the feature would produce spans with no
        // type. Report it as unavailable instead of colouring by index.
        return None;
    }
    Some(SemanticTokensOptions {
        token_types,
        token_modifiers: names("tokenModifiers"),
    })
}

/// A provider field is `true`, or an options object, or absent/`false`.
fn is_provider(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(enabled)) => *enabled,
        // An options object means the feature is enabled with options. This
        // function exists so that callers do not read only the boolean form.
        Some(serde_json::Value::Object(_)) => true,
        Some(_) => false,
    }
}

fn read_code_action(value: Option<&serde_json::Value>) -> Option<CodeActionOptions> {
    match value {
        Some(serde_json::Value::Bool(true)) => Some(CodeActionOptions::default()),
        Some(serde_json::Value::Object(options)) => Some(CodeActionOptions {
            resolve_provider: options
                .get("resolveProvider")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }),
        // Including `codeActionKinds` without `resolveProvider`, which is an
        // object and so still means the server offers code actions. Only `false`
        // and absence mean it does not.
        _ => None,
    }
}

fn read_rename(value: Option<&serde_json::Value>) -> Option<RenameOptions> {
    match value {
        Some(serde_json::Value::Bool(true)) => Some(RenameOptions::default()),
        Some(serde_json::Value::Object(options)) => Some(RenameOptions {
            prepare_provider: options
                .get("prepareProvider")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }),
        _ => None,
    }
}

/// `textDocumentSync` is either a number or an object; both are common.
fn read_sync(
    value: Option<&serde_json::Value>,
) -> (TextDocumentSyncKind, bool, Option<SaveOptions>) {
    match value {
        Some(serde_json::Value::Number(n)) => {
            let kind = n
                .as_i64()
                .and_then(TextDocumentSyncKind::from_number)
                .unwrap_or_default();
            // The short form does not specify open/close or save. Per the
            // specification, open and close are still sent and save is not.
            (kind, kind != TextDocumentSyncKind::None, None)
        }
        Some(serde_json::Value::Object(options)) => {
            let kind = options
                .get("change")
                .and_then(|v| v.as_i64())
                .and_then(TextDocumentSyncKind::from_number)
                .unwrap_or(TextDocumentSyncKind::None);
            let open_close = options
                .get("openClose")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let save = match options.get("save") {
                Some(serde_json::Value::Bool(true)) => Some(SaveOptions::default()),
                Some(serde_json::Value::Object(save)) => Some(SaveOptions {
                    include_text: save
                        .get("includeText")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                }),
                _ => None,
            };
            (kind, open_close, save)
        }
        // A server that does not specify a kind gets full syncs. Sending too
        // much text is slow, but sending too little leaves the server with
        // stale source.
        _ => (TextDocumentSyncKind::Full, true, None),
    }
}

/// The `capabilities` object deco sends in `initialize`.
///
/// Every request deco sends declares its capability here, and nothing deco
/// does not implement is advertised.
///
/// A missing declaration changes what servers send. Without
/// `codeActionLiteralSupport` a server may answer `textDocument/codeAction`
/// with `Command[]` only, which deco cannot run, and without
/// `workspaceEdit.documentChanges` edits carry no document version, so stale
/// edits cannot be detected. An extra declaration causes servers to send
/// messages the editor drops. For example, a server told that the client
/// handles `workspace/applyEdit` applies refactorings by sending that
/// request, and nothing happens.
pub fn client_capabilities() -> serde_json::Value {
    serde_json::json!({
        "general": {
            "positionEncodings": SUPPORTED_ENCODINGS
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>(),
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": false,
                "willSave": false,
                "willSaveWaitUntil": false,
                "didSave": true,
            },
            "hover": {
                "dynamicRegistration": false,
                // Plain text only. deco has no Markdown renderer yet, and
                // Markdown content would be displayed as unrendered syntax.
                "contentFormat": ["plaintext"],
            },
            "completion": {
                "dynamicRegistration": false,
                "completionItem": {
                    "snippetSupport": false,
                    "documentationFormat": ["plaintext"],
                },
                "contextSupport": true,
            },
            "definition": { "dynamicRegistration": false },
            "references": { "dynamicRegistration": false },
            "formatting": { "dynamicRegistration": false },
            "rangeFormatting": { "dynamicRegistration": false },
            "documentSymbol": {
                "dynamicRegistration": false,
                // Both result shapes are read; the tree keeps the nesting.
                "hierarchicalDocumentSymbolSupport": true,
            },
            // deco does not send `textDocument/prepareRename`.
            "rename": { "dynamicRegistration": false, "prepareSupport": false },
            "codeAction": {
                "dynamicRegistration": false,
                // Without this a server may send only `Command[]`, which
                // needs `workspace/executeCommand` (LSP 3.17,
                // textDocument/codeAction).
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": [
                            "",
                            "quickfix",
                            "refactor",
                            "refactor.extract",
                            "refactor.inline",
                            "refactor.rewrite",
                            "source",
                            "source.organizeImports",
                        ],
                    },
                },
                // `isPreferredSupport` is not declared: `isPreferred` is
                // parsed but does not affect the list.
                "disabledSupport": true,
                // `data` is kept in the raw action sent to `codeAction/resolve`.
                "dataSupport": true,
                "resolveSupport": { "properties": ["edit"] },
            },
            "semanticTokens": {
                "dynamicRegistration": false,
                // Only `textDocument/semanticTokens/full` is sent.
                "requests": { "full": true },
                // The standard 3.17 lists. Types and modifiers are matched by
                // name against the theme's `semanticTokenColors`, so any of
                // them can be coloured.
                "tokenTypes": [
                    "namespace", "type", "class", "enum", "interface", "struct",
                    "typeParameter", "parameter", "variable", "property",
                    "enumMember", "event", "function", "method", "macro",
                    "keyword", "modifier", "comment", "string", "number",
                    "regexp", "operator", "decorator",
                ],
                "tokenModifiers": [
                    "declaration", "definition", "readonly", "static",
                    "deprecated", "abstract", "async", "modification",
                    "documentation", "defaultLibrary",
                ],
                "formats": ["relative"],
            },
            "publishDiagnostics": {
                "relatedInformation": true,
                // Requested because it is needed to detect stale
                // diagnostics; see the diagnostics module.
                "versionSupport": true,
            },
        },
        "workspace": {
            // `applyEdit` is not declared: deco declines server-initiated
            // edits. `resourceOperations` is not declared: file operations
            // are refused.
            "workspaceEdit": {
                // Each `TextDocumentEdit` carries the version the edit was
                // computed for, which the stale-edit check needs.
                "documentChanges": true,
            },
        },
        "window": {
            "workDoneProgress": false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_absent_encoding_means_utf16() {
        // The specification's default, not "unknown".
        assert_eq!(negotiate_encoding(None), Ok(PositionEncoding::Utf16));
    }

    #[test]
    fn the_offered_encoding_is_accepted() {
        assert_eq!(
            negotiate_encoding(Some("utf-16")),
            Ok(PositionEncoding::Utf16)
        );
    }

    #[test]
    fn an_encoding_that_was_never_offered_is_fatal() {
        // No fallback. Accepting utf-8 while indexing in utf-16 misplaces
        // every position after the first non-ASCII character on a line.
        let Err(NegotiationError::UnofferedEncoding { chosen, offered }) =
            negotiate_encoding(Some("utf-8"))
        else {
            panic!("utf-8 must be refused while it is unimplemented");
        };
        assert_eq!(chosen, "utf-8");
        assert!(
            offered.contains("utf-16"),
            "the error says what was offered"
        );
    }

    #[test]
    fn nonsense_is_refused_rather_than_defaulted() {
        assert!(negotiate_encoding(Some("ebcdic")).is_err());
    }

    #[test]
    fn the_advertised_encodings_match_what_is_accepted() {
        // The two lists must stay consistent. Advertising an encoding that
        // negotiation rejects would break every server that selects it.
        let advertised = client_capabilities()["general"]["positionEncodings"].clone();
        for value in advertised.as_array().unwrap() {
            assert!(
                negotiate_encoding(value.as_str()).is_ok(),
                "{value} is advertised but not accepted"
            );
        }
    }

    #[test]
    fn a_provider_object_counts_as_enabled() {
        // Many servers send `{"workDoneProgress": true}` instead of `true`, and
        // reading only the boolean would disable the feature.
        let caps = ServerCapabilities::from_json(&json!({
            "hoverProvider": {"workDoneProgress": true},
            "definitionProvider": true,
            "referencesProvider": {},
        }));
        assert!(caps.hover);
        assert!(caps.definition);
        assert!(caps.references);
    }

    #[test]
    fn an_absent_or_false_provider_counts_as_disabled() {
        let caps = ServerCapabilities::from_json(&json!({"hoverProvider": false}));
        assert!(!caps.hover);
        assert!(!caps.definition, "absent is disabled");
    }

    #[test]
    fn sync_kind_reads_the_numeric_form() {
        let caps = ServerCapabilities::from_json(&json!({"textDocumentSync": 2}));
        assert_eq!(caps.sync_kind, TextDocumentSyncKind::Incremental);
        assert!(caps.open_close, "the short form still wants open and close");
        assert_eq!(caps.save, None);
    }

    #[test]
    fn sync_kind_reads_the_object_form() {
        let caps = ServerCapabilities::from_json(&json!({
            "textDocumentSync": {
                "openClose": true,
                "change": 1,
                "save": {"includeText": true},
            }
        }));
        assert_eq!(caps.sync_kind, TextDocumentSyncKind::Full);
        assert!(caps.open_close);
        assert_eq!(caps.save, Some(SaveOptions { include_text: true }));
    }

    #[test]
    fn save_as_a_bare_true_means_no_text() {
        let caps = ServerCapabilities::from_json(&json!({
            "textDocumentSync": {"openClose": true, "change": 2, "save": true}
        }));
        assert_eq!(
            caps.save,
            Some(SaveOptions {
                include_text: false
            })
        );
    }

    #[test]
    fn a_server_that_says_nothing_gets_full_syncs() {
        // Sending too much is preferred. Otherwise the server would answer
        // requests about source that no longer exists.
        let caps = ServerCapabilities::from_json(&json!({}));
        assert_eq!(caps.sync_kind, TextDocumentSyncKind::Full);
        assert!(caps.open_close);
    }

    #[test]
    fn an_object_form_without_change_means_no_change_notifications() {
        // Unlike an absent `textDocumentSync`, the server did specify sync
        // options here and did not request changes.
        let caps = ServerCapabilities::from_json(&json!({
            "textDocumentSync": {"openClose": true}
        }));
        assert_eq!(caps.sync_kind, TextDocumentSyncKind::None);
    }

    #[test]
    fn an_out_of_range_sync_kind_falls_back_to_full() {
        let caps = ServerCapabilities::from_json(&json!({"textDocumentSync": 99}));
        assert_eq!(caps.sync_kind, TextDocumentSyncKind::Full);
    }

    #[test]
    fn completion_carries_its_trigger_characters() {
        let caps = ServerCapabilities::from_json(&json!({
            "completionProvider": {
                "triggerCharacters": [".", "::"],
                "resolveProvider": true,
            }
        }));
        let completion = caps.completion.expect("completion is offered");
        assert_eq!(completion.trigger_characters, vec![".", "::"]);
        assert!(completion.resolve_provider);
    }

    #[test]
    fn completion_without_options_is_still_offered() {
        let caps = ServerCapabilities::from_json(&json!({"completionProvider": {}}));
        let completion = caps.completion.expect("an empty object still offers it");
        assert!(completion.trigger_characters.is_empty());
        assert!(!completion.resolve_provider);
    }

    #[test]
    fn code_actions_distinguish_resolve_support() {
        assert_eq!(
            ServerCapabilities::from_json(&json!({"codeActionProvider": true})).code_action,
            Some(CodeActionOptions {
                resolve_provider: false
            })
        );
        assert_eq!(
            ServerCapabilities::from_json(&json!({
                "codeActionProvider": {"resolveProvider": true}
            }))
            .code_action,
            Some(CodeActionOptions {
                resolve_provider: true
            })
        );
        // An object that only lists kinds still offers code actions. The
        // server sends every edit up front.
        assert_eq!(
            ServerCapabilities::from_json(&json!({
                "codeActionProvider": {"codeActionKinds": ["quickfix"]}
            }))
            .code_action,
            Some(CodeActionOptions {
                resolve_provider: false
            })
        );
        assert_eq!(
            ServerCapabilities::from_json(&json!({"codeActionProvider": false})).code_action,
            None
        );
        assert_eq!(
            ServerCapabilities::from_json(&json!({})).code_action,
            None,
            "absent is the same as declined"
        );
    }

    #[test]
    fn rename_distinguishes_prepare_support() {
        assert_eq!(
            ServerCapabilities::from_json(&json!({"renameProvider": true})).rename,
            Some(RenameOptions {
                prepare_provider: false
            })
        );
        assert_eq!(
            ServerCapabilities::from_json(&json!({
                "renameProvider": {"prepareProvider": true}
            }))
            .rename,
            Some(RenameOptions {
                prepare_provider: true
            })
        );
        assert_eq!(
            ServerCapabilities::from_json(&json!({"renameProvider": false})).rename,
            None
        );
    }

    #[test]
    fn a_malformed_capabilities_object_disables_features_rather_than_failing() {
        // Invalid capabilities disable only that server's features. Opening
        // files is unaffected.
        let caps = ServerCapabilities::from_json(&json!({
            "hoverProvider": "yes please",
            "textDocumentSync": "full",
            "completionProvider": null,
        }));
        assert!(!caps.hover);
        assert_eq!(caps.sync_kind, TextDocumentSyncKind::Full);
        assert_eq!(caps.completion, None);
    }

    #[test]
    fn nothing_unimplemented_is_advertised() {
        // Advertising a feature deco does not implement causes servers to send
        // messages that deco drops. To the user, the server appears broken.
        let caps = client_capabilities();
        assert_eq!(
            caps["textDocument"]["synchronization"]["willSave"],
            json!(false)
        );
        assert_eq!(
            caps["textDocument"]["completion"]["completionItem"]["snippetSupport"],
            json!(false),
            "snippets stay undeclared until the full syntax is implemented"
        );
        assert_eq!(caps["window"]["workDoneProgress"], json!(false));
        assert!(
            matches!(
                caps["workspace"].get("applyEdit"),
                None | Some(serde_json::Value::Bool(false))
            ),
            "deco declines server-initiated workspace/applyEdit"
        );
        assert!(
            caps["workspace"]["workspaceEdit"]
                .get("resourceOperations")
                .is_none(),
            "file operations in a workspace edit are refused"
        );
        assert_eq!(
            caps["textDocument"]["rename"]["prepareSupport"],
            json!(false),
            "deco does not send textDocument/prepareRename"
        );
        assert!(
            caps["textDocument"]["semanticTokens"]["requests"]
                .get("range")
                .is_none(),
            "only full-document semantic tokens are requested"
        );
    }

    #[test]
    fn every_request_deco_sends_is_declared() {
        let caps = client_capabilities();
        let text_document = &caps["textDocument"];
        for key in [
            "hover",
            "completion",
            "definition",
            "references",
            "formatting",
            "rangeFormatting",
            "documentSymbol",
            "rename",
            "codeAction",
            "semanticTokens",
        ] {
            assert!(
                text_document.get(key).is_some(),
                "textDocument.{key} is not declared"
            );
        }
        let kinds =
            &text_document["codeAction"]["codeActionLiteralSupport"]["codeActionKind"]["valueSet"];
        assert!(
            kinds.as_array().is_some_and(|kinds| !kinds.is_empty()),
            "without code action literals a server may send only commands"
        );
        assert_eq!(
            text_document["codeAction"]["resolveSupport"]["properties"],
            json!(["edit"])
        );
        assert_eq!(
            text_document["semanticTokens"]["requests"]["full"],
            json!(true)
        );
        assert_eq!(
            caps["workspace"]["workspaceEdit"]["documentChanges"],
            json!(true),
            "versioned edits are needed for the stale-edit check"
        );
    }

    #[test]
    fn only_plain_text_is_requested_while_there_is_no_markdown_renderer() {
        let caps = client_capabilities();
        assert_eq!(
            caps["textDocument"]["hover"]["contentFormat"],
            json!(["plaintext"])
        );
    }

    #[test]
    fn diagnostic_version_support_is_requested() {
        // Without it a server is not required to include a version in
        // diagnostics, and stale results cannot be distinguished from current
        // ones.
        assert_eq!(
            client_capabilities()["textDocument"]["publishDiagnostics"]["versionSupport"],
            json!(true)
        );
    }
}
