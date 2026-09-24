//! Keeping a server's copy of a document identical to the editor's.
//!
//! Every server response refers to the server's copy of the document. If that
//! copy differs by one character, every diagnostic, completion and hover is
//! slightly wrong, and no error is reported. The following rules are therefore
//! enforced, not assumed:
//!
//! - A document must be opened before it is changed, and can be changed only
//!   while open. Violating either rule breaks the protocol, and servers react
//!   by crashing or by stopping their responses.
//! - The version increases by one on every change and is never reused. It is
//!   the only way to decide whether a server's response still applies to the
//!   current text.
//! - Incremental changes are sent in the order they were applied, because each
//!   range uses the coordinates produced by the previous change.

use std::collections::HashMap;

use deco_core::position::Range;

use crate::capabilities::TextDocumentSyncKind;
use crate::uri::Uri;

/// One edit as `textDocument/didChange` expresses it.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentChange {
    /// Replace everything.
    Full {
        /// The document's entire new text.
        text: String,
    },
    /// Replace one range.
    Incremental {
        /// The range being replaced, in the coordinates of the document *as it
        /// was before this change* — not before the batch.
        range: Range,
        /// What replaces it.
        text: String,
    },
}

impl ContentChange {
    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Full { text } => serde_json::json!({ "text": text }),
            Self::Incremental { range, text } => serde_json::json!({
                "range": range,
                "text": text,
            }),
        }
    }
}

/// A document the server has been told about.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenDocument {
    /// Its URI.
    pub uri: Uri,
    /// Its VS Code language identifier, e.g. `rust`.
    pub language_id: String,
    /// The version last sent. Starts at 1 and only increases.
    pub version: i32,
}

/// Why a synchronisation call was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    /// The document was already open.
    ///
    /// Two `didOpen`s for one URI leave the server with two copies, and later
    /// changes cannot be assigned to either one.
    #[error("{0} is already open")]
    AlreadyOpen(String),
    /// The document was never opened.
    #[error("{0} is not open")]
    NotOpen(String),
}

/// The set of documents a single server has been told about.
///
/// This is per server, not global. Two servers attached to the same file each
/// have their own version counter, and the counters can differ.
#[derive(Debug, Clone, Default)]
pub struct DocumentSync {
    open: HashMap<Uri, OpenDocument>,
}

impl DocumentSync {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the server has been told about this document.
    pub fn is_open(&self, uri: &Uri) -> bool {
        self.open.contains_key(uri)
    }

    /// The version last sent for a document.
    pub fn version(&self, uri: &Uri) -> Option<i32> {
        self.open.get(uri).map(|doc| doc.version)
    }

    /// How many documents are open.
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// Whether no documents are open.
    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// Every open document, in no particular order.
    pub fn documents(&self) -> impl Iterator<Item = &OpenDocument> {
        self.open.values()
    }

    /// Records a document as open and returns the `didOpen` parameters.
    pub fn open(
        &mut self,
        uri: Uri,
        language_id: impl Into<String>,
        text: &str,
    ) -> Result<serde_json::Value, SyncError> {
        if self.open.contains_key(&uri) {
            return Err(SyncError::AlreadyOpen(uri.as_str().to_owned()));
        }
        let language_id = language_id.into();
        // Version 1, not 0: VS Code's first version is 1, and a server that
        // initialises its own counter to 0 would treat the open as stale.
        let document = OpenDocument {
            uri: uri.clone(),
            language_id: language_id.clone(),
            version: 1,
        };
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": 1,
                "text": text,
            }
        });
        self.open.insert(uri, document);
        Ok(params)
    }

    /// Bumps the version and returns the `didChange` parameters.
    ///
    /// `full_text` is used when the server requested full syncs, and `changes`
    /// is then ignored. The caller therefore does not need to know which kind
    /// was negotiated.
    ///
    /// Returns `Ok(None)` when the server wants no change notifications. This
    /// is not an error: the version still advances, so a later diagnostic with
    /// an older version is still detected as stale.
    pub fn change(
        &mut self,
        uri: &Uri,
        kind: TextDocumentSyncKind,
        changes: &[ContentChange],
        full_text: &str,
    ) -> Result<Option<serde_json::Value>, SyncError> {
        let document = self
            .open
            .get_mut(uri)
            .ok_or_else(|| SyncError::NotOpen(uri.as_str().to_owned()))?;

        // Saturating, not wrapping. A negative version would compare as older
        // than every diagnostic already received, so every later result would
        // be discarded as stale and the editor would stop showing errors
        // without notice. Staying at the maximum is also incorrect, but
        // diagnostics keep working.
        document.version = document.version.saturating_add(1);
        let version = document.version;

        if kind == TextDocumentSyncKind::None {
            return Ok(None);
        }

        let content_changes: Vec<serde_json::Value> = match kind {
            TextDocumentSyncKind::Full => {
                vec![ContentChange::Full {
                    text: full_text.to_owned(),
                }
                .to_json()]
            }
            // Order is preserved because each range refers to the document as
            // left by the previous change.
            TextDocumentSyncKind::Incremental => {
                changes.iter().map(ContentChange::to_json).collect()
            }
            TextDocumentSyncKind::None => unreachable!("returned above"),
        };

        Ok(Some(serde_json::json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": content_changes,
        })))
    }

    /// Returns the `didSave` parameters, including the text if the server
    /// asked for it.
    pub fn save(
        &self,
        uri: &Uri,
        include_text: bool,
        text: &str,
    ) -> Result<serde_json::Value, SyncError> {
        if !self.open.contains_key(uri) {
            return Err(SyncError::NotOpen(uri.as_str().to_owned()));
        }
        let mut params = serde_json::json!({ "textDocument": { "uri": uri } });
        if include_text {
            params["text"] = serde_json::Value::String(text.to_owned());
        }
        Ok(params)
    }

    /// Forgets a document and returns the `didClose` parameters.
    pub fn close(&mut self, uri: &Uri) -> Result<serde_json::Value, SyncError> {
        self.open
            .remove(uri)
            .ok_or_else(|| SyncError::NotOpen(uri.as_str().to_owned()))?;
        Ok(serde_json::json!({ "textDocument": { "uri": uri } }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_core::position::Position;
    use serde_json::json;

    fn uri() -> Uri {
        Uri::from_string("file:///w/main.rs")
    }

    fn opened() -> DocumentSync {
        let mut sync = DocumentSync::new();
        sync.open(uri(), "rust", "fn main() {}").unwrap();
        sync
    }

    #[test]
    fn opening_reports_version_one() {
        let mut sync = DocumentSync::new();
        let params = sync.open(uri(), "rust", "fn main() {}").unwrap();
        assert_eq!(params["textDocument"]["version"], json!(1));
        assert_eq!(params["textDocument"]["languageId"], json!("rust"));
        assert_eq!(params["textDocument"]["uri"], json!("file:///w/main.rs"));
        assert_eq!(params["textDocument"]["text"], json!("fn main() {}"));
        assert_eq!(sync.version(&uri()), Some(1));
    }

    #[test]
    fn opening_twice_is_refused() {
        // The server would have two copies, and a later change could not be
        // assigned to either one.
        let mut sync = opened();
        assert_eq!(
            sync.open(uri(), "rust", "x"),
            Err(SyncError::AlreadyOpen("file:///w/main.rs".into()))
        );
    }

    #[test]
    fn changing_an_unopened_document_is_refused() {
        let mut sync = DocumentSync::new();
        assert_eq!(
            sync.change(&uri(), TextDocumentSyncKind::Full, &[], "x"),
            Err(SyncError::NotOpen("file:///w/main.rs".into()))
        );
    }

    #[test]
    fn a_closed_document_cannot_be_changed() {
        let mut sync = opened();
        sync.close(&uri()).unwrap();
        assert!(sync
            .change(&uri(), TextDocumentSyncKind::Full, &[], "x")
            .is_err());
        assert!(!sync.is_open(&uri()));
    }

    #[test]
    fn closing_twice_is_refused() {
        let mut sync = opened();
        sync.close(&uri()).unwrap();
        assert_eq!(
            sync.close(&uri()),
            Err(SyncError::NotOpen("file:///w/main.rs".into()))
        );
    }

    #[test]
    fn the_version_increases_by_one_per_change() {
        let mut sync = opened();
        for expected in 2..=5 {
            let params = sync
                .change(&uri(), TextDocumentSyncKind::Full, &[], "x")
                .unwrap()
                .unwrap();
            assert_eq!(params["textDocument"]["version"], json!(expected));
        }
        assert_eq!(sync.version(&uri()), Some(5));
    }

    #[test]
    fn a_full_sync_sends_the_whole_text_and_ignores_the_ranges() {
        let mut sync = opened();
        let changes = [ContentChange::Incremental {
            range: Range::new(Position::new(0, 0), Position::new(0, 2)),
            text: "hi".into(),
        }];
        let params = sync
            .change(
                &uri(),
                TextDocumentSyncKind::Full,
                &changes,
                "the whole file",
            )
            .unwrap()
            .unwrap();
        let content = params["contentChanges"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], json!("the whole file"));
        assert!(
            content[0].get("range").is_none(),
            "a full change must carry no range, or servers treat it as incremental"
        );
    }

    #[test]
    fn an_incremental_sync_preserves_the_order_of_the_edits() {
        // Each range refers to the document as left by the previous edit, so
        // reordering or deduplicating them corrupts the server's copy.
        let mut sync = opened();
        let changes = [
            ContentChange::Incremental {
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                text: "a".into(),
            },
            ContentChange::Incremental {
                range: Range::new(Position::new(0, 5), Position::new(0, 6)),
                text: "b".into(),
            },
        ];
        let params = sync
            .change(&uri(), TextDocumentSyncKind::Incremental, &changes, "?")
            .unwrap()
            .unwrap();
        let content = params["contentChanges"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], json!("a"));
        assert_eq!(content[1]["text"], json!("b"));
        assert_eq!(content[0]["range"]["end"]["character"], json!(1));
        assert_eq!(content[1]["range"]["start"]["character"], json!(5));
    }

    #[test]
    fn a_range_is_serialised_in_the_shape_lsp_expects() {
        // deco_core::Range must be serialised as {start:{line,character}, end:…},
        // not as a tuple. Servers reject any other shape as invalid params.
        let mut sync = opened();
        let changes = [ContentChange::Incremental {
            range: Range::new(Position::new(2, 4), Position::new(3, 0)),
            text: String::new(),
        }];
        let params = sync
            .change(&uri(), TextDocumentSyncKind::Incremental, &changes, "")
            .unwrap()
            .unwrap();
        assert_eq!(
            params["contentChanges"][0]["range"],
            json!({"start": {"line": 2, "character": 4}, "end": {"line": 3, "character": 0}})
        );
    }

    #[test]
    fn a_server_wanting_no_changes_gets_none_but_the_version_still_moves() {
        // The version is used to detect a late diagnostic as stale, so it must
        // advance even when nothing is sent.
        let mut sync = opened();
        assert_eq!(
            sync.change(&uri(), TextDocumentSyncKind::None, &[], "x"),
            Ok(None)
        );
        assert_eq!(sync.version(&uri()), Some(2));
    }

    #[test]
    fn save_includes_the_text_only_when_asked() {
        let sync = opened();
        let without = sync.save(&uri(), false, "fn main() {}").unwrap();
        assert!(without.get("text").is_none());

        let with = sync.save(&uri(), true, "fn main() {}").unwrap();
        assert_eq!(with["text"], json!("fn main() {}"));
    }

    #[test]
    fn saving_an_unopened_document_is_refused() {
        let sync = DocumentSync::new();
        assert!(sync.save(&uri(), false, "").is_err());
    }

    #[test]
    fn documents_are_tracked_independently() {
        let mut sync = DocumentSync::new();
        let a = Uri::from_string("file:///w/a.rs");
        let b = Uri::from_string("file:///w/b.rs");
        sync.open(a.clone(), "rust", "").unwrap();
        sync.open(b.clone(), "rust", "").unwrap();
        sync.change(&a, TextDocumentSyncKind::Full, &[], "x")
            .unwrap();

        assert_eq!(sync.version(&a), Some(2));
        assert_eq!(sync.version(&b), Some(1), "b's counter is its own");
        assert_eq!(sync.len(), 2);

        sync.close(&a).unwrap();
        assert!(!sync.is_open(&a));
        assert!(sync.is_open(&b));
        assert!(!sync.is_empty());
    }

    #[test]
    fn a_reopened_document_starts_over_at_version_one() {
        // Closing tells the server to discard the document, so its counter is
        // reset as well.
        let mut sync = opened();
        sync.change(&uri(), TextDocumentSyncKind::Full, &[], "x")
            .unwrap();
        sync.close(&uri()).unwrap();
        let params = sync.open(uri(), "rust", "fresh").unwrap();
        assert_eq!(params["textDocument"]["version"], json!(1));
    }

    #[test]
    fn the_language_id_is_remembered_for_each_document() {
        let mut sync = DocumentSync::new();
        let rs = Uri::from_string("file:///w/a.rs");
        let md = Uri::from_string("file:///w/b.md");
        sync.open(rs.clone(), "rust", "").unwrap();
        sync.open(md.clone(), "markdown", "").unwrap();

        let mut by_uri: Vec<_> = sync
            .documents()
            .map(|d| (d.uri.as_str(), d.language_id.as_str()))
            .collect();
        by_uri.sort();
        assert_eq!(
            by_uri,
            vec![("file:///w/a.rs", "rust"), ("file:///w/b.md", "markdown")]
        );
    }
}
