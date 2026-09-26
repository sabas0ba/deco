//! What `git` says about a workspace.
//!
//! Like VS Code, deco runs the `git` binary and reads its output. No library is
//! linked, so this crate's only dependencies are `thiserror` and `serde`. On a
//! machine without git, the feature is unavailable rather than broken.
//!
//! [`status`] is a pure parser. It takes the text written by
//! `git status --porcelain=v2` and returns a [`Status`], without processes,
//! filesystem access or clocks, so each output format can be tested with a
//! string literal. [`git`] spawns the process. Its core is one function that
//! runs an argument vector in a directory and returns stdout.
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
