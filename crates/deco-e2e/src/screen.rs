//! What is on the screen, and how a scenario asserts about it.

use deco_theme::Rgba;
use deco_tui::render::Frame;

/// A painted frame, as characters.
///
/// The assertions check displayed text rather than session state. "The editor
/// opened the file" is a statement about a struct. "The file's name is on the
/// tab bar and its first line is on row two" is a statement about what the user
/// sees. Only the second detects a renderer that stopped drawing.
///
/// Every failure prints the whole screen, so the message shows what is
/// displayed instead of the missing string.
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
                    // `app::paint` sanitises every span before output, as the
                    // last step. The renderer sanitises document text, but a file
                    // name or a search result with untrusted bytes is made
                    // printable only in `app::paint`. Without this step the
                    // assertions would check strings no terminal receives.
                    .map(|span| deco_tui::render::sanitise(&span.text).into_owned())
                    .collect();
                // Trailing blanks are padding to the terminal's width. They are
                // removed so that scenarios do not need to include them.
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
    /// Returns `None` for a row the frame does not have or a column past the
    /// end of a row. The frame pads rows to the terminal's width, so in practice
    /// only a missing row returns `None`.
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
    /// Too few rows leave old content visible below the frame, and a row wider
    /// than the terminal wraps and scrolls the whole frame up.
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
