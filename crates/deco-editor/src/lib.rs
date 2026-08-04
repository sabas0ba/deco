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
pub mod session;

pub use commands::{Clipboard, Context, MemoryClipboard, Outcome};
pub use document::{Document, View};
pub use session::Session;
