//! What `git` says about a workspace.
//!
//! deco does what VS Code does: it runs the `git` binary and reads its output.
//! No library is linked, so this crate's only dependency is `thiserror` and a
//! machine without git has the feature *absent* rather than broken.
//!
//! The split is the one the rest of deco uses. [`status`] is a pure parser —
//! hand it the text `git status --porcelain=v2` writes and it hands back a
//! [`Status`], with no process, no filesystem and no clock involved, so every
//! shape git can produce is a test with a string literal in it. [`git`] is the
//! part that spawns, and is deliberately thin: one function that runs an
//! argument vector in a directory and returns stdout.
//!
//! ```
//! use deco_scm::{Head, parse};
//!
//! // What `git status --porcelain=v2 --branch -z` writes, NULs and all.
//! let status = parse("# branch.oid 1c9d4e5\0# branch.head main\0? new.rs\0")?;
//!
//! assert_eq!(status.head, Head::Branch("main".into()));
//! assert_eq!(status.summary(), "main ±1");
//! # Ok::<(), deco_scm::Malformed>(())
//! ```
//!
//! # What this does not do
//!
//! Operation covers staging, unstaging, committing and switching existing local
//! branches. It deliberately excludes discarding work and anything that reaches
//! the network; each has failure and credential behaviour beyond an index update.
//! See the [git chapter](https://github.com/sabas0ba/deco/blob/main/docs/roadmap.md)
//! for what remains.

#![deny(missing_docs)]

pub mod change;
pub mod diff;
pub mod git;
pub mod status;

pub use change::{Branch, CheckoutPlan, Comparison, ComparisonKind, ComparisonRequest, Operation};
pub use diff::{diff, Diff, Hunk, Mark};
pub use git::{Git, ScmError};
pub use status::{parse, Change, FileStatus, Head, Malformed, State, Status, Upstream};
