//! The frontend-agnostic editor session.
//!
//! Everything a user can do to text lives here, addressed by VS Code's command
//! identifiers. The terminal and GPU frontends are then thin: translate a key
//! event into a chord, ask [`deco_keymap`] which command it resolves to, and
//! call [`commands::execute`].
//!
//! Nothing in this crate knows what a terminal or a window is, which is what
//! lets the entire editable surface be tested headlessly.

pub mod commands;
pub mod document;
pub mod find;
pub mod input;
pub mod layout;
pub mod prompt;
pub mod session;
pub mod workspace;

pub use commands::{Clipboard, Context, MemoryClipboard, Outcome};
pub use document::{Document, View};
pub use find::Find;
pub use prompt::{Prompt, PromptKind};
pub use session::{Pane, Session};
pub use workspace::{Applied, Plan, WorkspaceError};
