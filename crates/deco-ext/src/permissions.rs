//! Permission decisions that outlive the session they were made in.
//!
//! A decision remembered only in memory means being asked again every time the
//! editor starts, which is the shape of prompt that teaches people to stop
//! reading prompts. So they are written down.
//!
//! # What a stored decision is a decision about
//!
//! **This version of this extension.** Every entry records the version it was
//! decided for, and a decision about a version that is no longer installed is
//! ignored — the user is asked again. A grant on disk otherwise outlives the
//! reason it was given: an extension that was allowed to read the workspace at
//! 1.0.0 is different code at 1.1.0, and carrying the answer across would be
//! allowing something without having seen what it now does. That is the one rule
//! here worth arguing about, and it is deliberately the strict reading.
//!
//! An update therefore costs a prompt. That is the intended price.
//!
//! # What this file is
//!
//! JSON next to `settings.json`, `0600` on Unix. Not because it is secret —
//! nothing in it is — but because anything that can write it can grant
//! capabilities to code that runs as you, so it must not be a file another
//! account can edit.
//!
//! A file that cannot be read or does not parse is reported and treated as
//! empty. Refusing to start an editor because a permissions file is damaged
//! would be a worse failure than asking again.

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
/// A `BTreeMap` so the file is written in a stable order: a permissions file that
/// reshuffles itself on every save is one nobody can read a diff of.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Permissions {
    by_extension: BTreeMap<String, Remembered>,
}

/// Why a permissions file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum PermissionsError {
    /// The file is there and could not be read.
    #[error("could not read {path}: {source}")]
    Unreadable {
        /// The file that was tried.
        path: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
    /// The file is there and is not what this expects.
    #[error("{path} is not a permissions file deco understands: {source}")]
    Malformed {
        /// The file that was tried.
        path: PathBuf,
        /// What the parser said.
        source: serde_json::Error,
    },
    /// The file could not be written.
    #[error("could not write {path}: {source}")]
    Unwritable {
        /// The file that was tried.
        path: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
}

impl Permissions {
    /// Reads the decisions from `path`.
    ///
    /// A missing file is no decisions rather than an error: not having decided
    /// anything yet is the ordinary state of a new installation.
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
        // An empty file is what an interrupted write leaves behind, and it is not
        // valid JSON. Treated as no decisions, because that is what it holds.
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
    /// Written to a temporary name and renamed, so a crash mid-write leaves the
    /// previous decisions rather than a truncated file — the same reason the
    /// installer stages a binary beside its destination.
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

    /// What was decided about `id` at `version`, if anything still applies.
    ///
    /// Nothing when the stored decision was about a different version: that is
    /// the rule this module exists to enforce.
    pub fn for_extension(&self, id: &str, version: &str) -> Option<&GrantStore> {
        self.by_extension
            .get(id)
            .filter(|remembered| remembered.version == version)
            .map(|remembered| &remembered.grants)
    }

    /// Whether `id` has decisions stored for some *other* version.
    ///
    /// Worth saying out loud when it happens: "you are being asked again because
    /// this extension was updated" is the difference between a prompt that makes
    /// sense and one that looks like deco forgot.
    pub fn stale_for(&self, id: &str, version: &str) -> Option<&str> {
        self.by_extension
            .get(id)
            .filter(|remembered| remembered.version != version)
            .map(|remembered| remembered.version.as_str())
    }

    /// Replaces what is remembered about `id` at `version`.
    ///
    /// An entry whose decisions are empty is removed rather than stored: a
    /// version with nothing decided about it is indistinguishable from one that
    /// was never asked about, and keeping it would grow the file forever.
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
/// A no-op off Unix, where the permission model is different and deco has nothing
/// useful to say about it — stated rather than silently skipped.
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
        // The rule this module is for. A grant given to 1.0.0 is a decision about
        // code that is no longer what is installed.
        let mut permissions = Permissions::default();
        permissions.set("acme.tools", "1.0.0", granted());

        assert!(permissions.for_extension("acme.tools", "1.1.0").is_none());
        assert!(permissions.for_extension("acme.tools", "1.0.0").is_some());
        // And the reason is available, so the prompt can say why it is asking.
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

        // An empty file is what an interrupted write leaves, and it holds no
        // decisions — which is different from holding something unreadable.
        std::fs::write(&path, "").expect("a file");
        assert!(Permissions::load(&path).expect("a read").is_empty());
        let _ = std::fs::remove_dir_all(path.parent().expect("a parent"));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;

        // Nothing in it is secret. What matters is the other direction: anything
        // that can write this file can grant capabilities to code that runs as
        // you.
        let path = scratch("mode");
        let mut permissions = Permissions::default();
        permissions.set("acme.tools", "1.0.0", granted());
        permissions.save(&path).expect("a write");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        // And the staging file it was renamed from is gone.
        assert!(!path.with_extension("json.incoming").exists());
        let _ = std::fs::remove_dir_all(path.parent().expect("a parent"));
    }
}
