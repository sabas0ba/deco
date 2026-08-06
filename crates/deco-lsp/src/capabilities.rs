//! What the editor tells a server it can do, and what the server answers.
//!
//! Two things here are worth more attention than their size suggests.
//!
//! **Position encoding.** LSP counts a `character` in UTF-16 code units by
//! default — a decision inherited from VS Code being written in JavaScript —
//! and [`deco_core::Position`] counts the same way, so the default costs
//! nothing. Servers may negotiate UTF-8 instead, and a mismatch is invisible
//! until it is not: every position is right until the first line containing an
//! emoji or a CJK character, and then hovers land one character off and edits
//! land in the wrong place. deco therefore advertises exactly what it can
//! honour and treats a server that answers with something else as an error
//! rather than guessing. See [`negotiate_encoding`].
//!
//! **Polymorphic capability fields.** The specification lets a server answer
//! most provider fields with either a boolean or an options object, and
//! `textDocumentSync` with either a number or a struct. Real servers use every
//! combination. Reading `hoverProvider` as a bool alone means silently
//! disabling hover for every server that sends `{"workDoneProgress": true}`,
//! which is a large fraction of them.

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
    /// Fatal on purpose. Continuing would mean every position sent to or
    /// received from this server is wrong on any line containing a character
    /// outside the Basic Multilingual Plane — which is a corruption bug, and
    /// one that only shows up in other people's languages.
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
/// One entry, because [`deco_core::Buffer`] indexes in UTF-16 and anything else
/// would need a conversion on every position in both directions. The list shape
/// is kept so that adding UTF-8 later is a change to this constant rather than
/// to the negotiation logic.
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
    /// The whole document on every change. Simple, and the safe default for a
    /// server that did not say.
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
    /// Characters that open a completion list without the user asking, e.g.
    /// `.` and `::`.
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
    /// Never fails: an unrecognised or malformed field means "the server did
    /// not offer this", and a missing feature degrades the editor rather than
    /// refusing the connection. The one thing that *can* fail —
    /// [`negotiate_encoding`] — is checked separately by the caller, because
    /// getting it wrong corrupts documents rather than merely disabling a menu.
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
            // `.map` over the raw lookup would be wrong: `"completionProvider":
            // null` is present-but-disabled, and would otherwise be read as an
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
        }
    }
}

/// A provider field is `true`, or an options object, or absent/`false`.
fn is_provider(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(enabled)) => *enabled,
        // An options object means the feature is on and configured. Reading
        // only the boolean form here is the bug this function exists to avoid.
        Some(serde_json::Value::Object(_)) => true,
        Some(_) => false,
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
            // The short form says nothing about open/close or save. The
            // specification's answer is that open and close are still sent;
            // save is not.
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
        // A server that says nothing gets full syncs. Sending too much text is
        // slow; sending too little means it is reasoning about stale source.
        _ => (TextDocumentSyncKind::Full, true, None),
    }
}

/// The `capabilities` object deco sends in `initialize`.
///
/// Only features that are actually implemented are advertised. Claiming more
/// invites a server to send messages the editor will drop — a server told the
/// client handles `workspace/applyEdit` will apply refactorings by sending one,
/// and silently nothing happens.
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
                // Plain text only: deco has no Markdown renderer yet, and a
                // server told otherwise sends unrendered syntax to display.
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
            "publishDiagnostics": {
                "relatedInformation": true,
                // Asked for because it is what makes a stale diagnostic
                // detectable; see the diagnostics module.
                "versionSupport": true,
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
        // Not a fallback: accepting utf-8 while indexing in utf-16 misplaces
        // every position past the first non-BMP character on a line.
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
        // The two must not drift: advertising an encoding that negotiation then
        // refuses would break every server that takes the offer.
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
        // The common failure: many servers answer `{"workDoneProgress": true}`
        // instead of `true`, and reading only the boolean disables the feature.
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
        // Erring towards sending too much: the alternative is a server
        // answering questions about source that no longer exists.
        let caps = ServerCapabilities::from_json(&json!({}));
        assert_eq!(caps.sync_kind, TextDocumentSyncKind::Full);
        assert!(caps.open_close);
    }

    #[test]
    fn an_object_form_without_change_means_no_change_notifications() {
        // Unlike an absent `textDocumentSync` entirely: here the server did
        // answer, and it did not ask for changes.
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
        // A server sending nonsense should cost its own features, not the
        // editor's ability to open a file.
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
        // Advertising a feature deco does not implement invites messages it
        // will drop on the floor, which looks to the user like the server is
        // broken.
        let caps = client_capabilities();
        assert_eq!(
            caps["textDocument"]["synchronization"]["willSave"],
            json!(false)
        );
        assert_eq!(
            caps["textDocument"]["completion"]["completionItem"]["snippetSupport"],
            json!(false),
            "a snippet inserted literally is worse than no snippet"
        );
        assert_eq!(caps["window"]["workDoneProgress"], json!(false));
        assert!(
            caps.get("workspace").is_none(),
            "no workspace edits until they can be applied"
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
        // Without it a server need not stamp diagnostics with a version, and
        // stale results cannot be told from fresh ones.
        assert_eq!(
            client_capabilities()["textDocument"]["publishDiagnostics"]["versionSupport"],
            json!(true)
        );
    }
}
