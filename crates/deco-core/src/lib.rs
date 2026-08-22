//! Core text model for the deco editor.
//!
//! The model is deliberately frontend-agnostic: neither the terminal nor the GPU
//! frontend appears anywhere below this line. Everything a frontend needs to
//! render is derivable from [`Buffer`] plus a [`SelectionSet`].
//!
//! # Position semantics
//!
//! deco speaks the same coordinate system as VS Code and the Language Server
//! Protocol: a [`Position`] is a zero-based line plus a zero-based offset in
//! **UTF-16 code units**. Internally the text is a rope indexed by `char`
//! (Unicode scalar values), so conversions happen at the boundary — see
//! [`Buffer::position_to_char`] and [`Buffer::char_to_position`]. Getting this
//! wrong is the classic source of off-by-one bugs with astral-plane characters
//! (emoji, CJK extension B), so the conversion is centralised here and tested
//! against those cases directly.

pub mod buffer;
pub mod edit;
pub mod history;
pub mod movement;
pub mod position;
pub mod search;
pub mod selection;
pub mod wrap;

pub use buffer::{Buffer, LineEnding};
pub use edit::{Change, Transaction};
pub use history::{EditKind, Group, History, HistoryOptions};
pub use movement::{
    Granularity, HorizontalDirection, VerticalDirection, WordCategory, DEFAULT_WORD_SEPARATORS,
};
pub use position::{Position, Range};
pub use selection::{Selection, SelectionSet};

/// Errors produced by the core text model.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CoreError {
    /// A position referred to a line that does not exist in the buffer.
    #[error("line {line} is out of bounds (buffer has {line_count} lines)")]
    LineOutOfBounds {
        /// The requested line.
        line: usize,
        /// Number of lines the buffer actually has.
        line_count: usize,
    },
    /// A range had its end before its start after normalisation.
    #[error("invalid range: {start:?} > {end:?}")]
    InvalidRange {
        /// Range start.
        start: Position,
        /// Range end.
        end: Position,
    },
}

/// Result alias for the core text model.
pub type Result<T> = std::result::Result<T, CoreError>;
