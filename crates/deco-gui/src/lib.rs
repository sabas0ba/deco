//! GPU-accelerated frontend for deco.
//!
//! Split the same way the terminal frontend is:
//!
//! - [`keys`] converts window key events into chords.
//! - [`mod@layout`] turns a session into positioned text and rectangles. It is a
//!   pure function of the session and the window size, so the layout is
//!   asserted in CI on a machine with no GPU.
//! - [`app`] owns the winit event loop and the wgpu/glyphon device work, and is
//!   the only part that needs a display.

pub mod app;
pub mod keys;
pub mod layout;

pub use app::run;
pub use layout::{layout, Colors, Layout, Metrics, Rect};
