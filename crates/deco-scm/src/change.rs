//! Changing a repository, rather than reading one.
//!
//! [`crate::status`] and [`mod@crate::diff`] answer questions. This is the
//! vocabulary of what can be *asked for*, and it lives here rather than in the
//! editor for the same reason [`Status`](crate::Status) does: the crate that
//! runs `git` owns what git can be told, and the editor decides which of those
//! things should happen.
//!
//! The division of labour is deco's usual one — the core decides and refuses,
//! whoever can spawn a process carries it out — so nothing here runs anything.
//! [`crate::Git::apply`] is the half that does.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A change to a repository, for a frontend to carry out.
///
/// The first thing in deco that *writes* to one. The same division of labour
/// the editor's file operations use — the core decides what should
/// happen and refuses what should not, whoever can run `git` carries it out —
/// and for the same reason: the core cannot spawn a process.
///
/// Every path is relative to the repository root, which is what
/// [`Status`](crate::Status) reports and what git answers about. By the time an
/// operation exists the question of *which* file is already settled.
///
/// # What is not here
///
/// **Discarding.** `git.clean` and `git checkout --` throw away work with no
/// undo and no trash, which is the same thing the tree's delete refuses to do
/// quietly. It is not built rather than built without a way back.
///
/// **Anything that reaches the network.** No push, no pull, no fetch. Those
/// need credentials, and a credential prompt is a thing an editor has to be
/// trusted with; reading and staging need neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    /// Add one file's working-tree state to the index.
    Stage(PathBuf),
    /// Add everything git reports as changed or untracked.
    ///
    /// Named separately rather than a loop of [`Operation::Stage`]: `git add`
    /// takes the whole set in one call, and doing it one file at a time would
    /// be a process each and a half-staged tree if one of them failed.
    StageAll,
    /// Take one file back out of the index, leaving the working tree alone.
    Unstage {
        /// What it is called in the index.
        path: PathBuf,
        /// The name it had before a staged rename, when it is one.
        ///
        /// Both halves have to go back together. Resetting only the new name
        /// leaves the old one staged *as a deletion* and the new one
        /// untracked — so a command that said it unstaged the rename would
        /// have left a commit that still deletes the original file. Verified
        /// against git 2.43 rather than reasoned about.
        original: Option<PathBuf>,
    },
    /// Record what is staged, with this message.
    Commit(String),
}

impl Operation {
    /// What to say once it has been done.
    pub fn describe(&self) -> String {
        match self {
            Self::Stage(path) => format!("staged {}", name_of(path)),
            Self::StageAll => "staged everything".to_owned(),
            Self::Unstage { path, .. } => format!("unstaged {}", name_of(path)),
            Self::Commit(_) => "committed".to_owned(),
        }
    }
}

/// A path's own name, for a message.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
