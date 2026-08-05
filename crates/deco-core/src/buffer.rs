//! The rope-backed text buffer.

use ropey::{Rope, RopeSlice};

use crate::edit::Transaction;
use crate::position::{Position, Range};

/// How lines are terminated when the buffer is written back to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum LineEnding {
    /// `\n` — the default everywhere except Windows.
    #[default]
    Lf,
    /// `\r\n`.
    Crlf,
}

impl LineEnding {
    /// The literal characters this line ending is written as.
    pub const fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }

    /// The platform-native line ending, used when `files.eol` is `auto` and the
    /// document gives no evidence either way.
    pub const fn platform_default() -> Self {
        if cfg!(windows) {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        }
    }

    /// Infers the dominant line ending of `text`.
    ///
    /// VS Code decides this from the *first* terminator it sees rather than by
    /// majority vote, and files with no terminator at all fall back to the
    /// platform default. We match that so round-tripping a file through deco
    /// never rewrites every line silently.
    pub fn detect(text: &str) -> Self {
        match text.find('\n') {
            Some(0) => LineEnding::Lf,
            Some(idx) if text.as_bytes()[idx - 1] == b'\r' => LineEnding::Crlf,
            Some(_) => LineEnding::Lf,
            None => LineEnding::platform_default(),
        }
    }
}

/// True when `c` terminates a line as far as the rope is concerned.
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// Number of trailing characters of `line` that make up its terminator.
fn line_break_chars(line: RopeSlice<'_>) -> usize {
    let len = line.len_chars();
    if len == 0 {
        return 0;
    }
    let last = line.char(len - 1);
    if !is_line_break(last) {
        return 0;
    }
    if last == '\n' && len >= 2 && line.char(len - 2) == '\r' {
        2
    } else {
        1
    }
}

/// A text document: a rope plus the metadata an editor needs to round-trip it.
#[derive(Debug, Clone)]
pub struct Buffer {
    rope: Rope,
    line_ending: LineEnding,
    /// Whether the file ended with a terminator when it was read. Preserved so
    /// saving does not add or drop a trailing newline behind the user's back.
    final_newline: bool,
    version: i32,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// An empty buffer.
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            line_ending: LineEnding::platform_default(),
            final_newline: false,
            version: 0,
        }
    }

    /// Builds a buffer from text, inferring the line ending.
    ///
    /// The rope always stores `\n` internally regardless of the on-disk
    /// terminator; [`Buffer::to_disk_string`] re-applies it. This keeps every
    /// offset calculation in the editor free of CRLF special cases.
    pub fn from_text(text: &str) -> Self {
        let line_ending = LineEnding::detect(text);
        let normalized = if text.contains('\r') {
            text.replace("\r\n", "\n")
        } else {
            text.to_owned()
        };
        let final_newline = normalized.ends_with('\n');
        Self {
            rope: Rope::from_str(&normalized),
            line_ending,
            final_newline,
            version: 0,
        }
    }

    /// The buffer's contents with `\n` line endings.
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// The buffer's contents using the document's line ending, suitable for
    /// writing to disk.
    pub fn to_disk_string(&self) -> String {
        match self.line_ending {
            LineEnding::Lf => self.rope.to_string(),
            LineEnding::Crlf => self.rope.to_string().replace('\n', "\r\n"),
        }
    }

    /// Borrows the underlying rope.
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// The line ending used when saving.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Overrides the line ending used when saving (`files.eol`).
    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        self.line_ending = line_ending;
    }

    /// Whether the document ends with a line terminator.
    pub fn ends_with_newline(&self) -> bool {
        self.final_newline
    }

    /// A monotonically increasing counter, incremented once per applied
    /// transaction. This is the version reported to language servers.
    pub fn version(&self) -> i32 {
        self.version
    }

    /// Number of lines. An empty buffer has one (empty) line, matching the way
    /// editors present it.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines().max(1)
    }

    /// Total number of `char`s (Unicode scalar values).
    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Whether the buffer holds no text.
    pub fn is_empty(&self) -> bool {
        self.rope.len_chars() == 0
    }

    /// The line at `line`, including its terminator, or `None` if out of range.
    pub fn line(&self, line: usize) -> Option<RopeSlice<'_>> {
        if line >= self.line_count() {
            return None;
        }
        // `len_lines()` reports 0 for an empty rope but we present 1 line.
        if self.rope.len_lines() == 0 {
            return Some(self.rope.slice(..));
        }
        Some(self.rope.line(line))
    }

    /// The line at `line` without its terminator.
    pub fn line_content(&self, line: usize) -> Option<RopeSlice<'_>> {
        let slice = self.line(line)?;
        let trim = line_break_chars(slice);
        Some(slice.slice(..slice.len_chars() - trim))
    }

    /// Length of `line` excluding its terminator, in UTF-16 code units. This is
    /// the value a `Position.character` is clamped against.
    pub fn line_len_utf16(&self, line: usize) -> u32 {
        self.line_content(line)
            .map(|s| s.len_utf16_cu() as u32)
            .unwrap_or(0)
    }

    /// Length of `line` excluding its terminator, in `char`s.
    pub fn line_len_chars(&self, line: usize) -> usize {
        self.line_content(line).map(|s| s.len_chars()).unwrap_or(0)
    }

    /// The last valid position in the document.
    pub fn end_position(&self) -> Position {
        let line = self.line_count() - 1;
        Position::new(line as u32, self.line_len_utf16(line))
    }

    /// Clamps `pos` to a position that actually exists in this buffer.
    ///
    /// Out-of-range lines snap to the end of the document and out-of-range
    /// characters snap to the end of their line, matching VS Code's
    /// `TextModel.validatePosition`. Note that a too-large line does *not* just
    /// clamp the line number and keep the column — it lands on the very end of
    /// the document, which is what every editor command implicitly relies on
    /// after a concurrent edit truncated the file.
    pub fn clamp_position(&self, pos: Position) -> Position {
        let max_line = (self.line_count() - 1) as u32;
        if pos.line > max_line {
            return self.end_position();
        }
        let max_char = self.line_len_utf16(pos.line as usize);
        Position::new(pos.line, pos.character.min(max_char))
    }

    /// Clamps both ends of `range` and orders them.
    pub fn clamp_range(&self, range: Range) -> Range {
        Range::ordered(
            self.clamp_position(range.start),
            self.clamp_position(range.end),
        )
    }

    /// Converts a UTF-16 position to a `char` offset, clamping first.
    pub fn position_to_char(&self, pos: Position) -> usize {
        let pos = self.clamp_position(pos);
        let line = pos.line as usize;
        let line_start = self
            .rope
            .line_to_char(line.min(self.rope.len_lines().saturating_sub(1)));
        let Some(content) = self.line_content(line) else {
            return line_start;
        };
        // `utf16_cu_to_char` would panic past the end; clamp_position already
        // bounded `character`, so this is exact rather than defensive.
        line_start + content.utf16_cu_to_char(pos.character as usize)
    }

    /// Converts a `char` offset to a UTF-16 position, clamping first.
    pub fn char_to_position(&self, char_idx: usize) -> Position {
        let char_idx = char_idx.min(self.rope.len_chars());
        if self.rope.len_chars() == 0 {
            return Position::ZERO;
        }
        let line = self.rope.char_to_line(char_idx);
        let line_start = self.rope.line_to_char(line);
        let character = self.rope.slice(line_start..char_idx).len_utf16_cu() as u32;
        Position::new(line as u32, character)
    }

    /// The text covered by `range`.
    pub fn text_in_range(&self, range: Range) -> String {
        let range = self.clamp_range(range);
        let start = self.position_to_char(range.start);
        let end = self.position_to_char(range.end);
        self.rope.slice(start..end).to_string()
    }

    /// Applies `tx` and returns the transaction that undoes it.
    ///
    /// Changes are applied from the end of the document backwards so that the
    /// offsets of the not-yet-applied changes stay valid, which is why the
    /// caller may pass them in any order.
    pub fn apply(&mut self, tx: &Transaction) -> Transaction {
        // Resolve every range against the *pre-edit* document first: the caller's
        // positions all refer to that snapshot.
        let mut resolved: Vec<(usize, usize, &str)> = tx
            .changes()
            .iter()
            .map(|c| {
                let range = self.clamp_range(c.range);
                (
                    self.position_to_char(range.start),
                    self.position_to_char(range.end),
                    &*c.text,
                )
            })
            .collect();
        resolved.sort_by_key(|(start, end, _)| (*start, *end));

        let mut removed: Vec<String> = Vec::with_capacity(resolved.len());
        for (start, end, _) in &resolved {
            removed.push(self.rope.slice(*start..*end).to_string());
        }

        for (start, end, text) in resolved.iter().rev() {
            if end > start {
                self.rope.remove(*start..*end);
            }
            if !text.is_empty() {
                self.rope.insert(*start, text);
            }
        }

        // Build the inverse against the *post-edit* document, walking forwards
        // and carrying the running offset delta of the earlier changes.
        let mut inverse = Vec::with_capacity(resolved.len());
        let mut delta: isize = 0;
        for (idx, (start, end, text)) in resolved.iter().enumerate() {
            let inserted = text.chars().count();
            let new_start = (*start as isize + delta) as usize;
            let new_end = new_start + inserted;
            inverse.push(crate::edit::Change {
                range: Range::new(
                    self.char_to_position(new_start),
                    self.char_to_position(new_end),
                ),
                text: std::mem::take(&mut removed[idx]),
            });
            delta += inserted as isize - (*end - *start) as isize;
        }

        self.version += 1;
        self.final_newline =
            self.rope.len_chars() > 0 && self.rope.char(self.rope.len_chars() - 1) == '\n';

        Transaction::from_changes_unchecked(inverse)
    }

    /// Replaces the entire contents, keeping the line-ending setting.
    pub fn set_text(&mut self, text: &str) {
        let replacement = Self::from_text(text);
        self.rope = replacement.rope;
        self.final_newline = replacement.final_newline;
        self.version += 1;
    }
}

impl std::str::FromStr for Buffer {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Buffer::from_text(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Change;

    fn buf(text: &str) -> Buffer {
        Buffer::from_text(text)
    }

    #[test]
    fn detects_line_endings() {
        assert_eq!(LineEnding::detect("a\r\nb"), LineEnding::Crlf);
        assert_eq!(LineEnding::detect("a\nb"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("\na"), LineEnding::Lf);
        assert_eq!(
            LineEnding::detect("no newline"),
            LineEnding::platform_default()
        );
    }

    #[test]
    fn crlf_is_normalised_internally_and_restored_on_save() {
        let b = buf("one\r\ntwo\r\n");
        assert_eq!(b.text(), "one\ntwo\n");
        assert_eq!(b.line_ending(), LineEnding::Crlf);
        assert_eq!(b.to_disk_string(), "one\r\ntwo\r\n");
        // Offsets see no CRLF, so the second line starts where you'd expect.
        assert_eq!(b.position_to_char(Position::new(1, 0)), 4);
    }

    #[test]
    fn empty_buffer_has_one_line() {
        let b = Buffer::new();
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line_len_utf16(0), 0);
        assert_eq!(b.end_position(), Position::ZERO);
        assert_eq!(b.position_to_char(Position::new(5, 5)), 0);
    }

    #[test]
    fn line_content_excludes_terminator() {
        let b = buf("alpha\nbeta\n");
        assert_eq!(b.line_content(0).unwrap().to_string(), "alpha");
        assert_eq!(b.line_content(1).unwrap().to_string(), "beta");
        assert_eq!(b.line(0).unwrap().to_string(), "alpha\n");
    }

    #[test]
    fn utf16_positions_account_for_surrogate_pairs() {
        // "😀" is one char but two UTF-16 code units; "あ" is one of each.
        let b = buf("a😀あb");
        assert_eq!(b.line_len_utf16(0), 1 + 2 + 1 + 1);

        assert_eq!(b.position_to_char(Position::new(0, 0)), 0);
        assert_eq!(b.position_to_char(Position::new(0, 1)), 1); // before emoji
        assert_eq!(b.position_to_char(Position::new(0, 3)), 2); // after emoji
        assert_eq!(b.position_to_char(Position::new(0, 4)), 3); // after あ

        assert_eq!(b.char_to_position(2), Position::new(0, 3));
        assert_eq!(b.char_to_position(4), Position::new(0, 5));
    }

    #[test]
    fn position_round_trips_through_char_offsets() {
        let b = buf("héllo\n😀world\nCJK漢字\n");
        for idx in 0..=b.len_chars() {
            let pos = b.char_to_position(idx);
            assert_eq!(
                b.position_to_char(pos),
                idx,
                "round trip failed at char {idx}"
            );
        }
    }

    #[test]
    fn clamping_snaps_out_of_range_positions() {
        let b = buf("ab\ncdef");
        assert_eq!(b.clamp_position(Position::new(0, 99)), Position::new(0, 2));
        assert_eq!(b.clamp_position(Position::new(99, 0)), Position::new(1, 4));
    }

    #[test]
    fn apply_single_insert_and_its_inverse() {
        let mut b = buf("hello world");
        let tx = Transaction::single(Change::insert(Position::new(0, 5), ",".into()));
        let undo = b.apply(&tx);
        assert_eq!(b.text(), "hello, world");
        b.apply(&undo);
        assert_eq!(b.text(), "hello world");
    }

    #[test]
    fn apply_single_delete_and_its_inverse() {
        let mut b = buf("hello, world");
        let tx = Transaction::single(Change::delete(Range::new(
            Position::new(0, 5),
            Position::new(0, 6),
        )));
        let undo = b.apply(&tx);
        assert_eq!(b.text(), "hello world");
        b.apply(&undo);
        assert_eq!(b.text(), "hello, world");
    }

    #[test]
    fn multi_cursor_edits_apply_atomically_and_invert() {
        // Three carets typing "X" at once, given in scrambled order.
        let mut b = buf("aa\nbb\ncc");
        let tx = Transaction::new(vec![
            Change::insert(Position::new(2, 1), "X".into()),
            Change::insert(Position::new(0, 1), "X".into()),
            Change::insert(Position::new(1, 1), "X".into()),
        ])
        .unwrap();
        let undo = b.apply(&tx);
        assert_eq!(b.text(), "aXa\nbXb\ncXc");
        b.apply(&undo);
        assert_eq!(b.text(), "aa\nbb\ncc");
    }

    #[test]
    fn multi_change_replacement_of_differing_lengths_inverts() {
        let mut b = buf("foo bar foo");
        let tx = Transaction::new(vec![
            Change::replace(
                Range::new(Position::new(0, 0), Position::new(0, 3)),
                "hello".into(),
            ),
            Change::replace(
                Range::new(Position::new(0, 8), Position::new(0, 11)),
                "x".into(),
            ),
        ])
        .unwrap();
        let undo = b.apply(&tx);
        assert_eq!(b.text(), "hello bar x");
        b.apply(&undo);
        assert_eq!(b.text(), "foo bar foo");
    }

    #[test]
    fn edits_spanning_lines_invert() {
        let mut b = buf("one\ntwo\nthree\n");
        let tx = Transaction::single(Change::replace(
            Range::new(Position::new(0, 1), Position::new(2, 2)),
            "X\nY".into(),
        ));
        let undo = b.apply(&tx);
        assert_eq!(b.text(), "oX\nYree\n");
        b.apply(&undo);
        assert_eq!(b.text(), "one\ntwo\nthree\n");
    }

    #[test]
    fn version_increments_per_transaction() {
        let mut b = buf("x");
        assert_eq!(b.version(), 0);
        b.apply(&Transaction::single(Change::insert(
            Position::new(0, 1),
            "y".into(),
        )));
        assert_eq!(b.version(), 1);
    }

    #[test]
    fn tracks_trailing_newline_across_edits() {
        let mut b = buf("a\n");
        assert!(b.ends_with_newline());
        b.apply(&Transaction::single(Change::delete(Range::new(
            Position::new(0, 1),
            Position::new(1, 0),
        ))));
        assert_eq!(b.text(), "a");
        assert!(!b.ends_with_newline());
    }

    #[test]
    fn text_in_range_reads_across_lines() {
        let b = buf("one\ntwo\nthree");
        let r = Range::new(Position::new(0, 1), Position::new(2, 2));
        assert_eq!(b.text_in_range(r), "ne\ntwo\nth");
    }
}
