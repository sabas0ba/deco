//! Changing files rather than their contents.
//!
//! Creating, renaming and deleting are not text edits, so they do not go through
//! [`crate::workspace::Plan`] — that resolves *edits within* files, and a file
//! that does not exist yet has no text to edit. What they share with it is the
//! division of labour: the core decides what should happen and refuses what
//! should not, and whoever has a filesystem carries it out. Here that is
//! [`Operation`], handed to a frontend as
//! [`Outcome::FileOperation`](crate::Outcome::FileOperation).
//!
//! # What undoes
//!
//! Creating and renaming are reversible with another operation of the same kind
//! — delete what was made, rename back — so they go on the explorer's undo
//! stack. Deleting is not: undoing it needs the bytes, and deco has nowhere to
//! keep them. There is no trash to move a file to either, so `files.enableTrash`
//! is one of the settings deco does not honour. A delete therefore asks first
//! and says plainly that it cannot be taken back, which is the honest version of
//! a feature that would otherwise quietly lose someone's work.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Why a file operation was refused before anything touched the disk.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FileError {
    /// The name has a path separator, or is `.` or `..`.
    ///
    /// A name is a name. Typing `../../etc/passwd` into a rename box should not
    /// be a way to move a file out of the workspace, and a tree that put it
    /// somewhere it cannot display is broken whatever else is true.
    #[error("`{0}` is a name with a path in it — type just the name")]
    NotAName(String),
    /// The name was empty or only spaces.
    #[error("a name is needed")]
    Empty,
    /// Something is already called that.
    #[error("`{0}` already exists")]
    Exists(String),
    /// The operation was on nothing — no row was selected.
    #[error("nothing is selected in the tree")]
    NoSelection,
    /// The path is not inside the workspace.
    ///
    /// Belt as well as braces: every path here is built from the workspace root
    /// and a checked name, so this should be unreachable. It is checked anyway
    /// because the cost of being wrong is deleting something outside the folder
    /// the user opened.
    #[error("`{}` is outside the workspace", .0.display())]
    Outside(PathBuf),
}

/// What a file looked like at a moment, as far as `std` can say.
///
/// Size and modification time, which is what is portable: a real identity is the
/// inode on Unix and a file index on Windows, and the standard library exposes
/// the second only behind an unstable feature. So this is *evidence* that the
/// file at a path is still the one that was there, not proof — a replacement
/// made after the fact has a later modification time, which is the case worth
/// catching, and one contrived to match would get through.
///
/// Refusing on a mismatch is the point: the alternative is moving somebody
/// else's file on a keystroke meant to undo your own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// How big it was.
    pub len: u64,
    /// When it was last written, if the platform said.
    pub modified: Option<std::time::SystemTime>,
}

/// A change to the files themselves, for a frontend to carry out.
///
/// Every variant names absolute paths. Resolving a typed name against the
/// selected row happens in the session, which knows what is selected; by the
/// time an operation exists the question of *where* is already settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Create an empty file, and open it.
    ///
    /// Opening it is the point: a new file you then have to find in the tree is
    /// two steps where VS Code has one.
    CreateFile(PathBuf),
    /// Create an empty directory.
    CreateFolder(PathBuf),
    /// Move `from` to `to`, retargeting any tab that holds it.
    Rename {
        /// What it is called now.
        from: PathBuf,
        /// What it should be called.
        to: PathBuf,
        /// What the file at `from` looked like when this was decided.
        ///
        /// Only ever set on the *inverse* of a rename that has happened, by
        /// [`Session::stamp_last_undo`](crate::Session::stamp_last_undo). Undoing
        /// a rename otherwise names a path and nothing else: if another program
        /// removed the renamed file and put a different one at the same path,
        /// `ctrl+z` would move *that* file back and point the buffer at it, and
        /// the next save would write over its contents.
        ///
        /// Evidence rather than identity — see [`Stamp`].
        expect: Option<Stamp>,
    },
    /// Remove it. A directory goes with everything in it.
    Delete {
        /// What to remove.
        path: PathBuf,
        /// Whether the tree was showing a directory when this was confirmed.
        ///
        /// Carried so the frontend can act on what the *user agreed to* rather
        /// than on what is there now. The tree has no watcher, so its picture
        /// can be stale: if a file has been replaced by a directory since, a
        /// frontend that looked at the disk would find a directory and delete it
        /// recursively — having asked about a file. The confirmation named one
        /// thing; this is how that thing stays named.
        directory: bool,
    },
    /// Remove it, but only if it is still empty.
    ///
    /// What undoing a create becomes. A plain [`Operation::Delete`] would be
    /// wrong: create a file, type in it, save, then press `ctrl+z` in the tree,
    /// and the undo would take the file *and its contents* — without the
    /// confirmation a delete asks for, and with no way back. Worse for a folder,
    /// where the delete is recursive.
    ///
    /// So the inverse of a create only removes what a create made: an empty
    /// file, an empty directory. Anything else and it refuses, because at that
    /// point undoing the create is not undoing anything — it is deleting work.
    DeleteIfEmpty {
        /// What to remove.
        path: PathBuf,
        /// Whether the create being undone made a directory. Same reason as
        /// [`Operation::Delete`]'s: undoing "new file" must not remove someone
        /// else's directory that has taken its name.
        directory: bool,
    },
}

impl Operation {
    /// The directory whose listing this invalidates.
    ///
    /// A rename can touch two, when a name with a directory in it is allowed —
    /// which it is not, so the parent is the same on both sides and one answer
    /// is enough.
    pub fn parent(&self) -> Option<&Path> {
        match self {
            Self::CreateFile(path) | Self::CreateFolder(path) => path.parent(),
            Self::Delete { path, .. } | Self::DeleteIfEmpty { path, .. } => path.parent(),
            Self::Rename { to, .. } => to.parent(),
        }
    }

    /// The operation that puts things back, if there is one.
    ///
    /// `None` for a delete: see the module docs. Returning `None` rather than a
    /// best-effort restore is the point — a stack that silently dropped the
    /// entry would make `ctrl+z` undo the operation *before* the delete, which
    /// is worse than one that says it cannot.
    pub fn inverse(&self) -> Option<Operation> {
        match self {
            Self::CreateFile(path) => Some(Self::DeleteIfEmpty {
                path: path.clone(),
                directory: false,
            }),
            Self::CreateFolder(path) => Some(Self::DeleteIfEmpty {
                path: path.clone(),
                directory: true,
            }),
            Self::Rename { from, to, .. } => Some(Self::Rename {
                from: to.clone(),
                to: from.clone(),
                // Filled in once the rename has actually happened; there is
                // nothing to stamp until then.
                expect: None,
            }),
            Self::Delete { .. } => None,
            // Undoing the undo of a create would be creating it again, which is
            // not what was there before: the file may have had content. One step
            // is what this stack promises.
            Self::DeleteIfEmpty { .. } => None,
        }
    }

    /// What to say once it has been done.
    pub fn describe(&self) -> String {
        match self {
            Self::CreateFile(path) => format!("created {}", name_of(path)),
            Self::CreateFolder(path) => format!("created {}/", name_of(path)),
            Self::Rename { from, to, .. } => {
                format!("renamed {} to {}", name_of(from), name_of(to))
            }
            Self::Delete { path, .. } | Self::DeleteIfEmpty { path, .. } => {
                format!("deleted {}", name_of(path))
            }
        }
    }
}

/// Checks that `name` is a name and not a path.
///
/// Both separators are refused on every platform, not just the one this is
/// running on: a workspace is often shared with people on the other kind, and a
/// file with a backslash in its name is a problem for them even where it is
/// technically legal here.
pub fn check_name(name: &str) -> Result<&str, FileError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(FileError::Empty);
    }
    if trimmed == "." || trimmed == ".." || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(FileError::NotAName(trimmed.to_owned()));
    }
    Ok(trimmed)
}

/// Checks that `path` is inside `root`.
pub fn check_inside(root: &Path, path: &Path) -> Result<(), FileError> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(FileError::Outside(path.to_path_buf()))
    }
}

/// A path's own name, for a message.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_with_a_path_in_it_is_refused_on_every_platform() {
        for bad in ["../escape", "a/b", "a\\b", ".", "..", "  "] {
            assert!(
                check_name(bad).is_err(),
                "{bad:?} was accepted as a file name"
            );
        }
        assert_eq!(check_name("  main.rs  ").unwrap(), "main.rs");
    }

    #[test]
    fn creating_and_renaming_can_be_undone_and_deleting_cannot() {
        let create = Operation::CreateFile(PathBuf::from("/w/new.rs"));
        assert_eq!(
            create.inverse(),
            Some(Operation::DeleteIfEmpty {
                path: PathBuf::from("/w/new.rs"),
                directory: false,
            }),
            "undoing a create must not take content that was added since"
        );

        let rename = Operation::Rename {
            from: PathBuf::from("/w/a.rs"),
            to: PathBuf::from("/w/b.rs"),
            expect: None,
        };
        assert_eq!(
            rename.inverse(),
            Some(Operation::Rename {
                from: PathBuf::from("/w/b.rs"),
                to: PathBuf::from("/w/a.rs"),
                expect: None,
            })
        );

        assert_eq!(
            Operation::Delete {
                path: PathBuf::from("/w/gone.rs"),
                directory: false,
            }
            .inverse(),
            None,
            "there is nowhere to keep the bytes"
        );
    }

    #[test]
    fn a_path_outside_the_workspace_is_refused() {
        let root = Path::new("/w");
        assert!(check_inside(root, Path::new("/w/src/main.rs")).is_ok());
        assert!(check_inside(root, Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn every_operation_names_the_directory_to_re_read() {
        assert_eq!(
            Operation::CreateFile(PathBuf::from("/w/src/new.rs")).parent(),
            Some(Path::new("/w/src"))
        );
        assert_eq!(
            Operation::Rename {
                from: PathBuf::from("/w/src/a.rs"),
                to: PathBuf::from("/w/src/b.rs"),
                expect: None,
            }
            .parent(),
            Some(Path::new("/w/src"))
        );
    }
}
