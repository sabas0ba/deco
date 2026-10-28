//! The file tree's changes to files, as a filesystem carries them out.
//!
//! Used on both sides of a session: the editor performs a [`Change`] on its
//! own disk, and in a remote session the server performs the same [`Change`]
//! on the remote's disk through `fs.change`. One implementation therefore
//! decides what a create, rename or delete may overwrite, wherever the files
//! are.
//!
//! Every function here acts on the paths it is given. Confinement to the
//! workspace is the caller's: the editor checks the paths against the tree's
//! root, and the server resolves them inside the directory it serves.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The size and modification time of an entry at one moment.
///
/// Evidence, not identity, that the entry at a path is the one seen before:
/// a later replacement has a later modification time. An undo is refused on
/// a mismatch, so it does not act on a different file that took the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// How big it was.
    pub len: u64,
    /// When it was last written, if the platform said.
    pub modified: Option<SystemTime>,
}

impl Stamp {
    /// The stamp of the entry at `path`, not following a final link, or
    /// `None` if it cannot be read.
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    /// The form sent over a connection. A modification time before 1970,
    /// which JSON numbers here cannot carry, is sent as unknown; the
    /// receiving side compares in this form, so such an entry never matches
    /// and its undo is refused rather than guessed.
    fn to_wire(&self) -> WireStamp {
        WireStamp {
            len: self.len,
            modified: self
                .modified
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|since| (since.as_secs(), since.subsec_nanos())),
        }
    }
}

/// A [`Stamp`] as JSON carries it: seconds and nanoseconds since 1970.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct WireStamp {
    len: u64,
    modified: Option<(u64, u32)>,
}

impl From<WireStamp> for Stamp {
    fn from(wire: WireStamp) -> Self {
        Self {
            len: wire.len,
            modified: wire
                .modified
                .map(|(secs, nanos)| UNIX_EPOCH + Duration::new(secs, nanos)),
        }
    }
}

impl Serialize for Stamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_wire().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Stamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        WireStamp::deserialize(deserializer).map(Self::from)
    }
}

/// Whether the entry at `path` matches `expected`, compared in the form a
/// connection carries, so a stamp that crossed one compares the same way as
/// one that did not.
fn matches(path: &Path, expected: &Stamp) -> bool {
    Stamp::of(path).is_some_and(|now| now.to_wire() == expected.to_wire())
}

/// One change the file tree makes. The meaning of each variant and field is
/// that of `deco_editor::FileOperation`, which the editor converts from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Change {
    /// Create an empty file; fails if anything is there.
    CreateFile {
        /// Where.
        path: PathBuf,
    },
    /// Create an empty directory; fails if anything is there.
    CreateFolder {
        /// Where.
        path: PathBuf,
    },
    /// Move `from` to `to`, never over something already at `to`.
    Rename {
        /// What it is called now.
        from: PathBuf,
        /// What it should be called.
        to: PathBuf,
        /// What `from` must still look like, for the undo of a rename.
        expect: Option<Stamp>,
        /// Whether the tree showed a directory.
        directory: bool,
    },
    /// Remove it; a directory with everything in it.
    Delete {
        /// What.
        path: PathBuf,
        /// Whether the tree showed a directory.
        directory: bool,
    },
    /// Remove it only while it is empty: the undo of a create.
    DeleteIfEmpty {
        /// What.
        path: PathBuf,
        /// Whether the create made a directory.
        directory: bool,
        /// What the created entry must still look like.
        expect: Option<Stamp>,
    },
}

impl Change {
    /// Every path the change acts on, for a caller to confine.
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            Self::CreateFile { path }
            | Self::CreateFolder { path }
            | Self::Delete { path, .. }
            | Self::DeleteIfEmpty { path, .. } => vec![path],
            Self::Rename { from, to, .. } => vec![from, to],
        }
    }

    /// The same change on other paths: `map` is given each path in turn.
    pub fn map_paths(&self, mut map: impl FnMut(&Path) -> PathBuf) -> Self {
        match self {
            Self::CreateFile { path } => Self::CreateFile { path: map(path) },
            Self::CreateFolder { path } => Self::CreateFolder { path: map(path) },
            Self::Rename {
                from,
                to,
                expect,
                directory,
            } => Self::Rename {
                from: map(from),
                to: map(to),
                expect: expect.clone(),
                directory: *directory,
            },
            Self::Delete { path, directory } => Self::Delete {
                path: map(path),
                directory: *directory,
            },
            Self::DeleteIfEmpty {
                path,
                directory,
                expect,
            } => Self::DeleteIfEmpty {
                path: map(path),
                directory: *directory,
                expect: expect.clone(),
            },
        }
    }

    /// The path a create or a rename puts something at.
    fn made(&self) -> Option<&Path> {
        match self {
            Self::CreateFile { path } | Self::CreateFolder { path } => Some(path),
            Self::Rename { to, .. } => Some(to),
            Self::Delete { .. } | Self::DeleteIfEmpty { .. } => None,
        }
    }
}

/// Carries out `change` and returns the stamp of what it made, for the undo
/// to check: the new entry of a create, the destination of a rename.
///
/// Each check here is repeated at the moment of the change because the tree's
/// listing can be stale: another program can take a name or replace an entry
/// between the listing and the change.
pub fn apply(change: &Change) -> io::Result<Option<Stamp>> {
    perform(change)?;
    Ok(change.made().and_then(Stamp::of))
}

/// Whether two paths name the same file on disk.
///
/// Compares canonical paths rather than strings, so a case-only rename on a
/// case-insensitive filesystem is recognised as the same file. A path that
/// cannot be canonicalised is treated as a different file. The caller only asks
/// when `to` exists, so a failure here means something changed and rejecting
/// the rename is the safe result.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn perform(operation: &Change) -> io::Result<()> {
    match operation {
        Change::CreateFile { path } => std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map(drop),
        Change::CreateFolder { path } => std::fs::create_dir(path),
        Change::Rename {
            from,
            to,
            expect,
            directory,
        } => {
            // Check against the kind the tree was showing, not the kind on disk
            // now. Otherwise a directory replaced by a file since it was read
            // would be moved as a directory, and every tab below the old path
            // would be retargeted onto a regular file. Delete has the same
            // protection.
            match std::fs::symlink_metadata(from) {
                Ok(meta) if meta.is_dir() == *directory => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "{} is not the {} the tree was showing any more",
                            from.display(),
                            if *directory { "folder" } else { "file" }
                        ),
                    ))
                }
                Err(error) => return Err(error),
            }
            // An undo carries the file's stamp from when it was moved. If the
            // stamp no longer matches, something replaced the file. Moving it
            // back would move a different file and point the buffer at it, so
            // the next save would overwrite its contents.
            if expect
                .as_ref()
                .is_some_and(|expected| !matches(from, expected))
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} is not the file that was renamed any more",
                        from.display()
                    ),
                ));
            }
            // `rename` would overwrite an existing `to` on Unix. The tree already
            // rejected that, but the destination can appear between its listing
            // and this call, so check again to avoid silently replacing a file.
            //
            // A case-only rename is exempt. On the case-insensitive filesystems
            // that macOS and Windows usually use, `Foo.rs` and `foo.rs` are the
            // same file, so this check would find the source itself and reject
            // every change of capitalisation. Canonicalising both paths
            // distinguishes the same file from a different file in the way.
            //
            // `symlink_metadata` is used rather than `exists`, which follows
            // links. A dangling symlink at the destination returns `false` from
            // `exists` and would be silently replaced. The tree does not show
            // such links either, because `list_dir` reports only real files and
            // directories, so this case occurs even without a race.
            if std::fs::symlink_metadata(to).is_ok() && !same_file(from, to) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", to.display()),
                ));
            }
            std::fs::rename(from, to)
        }
        // Dispatched on what the tree was showing when the user confirmed, not
        // on what is on disk now. The tree has no watcher. If a file has since
        // been replaced by a directory, checking the disk would find a directory
        // and delete it recursively after the user confirmed deleting a file.
        // `remove_file` on a directory fails, which is the intended result.
        Change::Delete {
            path,
            directory: true,
        } => std::fs::remove_dir_all(path),
        Change::Delete {
            path,
            directory: false,
        } => std::fs::remove_file(path),
        // Undoing a create. `remove_dir` is used rather than `remove_dir_all`
        // because it fails on a non-empty directory. The operating system
        // enforces this, so there is no race with a separate check.
        Change::DeleteIfEmpty {
            path,
            directory,
            expect,
        } => {
            // Being empty does not identify the entry. Another program can
            // remove what was created and leave a different empty file or
            // directory in its place, and undoing the create would delete it.
            if expect
                .as_ref()
                .is_some_and(|expected| !matches(path, expected))
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is not what was created any more", path.display()),
                ));
            }
            if *directory {
                std::fs::remove_dir(path).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("{} is no longer empty", path.display()),
                    )
                })
            } else {
                // A file must be checked explicitly because there is no
                // "unlink if empty". Data written between this check and the
                // unlink is lost. This window is accepted, because refusing
                // instead would mean a create could never be undone on a
                // filesystem shared with other programs.
                match std::fs::metadata(path) {
                    Ok(meta) if meta.len() > 0 => Err(io::Error::other(format!(
                        "{} has been written to since it was created — delete it \
                         yourself if that is what you meant",
                        path.display()
                    ))),
                    Ok(_) => std::fs::remove_file(path),
                    Err(error) => Err(error),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deco-file-ops-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_create_never_replaces_what_is_there() {
        let dir = scratch("create");
        let path = dir.join("a.txt");
        let stamp = apply(&Change::CreateFile { path: path.clone() }).unwrap();
        assert_eq!(stamp.map(|stamp| stamp.len), Some(0));
        std::fs::write(&path, "kept").unwrap();
        assert!(apply(&Change::CreateFile { path: path.clone() }).is_err());
        assert!(apply(&Change::CreateFolder { path: path.clone() }).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
    }

    #[test]
    fn a_rename_never_lands_on_another_file() {
        let dir = scratch("rename");
        let (from, to) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&from, "a").unwrap();
        std::fs::write(&to, "b").unwrap();
        let rename = Change::Rename {
            from: from.clone(),
            to: to.clone(),
            expect: None,
            directory: false,
        };
        assert!(apply(&rename).is_err());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "b");
        std::fs::remove_file(&to).unwrap();
        assert!(apply(&rename).unwrap().is_some());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "a");
    }

    #[test]
    fn an_undo_refuses_an_entry_that_changed_since() {
        let dir = scratch("undo");
        let path = dir.join("new.txt");
        let stamp = apply(&Change::CreateFile { path: path.clone() }).unwrap();
        // Written to since: the undo of the create leaves it.
        std::fs::write(&path, "work").unwrap();
        let undo = Change::DeleteIfEmpty {
            path: path.clone(),
            directory: false,
            expect: stamp,
        };
        assert!(apply(&undo).is_err());
        assert!(path.exists());
    }

    #[test]
    fn a_stamp_reads_back_as_written() {
        let stamp = Stamp {
            len: 7,
            modified: Some(UNIX_EPOCH + Duration::new(1_700_000_000, 123)),
        };
        let json = serde_json::to_value(&stamp).unwrap();
        assert_eq!(serde_json::from_value::<Stamp>(json).unwrap(), stamp);
        let change = Change::DeleteIfEmpty {
            path: "a".into(),
            directory: true,
            expect: Some(stamp),
        };
        let json = serde_json::to_value(&change).unwrap();
        assert_eq!(json["op"], "deleteIfEmpty");
        assert_eq!(serde_json::from_value::<Change>(json).unwrap(), change);
    }
}
