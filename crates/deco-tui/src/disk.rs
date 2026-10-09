//! Noticing that an open file changed on disk.
//!
//! Each open file's modification time and size are remembered when deco last
//! read or wrote it. [`DiskWatch::check`] compares them with the file's
//! current state, once a second at most, by asking the filesystem rather than
//! by subscribing to change notifications, which would need a dependency per
//! platform. A file whose state changed is read and compared by content, so a
//! touch without a change, or a rewrite with the same text, is not reported.
//!
//! Only local files are watched. A remote session's files would need a round
//! trip per file per check.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use deco_editor::Session;

/// How often the open files are looked at, in milliseconds.
pub const CHECK_INTERVAL_MS: u64 = 1000;

/// What a file looked like on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    /// The file's current state, or `None` when it cannot be read.
    pub fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

/// What a check did, for the caller to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A clean document took the file's new text.
    Reloaded(PathBuf),
    /// The file changed while the document has unsaved changes; nothing was
    /// replaced.
    Conflict(PathBuf),
    /// The file can no longer be read.
    Gone(PathBuf),
}

/// The last known state of every open file.
#[derive(Debug, Default)]
pub struct DiskWatch {
    known: HashMap<PathBuf, Stamp>,
    /// The state a conflict or a disappearance was reported for, so it is
    /// reported once rather than every second.
    reported: HashMap<PathBuf, Option<Stamp>>,
    /// The file a save was refused for, and its state then, so that saving it
    /// again overwrites that version and no later one.
    armed: Option<(PathBuf, Option<Stamp>)>,
    last_check_ms: Option<u64>,
}

impl DiskWatch {
    /// Records `path` as deco has just read or written it.
    pub fn remember(&mut self, path: &Path) {
        self.remember_as(path, Stamp::of(path));
    }

    /// Records `path` with the state it had when deco read it.
    fn remember_as(&mut self, path: &Path, stamp: Option<Stamp>) {
        self.reported.remove(path);
        match stamp {
            Some(stamp) => {
                self.known.insert(path.to_path_buf(), stamp);
            }
            None => {
                self.known.remove(path);
            }
        }
    }

    /// Whether `path` changed on disk since deco last read or wrote it.
    ///
    /// A file never recorded is recorded now and is not a change: there is
    /// nothing to compare it with.
    pub fn changed_since_known(&mut self, path: &Path) -> bool {
        let current = Stamp::of(path);
        match (self.known.get(path), current) {
            (Some(known), current) => Some(*known) != current,
            (None, Some(stamp)) => {
                self.known.insert(path.to_path_buf(), stamp);
                false
            }
            (None, None) => false,
        }
    }

    /// Whether a save started by the user may write `path`.
    ///
    /// A file that changed on disk since deco last read or wrote it is not
    /// overwritten on the first request. A second request for the same file,
    /// with no other save in between and no further change to the file, is
    /// taken as the confirmation. A file changed again in between asks again,
    /// because the user has not seen that version.
    pub fn may_overwrite(&mut self, path: &Path) -> bool {
        let armed = self.armed.take();
        if !self.changed_since_known(path) {
            return true;
        }
        let current = Stamp::of(path);
        if armed.as_ref() == Some(&(path.to_path_buf(), current)) {
            return true;
        }
        self.armed = Some((path.to_path_buf(), current));
        false
    }

    /// Looks at every open file, at most once per [`CHECK_INTERVAL_MS`].
    ///
    /// A clean document whose file changed takes the new text as one undo
    /// step. A document with unsaved changes keeps them, and the change is
    /// returned once so the caller can say so; saving then asks before
    /// overwriting.
    pub fn check(&mut self, session: &mut Session, now_ms: u64) -> Vec<Change> {
        if self
            .last_check_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < CHECK_INTERVAL_MS)
        {
            return Vec::new();
        }
        self.last_check_ms = Some(now_ms);
        let mut changes = Vec::new();
        for path in session.open_paths() {
            if !self.changed_since_known(&path) {
                continue;
            }
            let current = Stamp::of(&path);
            if self.reported.get(&path) == Some(&current) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                self.reported.insert(path.clone(), current);
                changes.push(Change::Gone(path));
                continue;
            };
            // A writer still at work: what was read may be older than the
            // state now on disk, and recording that state would treat the
            // newer text as already shown. The next check reads it again.
            if Stamp::of(&path) != current {
                continue;
            }
            match session.is_dirty_at(&path) {
                Some(false) => {
                    if session.reload_changed(&path, &text) {
                        changes.push(Change::Reloaded(path.clone()));
                    }
                    // Reloaded, or the same text: either way the document
                    // now matches the file as it was read.
                    self.remember_as(&path, current);
                }
                Some(true) => {
                    self.reported.insert(path.clone(), current);
                    changes.push(Change::Conflict(path));
                }
                None => {}
            }
        }
        changes
    }

    /// Forgets that a conflict on `path` was reported, so the next change is
    /// reported again.
    pub fn forget_report(&mut self, path: &Path) {
        self.reported.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_config::Settings;
    use deco_keymap::binding::Platform;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deco-disk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory");
        dir
    }

    /// Writes `text`, making sure the stamp differs from the previous write
    /// even on a filesystem with coarse timestamps.
    fn rewrite(path: &Path, text: &str) {
        let before = Stamp::of(path);
        std::fs::write(path, text).expect("a write");
        let mut tries = 0;
        while Stamp::of(path) == before && tries < 50 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(path, text).expect("a write");
            tries += 1;
        }
    }

    #[test]
    fn a_clean_file_changed_on_disk_is_reloaded() {
        let dir = scratch("reload");
        let path = dir.join("a.txt");
        std::fs::write(&path, "old\n").expect("a write");
        let mut session = Session::new(Settings::with_defaults(), None, Platform::Linux);
        session.open(path.clone(), "old\n");
        let mut watch = DiskWatch::default();
        watch.remember(&path);

        assert!(watch.check(&mut session, 0).is_empty());
        rewrite(&path, "new\n");
        // Within the interval nothing is looked at.
        assert!(watch.check(&mut session, 10).is_empty());
        assert_eq!(
            watch.check(&mut session, 2000),
            [Change::Reloaded(path.clone())]
        );
        assert_eq!(session.document.buffer.text(), "new\n");
        assert!(watch.check(&mut session, 4000).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsaved_changes_are_kept_and_the_conflict_reported_once() {
        let dir = scratch("conflict");
        let path = dir.join("a.txt");
        std::fs::write(&path, "old\n").expect("a write");
        let mut session = Session::new(Settings::with_defaults(), None, Platform::Linux);
        session.open(path.clone(), "old\n");
        session.run("type", Some(&serde_json::json!({"text": "mine "})), 0);
        let mut watch = DiskWatch::default();
        watch.remember(&path);

        rewrite(&path, "theirs\n");
        assert_eq!(
            watch.check(&mut session, 0),
            [Change::Conflict(path.clone())]
        );
        assert_eq!(session.document.buffer.text(), "mine old\n");
        assert!(watch.check(&mut session, 2000).is_empty(), "reported once");
        assert!(watch.changed_since_known(&path), "saving should still ask");
        assert!(!watch.may_overwrite(&path), "the first save is refused");
        rewrite(&path, "theirs, again\n");
        assert!(
            !watch.may_overwrite(&path),
            "a version written after the question is asked about again"
        );
        assert!(watch.may_overwrite(&path), "the second save overwrites");
        assert!(
            !watch.may_overwrite(&path),
            "until it is written, it asks again"
        );

        std::fs::remove_file(&path).expect("a removal");
        assert_eq!(
            watch.check(&mut session, 4000),
            [Change::Gone(path.clone())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
