//! Permission decisions that persist across sessions.
//!
//! If decisions were kept only in memory, the user would be asked again every
//! time the editor starts. Repeated prompts train users to stop reading them, so
//! decisions are stored on disk.
//!
//! # Scope of a stored decision
//!
//! A decision applies to **this version of this extension**. Every entry records
//! the version it was made for. A decision for a version that is no longer
//! installed is ignored, and the user is asked again. An extension that was
//! allowed to read the workspace at 1.0.0 is different code at 1.1.0, and a grant
//! must not carry across an update the user has not reviewed. This is the strict
//! interpretation, chosen on purpose.
//!
//! An update therefore causes a new prompt. This is intended.
//!
//! # File format
//!
//! JSON next to `settings.json`, mode `0600` on Unix. The contents are not
//! secret, but anything that can write the file can grant capabilities to code
//! that runs as the user, so other accounts must not be able to edit it.
//!
//! A file that cannot be read or does not parse is reported and treated as
//! empty. Asking again is better than refusing to start the editor because the
//! permissions file is damaged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capability::GrantStore;

/// What was decided about one extension.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Remembered {
    /// The version the decisions were made about.
    #[serde(default)]
    pub version: String,
    /// The decisions themselves.
    #[serde(flatten)]
    pub grants: GrantStore,
}

/// Every extension's remembered decisions, as they are stored.
///
/// A `BTreeMap` so the file is written in a stable order. Without it, entries
/// would be reordered on every save and diffs would be unreadable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Permissions {
    by_extension: BTreeMap<String, Remembered>,
}

/// Why a permissions file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum PermissionsError {
    /// The file exists and could not be read.
    #[error("could not read {path}: {source}")]
    Unreadable {
        /// The file that was tried.
        path: PathBuf,
        /// The operating system error.
        source: std::io::Error,
    },
    /// The file exists and is not in the expected format.
    #[error("{path} is not a permissions file deco understands: {source}")]
    Malformed {
        /// The file that was tried.
        path: PathBuf,
        /// The parser error.
        source: serde_json::Error,
    },
    /// The file could not be written.
    #[error("could not write {path}: {source}")]
    Unwritable {
        /// The file that was tried.
        path: PathBuf,
        /// The operating system error.
        source: std::io::Error,
    },
}

impl Permissions {
    /// Reads the decisions from `path`.
    ///
    /// A missing file means no decisions, not an error. A new installation has
    /// no decisions yet.
    pub fn load(path: &Path) -> Result<Self, PermissionsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(source) => {
                return Err(PermissionsError::Unreadable {
                    path: path.to_owned(),
                    source,
                })
            }
        };
        // An interrupted write can leave an empty file, which is not valid JSON.
        // It holds no decisions, so treat it as empty.
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text).map_err(|source| PermissionsError::Malformed {
            path: path.to_owned(),
            source,
        })
    }

    /// Writes the decisions to `path`, creating its directory if it is missing.
    ///
    /// Written to a temporary name and renamed, so a crash during the write leaves
    /// the previous decisions instead of a truncated file. The installer stages a
    /// binary beside its destination for the same reason.
    pub fn save(&self, path: &Path) -> Result<(), PermissionsError> {
        let unwritable = |source: std::io::Error| PermissionsError::Unwritable {
            path: path.to_owned(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(unwritable)?;
        }
        let text = serde_json::to_string_pretty(self).expect("decisions are serialisable");
        let staged = path.with_extension("json.incoming");
        std::fs::write(&staged, format!("{text}\n")).map_err(unwritable)?;
        restrict(&staged).map_err(unwritable)?;
        std::fs::rename(&staged, path).map_err(unwritable)
    }

    /// The decisions for `id` at `version`, if any still apply.
    ///
    /// Returns `None` when the stored decisions are for a different version. This
    /// is the main rule this module enforces.
    pub fn for_extension(&self, id: &str, version: &str) -> Option<&GrantStore> {
        self.by_extension
            .get(id)
            .filter(|remembered| remembered.version == version)
            .map(|remembered| &remembered.grants)
    }

    /// Whether `id` has decisions stored for some *other* version.
    ///
    /// Lets the prompt explain that the user is asked again because the extension
    /// was updated. Without that explanation, the prompt looks like deco lost the
    /// earlier decision.
    pub fn stale_for(&self, id: &str, version: &str) -> Option<&str> {
        self.by_extension
            .get(id)
            .filter(|remembered| remembered.version != version)
            .map(|remembered| remembered.version.as_str())
    }

    /// Replaces what is remembered about `id` at `version`.
    ///
    /// An entry with no decisions is removed instead of stored. It is equivalent
    /// to having no entry, and keeping it would make the file grow without bound.
    pub fn set(&mut self, id: &str, version: &str, grants: GrantStore) {
        if grants.allowed.is_empty() && grants.denied.is_empty() {
            self.by_extension.remove(id);
            return;
        }
        self.by_extension.insert(
            id.to_owned(),
            Remembered {
                version: version.to_owned(),
                grants,
            },
        );
    }

    /// Whether nothing at all is remembered.
    pub fn is_empty(&self) -> bool {
        self.by_extension.is_empty()
    }
}

/// Makes a file readable and writable by its owner alone.
///
/// A no-op on platforms other than Unix, which use a different permission model
/// that deco does not configure.
fn restrict(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, PathScope};

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "deco-permissions-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root.join("permissions.json")
    }

    fn read_workspace() -> Capability {
        Capability::ReadFile {
            scope: PathScope::Workspace,
        }
    }

    fn granted() -> GrantStore {
        GrantStore {
            allowed: vec![read_workspace()],
            denied: Vec::new(),
        }
    }

    #[test]
    fn a_decision_survives_being_written_and_read_back() {
        let path = scratch("round-trip");
        let mut permissions = Permissions::default();
        permissions.set("acme.tools", "1.0.0", granted());
        permissions.save(&path).expect("a write");

        let read = Permissions::load(&path).expect("a read");
        assert_eq!(read, permissions);
        assert_eq!(
            read.for_extension("acme.tools", "1.0.0")
                .map(|g| g.allowed.clone()),
            Some(vec![read_workspace()])
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("a parent"));
    }

    #[test]
    fn an_update_means_the_decision_no_longer_applies() {
        // The main rule of this module. A grant for 1.0.0 applies to code that is
        // no longer installed.
        let mut permissions = Permissions::default();
        permissions.set("acme.tools", "1.0.0", granted());

        assert!(permissions.for_extension("acme.tools", "1.1.0").is_none());
        assert!(permissions.for_extension("acme.tools", "1.0.0").is_some());
        // The previous version is available, so the prompt can explain why it asks.
        assert_eq!(permissions.stale_for("acme.tools", "1.1.0"), Some("1.0.0"));
        assert_eq!(permissions.stale_for("acme.tools", "1.0.0"), None);
    }

    #[test]
    fn forgetting_everything_removes_the_entry_rather_than_storing_an_empty_one() {
        let mut permissions = Permissions::default();
        permissions.set("acme.tools", "1.0.0", granted());
        permissions.set("acme.tools", "1.0.0", GrantStore::default());
        assert!(permissions.is_empty());
    }

    #[test]
    fn a_missing_file_is_no_decisions_rather_than_a_failure() {
        let path = scratch("missing");
        assert!(Permissions::load(&path).expect("a read").is_empty());
    }

    #[test]
    fn a_damaged_file_is_refused_by_name_rather_than_guessed_at() {
        let path = scratch("damaged");
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        std::fs::write(&path, "{ not json").expect("a file");
        let error = Permissions::load(&path).expect_err("a refusal");
        assert!(
            matches!(error, PermissionsError::Malformed { .. }),
            "{error}"
        );

        // An interrupted write can leave an empty file. It holds no decisions,
        // which is different from holding unreadable content.
        std::fs::write(&path, "").expect("a file");
        assert!(Permissions::load(&path).expect("a read").is_empty());
        let _ = std::fs::remove_dir_all(path.parent().expect("a parent"));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;

        // The contents are not secret. The concern is write access: anything that
        // can write this file can grant capabilities to code that runs as the
        // user.
        let path = scratch("mode");
        let mut permissions = Permissions::default();
        permissions.set("acme.tools", "1.0.0", granted());
        permissions.save(&path).expect("a write");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        // The staging file was renamed and no longer exists.
        assert!(!path.with_extension("json.incoming").exists());
        let _ = std::fs::remove_dir_all(path.parent().expect("a parent"));
    }
}
