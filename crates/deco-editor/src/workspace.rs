//! One edit across several documents, applied all at once or not at all.
//!
//! A language server's response to `textDocument/rename` contains replacements
//! for every file that mentions the symbol, most of which are not open. Code
//! actions, replace-across-files and agent turns use the same structure. Unlike
//! single-document edits, these must not be applied partially. Half a rename
//! leaves a project that does not build, and if the parts were separate undo
//! steps, no single undo would restore it.
//!
//! This module therefore works in two phases, and never starts the second before
//! the first succeeds:
//!
//! 1. **Check that everything can be applied.** Every document is resolved,
//!    every version the server stated is checked, and every transaction is
//!    *built*, which detects overlapping edits, before any buffer is changed.
//!    A failure at any point leaves the session unchanged.
//! 2. **Apply everything**, recording one shared [`deco_core::Group`] across
//!    all affected documents, so that `ctrl+z` in any of them reverts the whole
//!    edit.
//!
//! # Files that are not open
//!
//! Most of a rename usually affects files that are not open. VS Code writes
//! those directly to disk. deco opens them instead, as background tabs with
//! unsaved changes, for two reasons. First, the core performs no I/O, which is
//! what makes the editable surface testable headlessly. A write here would have
//! to be a request to the frontend, and its undo another request. Second, deco
//! does not modify files on disk on behalf of another program without an
//! explicit save. When the files are opened instead of written, the change is
//! visible, `ctrl+z` can revert it, and `ctrl+k s` saves it.
//!
//! The frontend supplies the text because reading the file is I/O. [`Plan`]
//! lists the paths it needs and [`Plan::with_contents`] receives their text.

use std::path::{Path, PathBuf};

use deco_lsp::requests::{TextEdit, WorkspaceEdit};
use deco_lsp::uri::Uri;

/// Why a workspace edit was refused.
///
/// Every variant means nothing was changed. No error leaves a session partially
/// edited.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// A URI that does not name a file on this machine.
    ///
    /// `untitled:` documents and server-specific synthetic schemes produce this
    /// error. The whole edit is rejected rather than skipping these documents,
    /// because the remaining edits are only valid as a complete change.
    #[error("the server sent an edit for `{0}`, which is not a file deco can open")]
    NotAFile(String),
    /// The document changed after the server computed the edit.
    ///
    /// Positions are valid only for the text they were computed against. If a
    /// keystroke has moved the text down a line, applying the rename would
    /// replace whatever is now at those coordinates.
    #[error("`{path}` has changed since the server read it — nothing was renamed")]
    Stale {
        /// Which document.
        path: PathBuf,
        /// The version the server was working from.
        expected: i64,
        /// The version the document is actually at.
        actual: i64,
    },
    /// Two edits for one document covered the same text.
    #[error("the server sent overlapping edits for `{path}`, which have no well-defined result")]
    Overlapping {
        /// Which document.
        path: PathBuf,
    },
    /// A file the edit needs was not open and its text was not supplied.
    #[error("`{path}` could not be read: {reason}")]
    Unreadable {
        /// Which document.
        path: PathBuf,
        /// What the frontend said went wrong.
        reason: String,
    },
    /// A replace-in-files query is not a valid regular expression.
    #[error(transparent)]
    InvalidPattern(#[from] deco_core::search::PatternError),
}

/// One document's share of a workspace edit, resolved to a path.
#[derive(Debug, Clone)]
pub struct PlannedDocument {
    /// Where the file is.
    pub path: PathBuf,
    /// The version the server computed against, when it said.
    pub version: Option<i64>,
    /// What to change.
    pub edits: Vec<TextEdit>,
    /// Whether a tab already holds this file.
    pub open: bool,
    /// The text to start from, for a file no tab holds.
    ///
    /// Filled in by [`Plan::with_contents`]. Always `None` for a file that is
    /// already open, because its buffer must be the starting point. Editing from
    /// the text on disk would discard any unsaved changes.
    pub contents: Option<String>,
}

/// A workspace edit, checked as far as it can be without touching a buffer.
///
/// A plan is not guaranteed to be applicable.
/// [`crate::Session::apply_workspace_edit`] re-checks everything that could have
/// changed in the meantime, so the plan can be passed to a frontend to fill in
/// and then returned.
#[derive(Debug, Clone)]
pub struct Plan {
    documents: Vec<PlannedDocument>,
}

impl Plan {
    /// Resolves `edit` against the paths and versions of what is open.
    ///
    /// `version_of` returns the version last sent to the language server for a
    /// path, or `None` for a document the client is not tracking. It is a
    /// callback because the versions are held by the LSP client in a frontend.
    /// The rule for handling a mismatch is defined here so that every caller
    /// uses the same rule.
    ///
    /// If the server states no version, no check is made. This is different
    /// from a passed check. The `changes` form of a workspace edit carries no
    /// versions, so an edit in that form is applied without a version check.
    pub(crate) fn build(
        edit: &WorkspaceEdit,
        resolve: impl Fn(&Uri) -> Option<PathBuf>,
        is_open: impl Fn(&Path) -> bool,
        version_of: impl Fn(&Path) -> Option<i64>,
    ) -> Result<Self, WorkspaceError> {
        let mut documents = Vec::with_capacity(edit.changes.len());
        for change in &edit.changes {
            // Resolved by the caller rather than by `Uri::to_path`. A server in
            // the remote environment of a remote session uses remote paths, and
            // the session owns the mapping to local paths. Using the wrong side
            // of the mapping would edit files on the wrong machine.
            let path = resolve(&change.uri)
                .ok_or_else(|| WorkspaceError::NotAFile(change.uri.as_str().to_owned()))?;

            if let (Some(expected), Some(actual)) = (change.version, version_of(&path)) {
                if expected != actual {
                    return Err(WorkspaceError::Stale {
                        path,
                        expected,
                        actual,
                    });
                }
            }

            // Two URIs can name the same file because percent-encoding is not
            // unique. Their edits are merged into one document and one
            // transaction. Opening the file twice would create two buffers for
            // one path, which tabs already prevent.
            if let Some(seen) = documents
                .iter_mut()
                .find(|seen: &&mut PlannedDocument| seen.path == path)
            {
                seen.edits.extend(change.edits.iter().cloned());
                continue;
            }

            documents.push(PlannedDocument {
                open: is_open(&path),
                path,
                version: change.version,
                edits: change.edits.clone(),
                contents: None,
            });
        }
        Ok(Self { documents })
    }

    /// A plan whose documents were worked out by the caller.
    ///
    /// Used for an edit that did not come from a language server, such as a
    /// replace across the workspace, so there are no URIs to resolve and no
    /// versions to check. [`crate::Session::apply_workspace_edit`] still makes
    /// its checks on every plan: nothing is written until all of it can be.
    pub(crate) fn from_documents(documents: Vec<PlannedDocument>) -> Self {
        Self { documents }
    }

    /// The files this edit needs that no tab holds, in the server's order.
    ///
    /// The frontend reads these before calling [`Plan::with_contents`].
    pub fn missing(&self) -> impl Iterator<Item = &Path> {
        self.documents
            .iter()
            .filter(|document| !document.open)
            .map(|document| document.path.as_path())
    }

    /// Every file the edit touches, open or not.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.documents
            .iter()
            .map(|document| document.path.as_path())
    }

    /// How many documents take part.
    pub fn documents(&self) -> usize {
        self.documents.len()
    }

    /// How many replacements there are in total.
    pub fn edits(&self) -> usize {
        self.documents.iter().map(|d| d.edits.len()).sum()
    }

    /// Supplies the text of the files [`Plan::missing`] named.
    ///
    /// `read` returns the text for a path, or the reason it could not be read.
    /// A file known to the server may have been deleted since. If any file
    /// cannot be read, the whole edit is rejected.
    pub fn with_contents(
        mut self,
        mut read: impl FnMut(&Path) -> Result<String, String>,
    ) -> Result<Self, WorkspaceError> {
        for document in &mut self.documents {
            if document.open {
                continue;
            }
            match read(&document.path) {
                Ok(text) => document.contents = Some(text),
                Err(reason) => {
                    return Err(WorkspaceError::Unreadable {
                        path: document.path.clone(),
                        reason,
                    })
                }
            }
        }
        Ok(self)
    }

    /// The planned documents, for the session that applies them.
    pub(crate) fn documents_mut(&mut self) -> &mut Vec<PlannedDocument> {
        &mut self.documents
    }
}

/// What applying a workspace edit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// How many documents were changed.
    pub documents: usize,
    /// How many replacements were made.
    pub edits: usize,
    /// How many of those documents the edit had to open.
    pub opened: usize,
}

impl Applied {
    /// A sentence for the status bar.
    ///
    /// Reports how many files were opened as well as how many were changed.
    /// The opened tabs contain unsaved changes the user did not open
    /// explicitly, so the user must be told to save them.
    pub fn summary(&self, what: &str) -> String {
        let edits = plural(self.edits, "change", "changes");
        let documents = plural(self.documents, "file", "files");
        if self.opened == 0 {
            return format!("{what}: {edits} in {documents}");
        }
        format!(
            "{what}: {edits} in {documents} ({} opened, unsaved)",
            self.opened
        )
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_core::Range;
    use deco_lsp::requests::DocumentEdits;

    fn edit(new_text: &str) -> TextEdit {
        TextEdit {
            range: Range::new(
                deco_core::Position::new(0, 0),
                deco_core::Position::new(0, 3),
            ),
            new_text: new_text.to_owned(),
        }
    }

    fn workspace(documents: &[(&str, Option<i64>)]) -> WorkspaceEdit {
        WorkspaceEdit {
            changes: documents
                .iter()
                .map(|(uri, version)| DocumentEdits {
                    uri: Uri::from_string(*uri),
                    version: *version,
                    edits: vec![edit("new")],
                })
                .collect(),
        }
    }

    #[test]
    fn a_uri_that_is_not_a_file_refuses_the_whole_edit() {
        let error = Plan::build(
            &workspace(&[("file:///w/a.rs", None), ("untitled:Untitled-1", None)]),
            |uri: &Uri| uri.to_path(deco_lsp::uri::PathStyle::Unix).ok(),
            |_| true,
            |_| None,
        )
        .expect_err("an untitled document has no path to edit");
        assert!(matches!(error, WorkspaceError::NotAFile(uri) if uri == "untitled:Untitled-1"));
    }

    #[test]
    fn a_stale_version_refuses_the_whole_edit() {
        let error = Plan::build(
            &workspace(&[("file:///w/a.rs", Some(3))]),
            |uri: &Uri| uri.to_path(deco_lsp::uri::PathStyle::Unix).ok(),
            |_| true,
            |_| Some(5),
        )
        .expect_err("the document moved on");
        assert_eq!(
            error,
            WorkspaceError::Stale {
                path: PathBuf::from("/w/a.rs"),
                expected: 3,
                actual: 5,
            }
        );
        assert!(
            error.to_string().contains("nothing was renamed"),
            "the message says the edit did not half-happen: {error}"
        );
    }

    #[test]
    fn a_server_that_states_no_version_is_taken_on_trust() {
        // The `changes` form carries no version. It is applied without a version
        // check rather than rejected. This test is separate from the passing
        // check because the two cases differ.
        let plan = Plan::build(
            &workspace(&[("file:///w/a.rs", None)]),
            |uri: &Uri| uri.to_path(deco_lsp::uri::PathStyle::Unix).ok(),
            |_| true,
            |_| Some(5),
        )
        .expect("nothing to compare against");
        assert_eq!(plan.documents(), 1);
    }

    #[test]
    fn the_plan_names_the_files_it_needs_read() {
        let plan = Plan::build(
            &workspace(&[("file:///w/open.rs", None), ("file:///w/closed.rs", None)]),
            |uri: &Uri| uri.to_path(deco_lsp::uri::PathStyle::Unix).ok(),
            |path| path.ends_with("open.rs"),
            |_| None,
        )
        .expect("both are files");

        let missing: Vec<&Path> = plan.missing().collect();
        assert_eq!(missing, [Path::new("/w/closed.rs")]);
        assert_eq!(plan.paths().count(), 2);
        assert_eq!(plan.edits(), 2);
    }

    #[test]
    fn an_open_file_is_never_read_from_disk() {
        // Its buffer may hold unsaved changes. Starting from the file on disk
        // would discard them and still report success.
        let plan = Plan::build(
            &workspace(&[("file:///w/open.rs", None)]),
            |uri: &Uri| uri.to_path(deco_lsp::uri::PathStyle::Unix).ok(),
            |_| true,
            |_| None,
        )
        .expect("a file")
        .with_contents(|path| panic!("read {} when a tab holds it", path.display()))
        .expect("nothing to read");

        assert!(plan.documents[0].contents.is_none());
    }

    #[test]
    fn a_file_that_cannot_be_read_refuses_the_whole_edit() {
        let error = Plan::build(
            &workspace(&[("file:///w/gone.rs", None)]),
            |uri: &Uri| uri.to_path(deco_lsp::uri::PathStyle::Unix).ok(),
            |_| false,
            |_| None,
        )
        .expect("a file")
        .with_contents(|_| Err("no such file".to_owned()))
        .expect_err("a file the edit needs is not there");

        assert_eq!(
            error,
            WorkspaceError::Unreadable {
                path: PathBuf::from("/w/gone.rs"),
                reason: "no such file".to_owned(),
            }
        );
    }

    #[test]
    fn the_summary_counts_what_a_user_has_to_act_on() {
        assert_eq!(
            Applied {
                documents: 1,
                edits: 1,
                opened: 0
            }
            .summary("Renamed"),
            "Renamed: 1 change in 1 file"
        );
        assert_eq!(
            Applied {
                documents: 3,
                edits: 7,
                opened: 2
            }
            .summary("Renamed"),
            "Renamed: 7 changes in 3 files (2 opened, unsaved)"
        );
    }
}
