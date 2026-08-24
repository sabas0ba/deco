//! One edit across several documents, applied all at once or not at all.
//!
//! A language server answering `textDocument/rename` does not send a list of
//! replacements for the file on screen — it sends replacements for every file
//! that mentions the symbol, most of which are not open. The same shape carries
//! a code action, a replace-across-files, and an agent's turn. What they share
//! is the property that makes them different from the edits deco already
//! applied: *partly* done is worse than not done at all. Half a rename leaves a
//! project that does not build, and if the halves landed as separate undo steps
//! there is no single keystroke that puts it back.
//!
//! So this module answers two questions in this order, and never the second
//! before the first:
//!
//! 1. **Can all of it be applied?** Every document is resolved, every version
//!    the server stated is checked, and every transaction is *built* — which is
//!    where overlapping edits are caught — before a single buffer is touched.
//!    A failure at any point leaves the session exactly as it was.
//! 2. **Apply all of it**, recording one shared [`deco_core::Group`] across
//!    every document that took part, so that `ctrl+z` from any of them takes
//!    the whole thing back.
//!
//! # Files that are not open
//!
//! Most of a rename lands in files nobody has opened. VS Code writes those to
//! disk directly. deco opens them instead, as background tabs holding unsaved
//! changes, for two reasons. The core performs no I/O at all — the reason the
//! whole editable surface is testable headlessly — so a write here would have
//! to be a request to the frontend, and the undo of that write another one.
//! And an editor that rewrites files you have never looked at, without being
//! asked to save, is doing the one thing the rest of deco is careful not to:
//! acting on your disk on somebody else's say-so. Opened rather than written,
//! the change is visible, `ctrl+z` reaches it, and `ctrl+k s` is what makes it
//! permanent.
//!
//! The frontend supplies the text, because reading the file is I/O: [`Plan`]
//! names the paths it needs and [`Plan::with_contents`] takes them back.

use std::path::{Path, PathBuf};

use deco_lsp::requests::{TextEdit, WorkspaceEdit};
use deco_lsp::uri::Uri;

/// Why a workspace edit was refused.
///
/// Every variant means nothing was changed. There is deliberately no error that
/// leaves a session half-edited: that is the entire point of the type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// A URI that does not name a file on this machine.
    ///
    /// `untitled:` documents and a server's own synthetic schemes end up here.
    /// Refused rather than skipped, because the edits deco *can* place are the
    /// other half of a change that only makes sense whole.
    #[error("the server sent an edit for `{0}`, which is not a file deco can open")]
    NotAFile(String),
    /// The document changed after the server computed the edit.
    ///
    /// Positions are only meaningful against the text they were computed for.
    /// A rename that arrives after a keystroke has moved everything down a line
    /// would replace whatever now sits at those coordinates.
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
    /// already open, whose buffer is the only correct starting point — a
    /// document with unsaved changes edited from its text on disk would silently
    /// discard them.
    pub contents: Option<String>,
}

/// A workspace edit, checked as far as it can be without touching a buffer.
///
/// Holding one is not permission to apply it: [`crate::Session::apply_workspace_edit`]
/// re-checks everything that could have changed in between, which is why the
/// plan can be handed to a frontend to fill in and handed back.
#[derive(Debug, Clone)]
pub struct Plan {
    documents: Vec<PlannedDocument>,
}

impl Plan {
    /// Resolves `edit` against the paths and versions of what is open.
    ///
    /// `version_of` answers with the version last sent to the language server
    /// for a path, or `None` for a document it is not tracking. It is a callback
    /// rather than a lookup because those versions belong to the LSP client,
    /// which lives in a frontend — but the *rule* about what a mismatch means
    /// belongs here, where every caller gets the same one.
    ///
    /// A server that stated no version gets no check. That is not the same as a
    /// check that passed, and it is worth knowing which servers do it: the
    /// `changes` spelling of a workspace edit carries no versions at all, so an
    /// edit that arrives that way is applied on trust.
    pub(crate) fn build(
        edit: &WorkspaceEdit,
        resolve: impl Fn(&Uri) -> Option<PathBuf>,
        is_open: impl Fn(&Path) -> bool,
        version_of: impl Fn(&Path) -> Option<i64>,
    ) -> Result<Self, WorkspaceError> {
        let mut documents = Vec::with_capacity(edit.changes.len());
        for change in &edit.changes {
            // Through the caller rather than through `Uri::to_path`, because a
            // server running on the far end of a remote session names files by
            // *its* paths: the mapping back is the session's, and a plan built
            // against the wrong end of it would edit files on the wrong machine.
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

            // Two URIs can spell one file — percent-encoding is not unique — and
            // the two halves then belong to one document and one transaction.
            // Opened twice they would be two buffers over one path, which is the
            // divergent-copies problem tabs already refuse to create.
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
    /// For an edit that did not come from a language server and so has no URIs
    /// to resolve and no versions to check — a replace across the workspace,
    /// where the editor itself decided what to change. The checks that remain
    /// are the ones [`crate::Session::apply_workspace_edit`] makes on every
    /// plan, which are the ones that matter: nothing is written until all of it
    /// can be.
    pub(crate) fn from_documents(documents: Vec<PlannedDocument>) -> Self {
        Self { documents }
    }

    /// The files this edit needs that no tab holds, in the server's order.
    ///
    /// What the frontend has to read before [`Plan::with_contents`].
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
    /// `read` is given a path and answers with its text, or with why it could
    /// not be read — a file the server knows about may have been deleted since,
    /// and a rename that cannot see one of the files it is changing must not
    /// proceed with the rest.
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
    /// Says how many files were opened as well as how many were changed,
    /// because those tabs are unsaved work that appeared without being asked
    /// for, and a user who is not told about them will not save them.
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
        // The `changes` spelling carries none. Applying it is the only thing that
        // can be done with it, so it is not an error — but it is not a check that
        // passed either, which is why the two cases are written down separately.
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
        // Its buffer may hold unsaved changes, and starting from the file on disk
        // would discard them while reporting success.
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
