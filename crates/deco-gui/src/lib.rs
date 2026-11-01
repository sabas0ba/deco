//! GPU-accelerated frontend for deco.
//!
//! The window shows the editor the terminal frontend shows. Both are driven by
//! [`deco_tui::Driver`], which decides what a key does and carries out what
//! needs files, language servers, Git or a terminal, and both draw the
//! [`deco_tui::Frame`] it renders: rows of coloured cells. This frontend places
//! those cells in pixels and draws them with the GPU.
//!
//! - [`keys`] converts window key events into chords.
//! - [`mod@grid`] places a frame's cells in pixels. It is a pure function of the
//!   frame and the font measurements, so it is tested in CI on a machine with
//!   no GPU.
//! - [`quads`] turns rectangles into vertices and draws them.
//! - [`app`] owns the winit event loop and the wgpu/glyphon device work, and is
//!   the only part that needs a display.

pub mod app;
pub mod grid;
pub mod keys;
pub mod quads;

pub use app::run;
pub use grid::{grid, Grid, Metrics, Quad, Rect};
