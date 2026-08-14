//! Terminal frontend for deco.
//!
//! Three pieces, deliberately separable:
//!
//! - [`keys`] converts terminal key events into chords, undoing as much of the
//!   terminal's modifier mangling as it can.
//! - [`mod@render`] turns a session into a grid of styled cells. It is a pure
//!   function of the session and the terminal size, so the layout is asserted
//!   in CI with no terminal attached.
//! - [`app`] owns the event loop. [`app::Driver`] is that loop with the terminal
//!   taken out of it: the same path a keystroke takes, from chord to command to
//!   the filesystem work its outcome asks for, drivable with no terminal
//!   attached. [`run_with`] is that driver plus crossterm's events and stdout,
//!   and stdout is still the only thing it touches.

pub mod app;
pub mod extensions;
pub mod files;
pub mod keys;
pub mod lsp;
pub mod render;
pub mod suggest;
pub mod themes;

pub use app::{run, run_with, Driver, Flow, Options};
pub use render::{render, sanitise, Frame, Row, Span};
