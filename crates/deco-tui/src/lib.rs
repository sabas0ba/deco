//! Terminal frontend for deco.
//!
//! Three pieces, deliberately separable:
//!
//! - [`keys`] converts terminal key events into chords, undoing as much of the
//!   terminal's modifier mangling as it can.
//! - [`mod@render`] turns a session into a grid of styled cells. It is a pure
//!   function of the session and the terminal size, so the layout is asserted
//!   in CI with no terminal attached.
//! - [`app`] owns the event loop and is the only part that touches stdout.

pub mod app;
pub mod keys;
pub mod render;

pub use app::run;
pub use render::{render, Frame, Row, Span};
