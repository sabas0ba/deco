//! Types for changing a repository, as opposed to reading it.
//!
//! [`crate::status`] and [`mod@crate::diff`] read repository state. This module
//! defines the operations that can be requested. It lives here rather than in
//! the editor for the same reason as [`Status`](crate::Status): the crate that
//! runs `git` defines which git operations exist, and the editor decides which
//! of them to perform.
//!
//! As elsewhere in deco, the core decides and rejects, and the component that
//! can spawn processes executes. Nothing in this module runs a process;
//! [`crate::Git::apply`] does.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One local branch offered by the checkout picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// The short local name, such as `main` or `feature/search`.
    pub name: String,
    /// Whether `HEAD` currently names this branch.
    pub current: bool,
}

/// The changes a branch switch would make, computed before confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutPlan {
    /// The current branch or detached commit label.
    pub current: String,
    /// The local branch selected in the picker.
    pub target: String,
    /// Committed paths whose contents differ between the two branches.
    pub branch_changes: usize,
    /// Changes already in the index that Git will carry across or refuse.
    pub staged: usize,
    /// Tracked working-tree changes that Git will carry across or refuse.
    pub unstaged: usize,
    /// Untracked files that remain in the working tree.
    pub untracked: usize,
}

impl CheckoutPlan {
    /// A compact summary of the plan, for the confirmation row.
    pub fn summary(&self) -> String {
        let local = self.staged + self.unstaged + self.untracked;
        if local == 0 {
            format!(
                "{} path{} · clean worktree",
                self.branch_changes,
                if self.branch_changes == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "{} path{} · staged {}/unstaged {}/untracked {}",
                self.branch_changes,
                if self.branch_changes == 1 { "" } else { "s" },
                self.staged,
                self.unstaged,
                self.untracked,
            )
        }
    }
}

/// Which two repository states a diff view compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonKind {
    /// `HEAD` on the left and the index on the right.
    Staged,
    /// The index on the left and the working tree on the right.
    WorkingTree,
}

/// One file to compare, in repository-relative coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonRequest {
    /// The path in the index and working tree.
    pub path: PathBuf,
    /// The path in `HEAD` for a staged rename.
    pub original: Option<PathBuf>,
    /// Which repository states form the two sides.
    pub kind: ComparisonKind,
}

/// The two texts shown by a source-control diff view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comparison {
    /// The older side: `HEAD` or the index.
    pub original: Option<String>,
    /// The newer side: the index or the working tree.
    pub modified: Option<String>,
}

/// A change to a repository, for a frontend to carry out.
///
/// These are the only operations in deco that write to a repository. They
/// follow the same split as the editor's file operations: the core decides what
/// should happen and rejects what should not, and the component that can run
/// `git` executes it, because the core cannot spawn a process.
///
/// Every path is relative to the repository root, matching
/// [`Status`](crate::Status) and git's own output. The target file is fixed
/// when the operation is created.
///
/// # Not supported
///
/// **Discarding changes.** `git.clean` and `git checkout --` discard work with
/// no undo and no trash, which the file tree's delete also does not do without
/// confirmation. The feature is omitted rather than implemented without a way
/// to recover.
///
/// **Network operations.** No push, pull or fetch. These require credentials,
/// and handling credential prompts requires the editor to be trusted with them.
/// Reading and staging require neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    /// Add one file's working-tree state to the index.
    Stage(PathBuf),
    /// Add everything git reports as changed or untracked.
    ///
    /// A separate operation rather than a loop of [`Operation::Stage`]:
    /// `git add` takes the whole set in one call. Staging one file at a time
    /// would start a process per file and leave a partially staged tree if one
    /// call failed.
    StageAll,
    /// Remove one file from the index, leaving the working tree unchanged.
    Unstage {
        /// The file's path in the index.
        path: PathBuf,
        /// The path before a staged rename, if the change is a rename.
        ///
        /// Both paths must be reset together. Resetting only the new path
        /// leaves the old one staged as a deletion and the new one untracked,
        /// so the next commit would still delete the original file. Verified
        /// with git 2.43.
        original: Option<PathBuf>,
    },
    /// Commit the staged changes with this message.
    Commit(String),
    /// Switch to an existing local branch without discarding local work.
    ///
    /// The target is checked against [`crate::Git::branches`] immediately
    /// before it reaches Git. No force flag is used: when a switch would
    /// overwrite a tracked or untracked change, Git refuses it.
    Checkout(String),
}

impl Operation {
    /// A message describing the completed operation.
    pub fn describe(&self) -> String {
        match self {
            Self::Stage(path) => format!("staged {}", name_of(path)),
            Self::StageAll => "staged everything".to_owned(),
            Self::Unstage { path, .. } => format!("unstaged {}", name_of(path)),
            Self::Commit(_) => "committed".to_owned(),
            Self::Checkout(branch) => format!("switch to {branch}"),
        }
    }
}

/// The file name of a path, for a message.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
