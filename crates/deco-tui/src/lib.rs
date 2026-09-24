//! Terminal frontend for deco.
//!
//! The crate has three separate parts:
//!
//! - [`keys`] converts terminal key events into chords and restores modifiers
//!   that the terminal drops or encodes differently, where possible.
//! - [`mod@render`] turns a session into a grid of styled cells. It is a pure
//!   function of the session and the terminal size, so the layout is tested
//!   in CI with no terminal attached.
//! - [`app`] owns the event loop. [`app::Driver`] is the event loop without the
//!   terminal. It runs the same path as a keystroke, from chord to command to the
//!   filesystem work the outcome requires, and can be driven with no terminal
//!   attached. [`run_with`] is the driver plus crossterm's events and stdout.
//!   It writes only to stdout.

pub mod app;
pub mod extensions;
pub mod files;
pub mod keys;
pub mod lsp;
pub mod render;
pub mod scm;
pub mod suggest;
pub mod themes;

pub use app::{run, run_with, Driver, Flow, Options, RemoteSession};
pub use render::{render, sanitise, Frame, Row, Span};
