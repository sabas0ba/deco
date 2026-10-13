//! The integrated terminal: what a program in it shows, and the pseudo-terminal
//! it runs in.
//!
//! - [`parser`] splits the program's output into actions, following the DEC
//!   parser model that xterm implements.
//! - [`screen`] applies those actions to a grid of cells with a cursor and a
//!   history.
//! - [`input`] turns keys into the bytes a program expects from a terminal.
//! - [`pty`] starts a program in a pseudo-terminal and moves bytes in and out.
//!
//! Everything except [`pty`] is pure, so the editor core can hold a screen and
//! tests can check what a sequence of bytes draws without starting a program.
//! No terminal-emulation crate is used: the parser and the screen are written
//! here, and the pseudo-terminal is opened with `libc` and `windows-sys`.

#![deny(missing_docs)]

pub mod input;
pub mod parser;
pub mod pty;
pub mod screen;

pub use screen::Screen;

/// A screen and the parser feeding it.
#[derive(Debug, Clone)]
pub struct Terminal {
    parser: parser::Parser,
    screen: Screen,
}

impl Terminal {
    /// An empty terminal of `cols` by `rows` that keeps `scrollback` lines.
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        Self {
            parser: parser::Parser::new(),
            screen: Screen::new(cols, rows, scrollback),
        }
    }

    /// Applies what the program wrote.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.screen, bytes);
    }

    /// The screen, for drawing.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// The screen, for resizing and taking replies.
    pub fn screen_mut(&mut self) -> &mut Screen {
        &mut self.screen
    }
}
