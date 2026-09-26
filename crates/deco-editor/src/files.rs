//! Operations on files rather than on their contents.
//!
//! Creating, renaming and deleting are not text edits, so they do not go through
//! [`crate::workspace::Plan`], which resolves edits *within* files. They follow
//! the same division of work: the core decides what should happen and rejects
//! invalid requests, and the frontend performs the filesystem operation. The
//! request is an [`Operation`], passed to a frontend as
//! [`Outcome::FileOperation`](crate::Outcome::FileOperation).
//!
//! # What undoes
//!
//! Creating and renaming can be reversed by another operation (delete what was
//! created, rename back), so they go on the explorer's undo stack. Deleting
//! cannot be reversed because deco does not keep the deleted bytes. deco has no
//! trash either, so `files.enableTrash` is one of the settings deco does not
//! honour. Deletion requires confirmation with a warning that the operation
//! cannot be undone.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Why a file operation was refused before anything touched the disk.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FileError {
    /// The name has a path separator, or is `.` or `..`.
    ///
    /// Rejecting these prevents a rename such as `../../etc/passwd` from moving
    /// a file out of the workspace or to a location the tree cannot display.
    #[error("`{0}` is a name with a path in it — type just the name")]
    NotAName(String),
    /// The name was empty or only spaces.
    #[error("a name is needed")]
    Empty,
    /// Something is already called that.
    #[error("`{0}` already exists")]
    Exists(String),
    /// No row was selected.
    #[error("nothing is selected in the tree")]
    NoSelection,
    /// The path is not inside the workspace.
    ///
    /// Every path here is built from the workspace root and a checked name, so
    /// this should be unreachable. It is checked anyway because a mistake could
    /// delete something outside the folder the user opened.
    #[error("`{}` is outside the workspace", .0.display())]
    Outside(PathBuf),
}

/// The size and modification time of a file at one moment, as reported by `std`.
///
/// These are the portable fields. A real identity is the inode on Unix and a
/// file index on Windows, and the standard library exposes the latter only
/// behind an unstable feature. A stamp is therefore evidence, not proof, that
/// the file at a path is unchanged. A later replacement has a later
/// modification time and is detected; a replacement crafted to match is not.
///
/// An undo is rejected on a mismatch so that it does not move a different file
/// that now occupies the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// How big it was.
    pub len: u64,
    /// When it was last written, if the platform said.
    pub modified: Option<std::time::SystemTime>,
}

/// A change to the files themselves, for a frontend to carry out.
///
/// Every variant holds absolute paths. The session resolves a typed name
/// against the selected row before it creates the operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Create an empty file, and open it.
    ///
    /// The file is opened immediately, as in VS Code, so the user does not have
    /// to find it in the tree.
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
        /// Set only on the *inverse* of a completed rename, by
        /// [`Session::stamp_last_undo`](crate::Session::stamp_last_undo).
        /// Without it, undoing a rename identifies the file only by path. If
        /// another program removed the renamed file and put a different one at
        /// the same path, `ctrl+z` would move that file back and point the
        /// buffer at it, and the next save would overwrite its contents.
        ///
        /// This is evidence, not identity. See [`Stamp`].
        expect: Option<Stamp>,
        /// Whether the tree was showing a directory when this was decided.
        ///
        /// Carried for the same reason as in [`Operation::Delete`]. Without it,
        /// a directory replaced by a file since the tree read it would be moved
        /// as a directory, and
        /// [`Session::file_operation_done`](crate::Session::file_operation_done)
        /// would retarget every tab *below* the old path onto a regular file.
        directory: bool,
    },
    /// Remove it. A directory goes with everything in it.
    Delete {
        /// What to remove.
        path: PathBuf,
        /// Whether the tree was showing a directory when this was confirmed.
        ///
        /// The frontend acts on what the user confirmed rather than on what is
        /// on disk now. The tree has no watcher, so its state can be stale. If a
        /// file has since been replaced by a directory, a frontend that checked
        /// the disk would delete the directory recursively after the user
        /// confirmed deleting a file.
        directory: bool,
    },
    /// Remove it, but only if it is still empty.
    ///
    /// This is the inverse of a create. A plain [`Operation::Delete`] would be
    /// wrong: after creating a file, typing in it and saving, `ctrl+z` in the
    /// tree would delete the file *and its contents* without the confirmation a
    /// delete requires and without a way to restore it. For a folder the delete
    /// would be recursive.
    ///
    /// The inverse of a create therefore removes only an empty file or an empty
    /// directory. Anything else is rejected.
    DeleteIfEmpty {
        /// What to remove.
        path: PathBuf,
        /// Whether the create being undone made a directory. As with
        /// [`Operation::Delete`], undoing "new file" must not remove a directory
        /// that has since taken its name.
        directory: bool,
        /// What the created entry looked like, once it existed.
        ///
        /// Emptiness alone does not identify it. Another program can remove the
        /// new entry and leave a *different* empty file or directory at the same
        /// path, and the undo would delete that instead. This is the same check
        /// a rename's undo uses. See [`Stamp`].
        expect: Option<Stamp>,
    },
}

impl Operation {
    /// The directory whose listing this invalidates.
    ///
    /// A rename could affect two directories if names could contain a directory.
    /// They cannot, so both sides of a rename share one parent.
    pub fn parent(&self) -> Option<&Path> {
        match self {
            Self::CreateFile(path) | Self::CreateFolder(path) => path.parent(),
            Self::Delete { path, .. } | Self::DeleteIfEmpty { path, .. } => path.parent(),
            Self::Rename { to, .. } => to.parent(),
        }
    }

    /// The path this operation puts something *at*, if it puts anything.
    ///
    /// Any cached workspace state for this path must be discarded, whether the
    /// operation succeeded or not:
    ///
    /// - **Success.** The path now holds what this operation created, and any
    ///   listing or expansion cached under that name describes an entry that
    ///   no longer exists.
    /// - **Failure.** The path was checked as free before the attempt, because
    ///   a name the tree shows as taken is rejected before reaching the disk.
    ///   A failure therefore means the tree's state for that path was wrong:
    ///   another program put something there, and the cached state does not
    ///   describe it.
    ///
    /// The failure case is easy to miss. A directory removed outside deco
    /// keeps its cached rows. A create with that name fails because another
    /// program recreated it. The refresh of the parent would then render the
    /// new directory with the removed directory's children, without requesting
    /// a listing, because it has a cached one.
    ///
    /// Returns `None` for a delete, which puts nothing at its path.
    pub fn arriving(&self) -> Option<&Path> {
        match self {
            Self::CreateFile(path) | Self::CreateFolder(path) => Some(path),
            Self::Rename { to, .. } => Some(to),
            Self::Delete { .. } | Self::DeleteIfEmpty { .. } => None,
        }
    }

    /// The operation that puts things back, if there is one.
    ///
    /// Returns `None` for a delete (see the module docs). The caller can then
    /// report that the delete cannot be undone. If the undo stack dropped the
    /// entry silently, `ctrl+z` would undo the operation *before* the delete
    /// instead.
    pub fn inverse(&self) -> Option<Operation> {
        match self {
            Self::CreateFile(path) => Some(Self::DeleteIfEmpty {
                path: path.clone(),
                directory: false,
                // Filled in once the file exists.
                expect: None,
            }),
            Self::CreateFolder(path) => Some(Self::DeleteIfEmpty {
                path: path.clone(),
                directory: true,
                expect: None,
            }),
            Self::Rename {
                from,
                to,
                directory,
                ..
            } => Some(Self::Rename {
                from: to.clone(),
                to: from.clone(),
                // Filled in once the rename has happened.
                expect: None,
                // The entry moved back is the same kind as the one moved.
                directory: *directory,
            }),
            Self::Delete { .. } => None,
            // Redoing a create would recreate an empty entry, but the original
            // file may have had content. The stack supports only one step.
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

    /// What to say when it could not be done, after "could not".
    pub fn attempted(&self) -> String {
        match self {
            Self::CreateFile(path) => format!("create {}", name_of(path)),
            Self::CreateFolder(path) => format!("create {}/", name_of(path)),
            Self::Rename { from, to, .. } => {
                format!("rename {} to {}", name_of(from), name_of(to))
            }
            Self::Delete { path, .. } | Self::DeleteIfEmpty { path, .. } => {
                format!("delete {}", name_of(path))
            }
        }
    }
}

/// Checks that `name` is a name and not a path.
///
/// Both separators are rejected on every platform. A workspace is often shared
/// across platforms, and a file with a backslash in its name causes problems on
/// Windows even where the name is legal.
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
                expect: None,
            }),
            "undoing a create must not take content that was added since"
        );

        let rename = Operation::Rename {
            from: PathBuf::from("/w/a.rs"),
            to: PathBuf::from("/w/b.rs"),
            expect: None,
            directory: false,
        };
        assert_eq!(
            rename.inverse(),
            Some(Operation::Rename {
                from: PathBuf::from("/w/b.rs"),
                to: PathBuf::from("/w/a.rs"),
                expect: None,
                directory: false,
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
                directory: false,
            }
            .parent(),
            Some(Path::new("/w/src"))
        );
    }
}
