//! What is on the screen, and how a scenario asserts about it.

use deco_theme::Rgba;
use deco_tui::render::Frame;

/// A painted frame, as characters.
///
/// The assertions are deliberately about text rather than about the session:
/// "the editor opened the file" is a claim about a struct, and "the file's name
/// is on the tab bar and its first line is on row two" is a claim about what the
/// person in front of it can see. Only the second one catches a renderer that
/// stopped drawing.
///
/// Every failure prints the whole screen, because the useful question about a
/// missing string is never "is it missing" but "what is there instead".
pub struct Screen {
    lines: Vec<String>,
    frame: Frame,
    size: (u16, u16),
}

impl Screen {
    pub(crate) fn of(frame: Frame, size: (u16, u16)) -> Self {
        let lines = frame
            .rows
            .iter()
            .map(|row| {
                let painted: String = row
                    .spans
                    .iter()
                    // What the terminal would receive, not what the frame holds.
                    // `app::paint` substitutes every span on its way out, and it
                    // is the last thing that does: the renderer substitutes a
                    // document's own text, but a file name or a search result
                    // carrying somebody else's bytes is only made printable here.
                    // A screen that skipped this step would be asserting about a
                    // string no terminal ever sees.
                    .map(|span| deco_tui::render::sanitise(&span.text).into_owned())
                    .collect();
                // Trailing blanks are padding to the terminal's width, and a
                // scenario that had to write them out would be asserting about
                // the padding.
                painted.trim_end().to_owned()
            })
            .collect();
        Self { lines, frame, size }
    }

    /// Every row, top to bottom.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// One row, or `""` for a row the frame does not have.
    pub fn line(&self, row: usize) -> &str {
        self.lines.get(row).map(String::as_str).unwrap_or_default()
    }

    /// The whole screen as one string, rows separated by newlines.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The bottom row, which is where deco's status line is.
    pub fn status_line(&self) -> &str {
        self.lines.last().map(String::as_str).unwrap_or_default()
    }

    /// Whether any row contains `needle`.
    pub fn shows(&self, needle: &str) -> bool {
        self.lines.iter().any(|line| line.contains(needle))
    }

    /// The first row containing `needle`.
    pub fn row_of(&self, needle: &str) -> Option<usize> {
        self.lines.iter().position(|line| line.contains(needle))
    }

    /// Where the caret is, as a column and a row.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.frame.cursor
    }

    /// The colours of the cell at `(row, column)`, foreground then background.
    ///
    /// `None` past the end of a row: the frame pads to the terminal's width, so
    /// this is only ever `None` for a row the frame does not have.
    pub fn colours_at(&self, row: usize, column: usize) -> Option<(Rgba, Rgba)> {
        let row = self.frame.rows.get(row)?;
        let mut seen = 0usize;
        for span in &row.spans {
            let width = span.text.chars().count();
            if column < seen + width {
                return Some((span.fg, span.bg));
            }
            seen += width;
        }
        None
    }

    /// The frame itself, for the assertions this type does not make.
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    // ---- assertions -------------------------------------------------------

    /// Fails unless some row contains `needle`.
    #[track_caller]
    pub fn assert_shows(&self, needle: &str) -> &Self {
        assert!(
            self.shows(needle),
            "nothing on screen contains {needle:?}\n{}",
            self.dump()
        );
        self
    }

    /// Fails unless no row contains `needle`.
    #[track_caller]
    pub fn assert_lacks(&self, needle: &str) -> &Self {
        assert!(
            !self.shows(needle),
            "{needle:?} is on screen and should not be\n{}",
            self.dump()
        );
        self
    }

    /// Fails unless row `row` contains `needle`.
    #[track_caller]
    pub fn assert_row_shows(&self, row: usize, needle: &str) -> &Self {
        assert!(
            self.line(row).contains(needle),
            "row {row} is {:?}, which does not contain {needle:?}\n{}",
            self.line(row),
            self.dump()
        );
        self
    }

    /// Fails unless the bottom row contains `needle`.
    #[track_caller]
    pub fn assert_status(&self, needle: &str) -> &Self {
        assert!(
            self.status_line().contains(needle),
            "the status line is {:?}, which does not contain {needle:?}\n{}",
            self.status_line(),
            self.dump()
        );
        self
    }

    /// Fails unless the frame is exactly as tall and as wide as the terminal.
    ///
    /// A row short of the height leaves whatever was underneath it on screen,
    /// and a row wider than the width wraps and pushes the whole frame up.
    #[track_caller]
    pub fn assert_fits(&self) -> &Self {
        let (width, height) = self.size;
        assert_eq!(
            self.frame.rows.len(),
            height as usize,
            "the frame is {} rows for a {height}-row terminal\n{}",
            self.frame.rows.len(),
            self.dump()
        );
        for (index, row) in self.frame.rows.iter().enumerate() {
            let painted: usize = row.spans.iter().map(|span| span.text.chars().count()).sum();
            assert!(
                painted <= width as usize,
                "row {index} paints {painted} cells into a {width}-cell terminal\n{}",
                self.dump()
            );
        }
        self
    }

    /// The screen, framed, for an assertion message.
    pub fn dump(&self) -> String {
        let width = self.size.0 as usize;
        let mut out = String::from("\n");
        out.push_str(&format!("┌{}┐\n", "─".repeat(width)));
        for line in &self.lines {
            let padding = width.saturating_sub(line.chars().count());
            out.push_str(&format!("│{line}{}│\n", " ".repeat(padding)));
        }
        out.push_str(&format!("└{}┘", "─".repeat(width)));
        out
    }
}
