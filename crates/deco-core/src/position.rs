//! Zero-based line/character positions, wire-compatible with LSP and VS Code.

/// A zero-based position in a document.
///
/// `character` counts **UTF-16 code units** from the start of the line, exactly
/// as `vscode.Position` and `lsp.Position` do. It is not a byte offset and not
/// a `char` offset; use [`crate::Buffer`] to convert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Position {
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based offset within the line, in UTF-16 code units.
    pub character: u32,
}

impl Position {
    /// The start of the document.
    pub const ZERO: Position = Position {
        line: 0,
        character: 0,
    };

    /// Constructs a position.
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }

    /// Returns this position with `character` replaced.
    pub const fn with_character(self, character: u32) -> Self {
        Self {
            line: self.line,
            character,
        }
    }
}

/// A half-open range `[start, end)` of a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Range {
    /// Inclusive start.
    pub start: Position,
    /// Exclusive end.
    pub end: Position,
}

impl Range {
    /// Constructs a range without reordering. Prefer [`Range::ordered`] when the
    /// operands may be reversed (e.g. derived from a selection).
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// An empty range at `pos`.
    pub const fn empty(pos: Position) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }

    /// Constructs a range, swapping the operands if they are reversed.
    pub fn ordered(a: Position, b: Position) -> Self {
        if a <= b {
            Self { start: a, end: b }
        } else {
            Self { start: b, end: a }
        }
    }

    /// Whether the range covers no text.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Whether the range starts and ends on the same line.
    pub fn is_single_line(&self) -> bool {
        self.start.line == self.end.line
    }

    /// Whether `pos` falls within `[start, end)`, or equals `start` for an empty
    /// range.
    pub fn contains(&self, pos: Position) -> bool {
        if self.is_empty() {
            pos == self.start
        } else {
            pos >= self.start && pos < self.end
        }
    }

    /// Whether the two ranges share at least one position, treating empty ranges
    /// as touching a range they sit inside.
    pub fn intersects(&self, other: &Range) -> bool {
        self.start < other.end && other.start < self.end
            || self.is_empty() && other.contains(self.start)
            || other.is_empty() && self.contains(other.start)
    }

    /// The smallest range covering both operands.
    pub fn union(&self, other: &Range) -> Range {
        Range {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_order_by_line_then_character() {
        assert!(Position::new(0, 5) < Position::new(1, 0));
        assert!(Position::new(1, 2) < Position::new(1, 3));
        assert_eq!(Position::new(2, 2), Position::new(2, 2));
    }

    #[test]
    fn ordered_swaps_reversed_operands() {
        let r = Range::ordered(Position::new(3, 0), Position::new(1, 4));
        assert_eq!(r.start, Position::new(1, 4));
        assert_eq!(r.end, Position::new(3, 0));
    }

    #[test]
    fn contains_is_half_open() {
        let r = Range::new(Position::new(0, 2), Position::new(0, 5));
        assert!(!r.contains(Position::new(0, 1)));
        assert!(r.contains(Position::new(0, 2)));
        assert!(r.contains(Position::new(0, 4)));
        assert!(!r.contains(Position::new(0, 5)));
    }

    #[test]
    fn empty_range_contains_only_its_own_position() {
        let r = Range::empty(Position::new(1, 1));
        assert!(r.contains(Position::new(1, 1)));
        assert!(!r.contains(Position::new(1, 2)));
    }

    #[test]
    fn intersects_handles_empty_ranges_inside_others() {
        let outer = Range::new(Position::new(0, 0), Position::new(0, 10));
        let caret = Range::empty(Position::new(0, 4));
        assert!(outer.intersects(&caret));
        assert!(caret.intersects(&outer));

        let outside = Range::empty(Position::new(0, 20));
        assert!(!outer.intersects(&outside));
    }

    #[test]
    fn adjacent_ranges_do_not_intersect() {
        let a = Range::new(Position::new(0, 0), Position::new(0, 3));
        let b = Range::new(Position::new(0, 3), Position::new(0, 6));
        assert!(!a.intersects(&b));
    }
}
