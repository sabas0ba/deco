//! The frontend-agnostic editor session.
//!
//! Everything a user can do to text lives here, addressed by VS Code's command
//! identifiers. The terminal and GPU frontends only translate a key event into
//! a chord, ask [`deco_keymap`] which command it resolves to, and call
//! [`commands::execute`].
//!
//! This crate does not depend on a terminal or a window, so the entire
//! editable surface can be tested headlessly.

pub mod commands;
pub mod document;
pub mod explorer;
pub mod files;
pub mod find;
pub mod input;
pub mod layout;
pub mod prompt;
pub mod scm;
pub mod session;
mod snippet;
pub mod workspace;

pub use commands::{Clipboard, Context, MemoryClipboard, Outcome};
pub use deco_scm::Operation as GitOperation;
pub use document::{Document, View};
pub use explorer::{Explorer, Row as ExplorerRow};
pub use files::{FileError, Operation as FileOperation};
pub use find::Find;
pub use prompt::{Prompt, PromptKind};
pub use scm::{Group as ScmGroup, Row as ScmRow, SourceControl};
pub use session::{
    ComparisonLine, ComparisonLineKind, ComparisonPane, Focus, Pane, Session, SideBarView,
};
pub use workspace::{Applied, Plan, WorkspaceError};
