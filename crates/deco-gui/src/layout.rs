//! Turning a session into positioned text and rectangles.
//!
//! Like the terminal frontend's renderer, this is a pure function of the
//! session and the window size. The GPU code in [`crate::app`] only uploads
//! what this produces, which keeps the parts that can be tested in CI separate
//! from the parts that need a graphics device.
//!
//! Positions assume a monospace font, so a column is a fixed number of pixels.
//! Proportional fonts need the shaper's own advances; glyphon can supply them,
//! and this module is where that would go.

use deco_core::movement::display_column;
use deco_editor::Session;
use deco_theme::Rgba;

/// Font and spacing measurements for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// Font size in pixels.
    pub font_size: f32,
    /// Baseline-to-baseline distance in pixels.
    pub line_height: f32,
    /// Advance width of one character.
    pub cell_width: f32,
    /// Left padding before the gutter.
    pub padding: f32,
}

impl Metrics {
    /// Derives metrics from the document's settings.
    ///
    /// The 0.6 ratio is a reasonable approximation for the advance width of a
    /// monospace face; [`crate::app`] replaces it with the real advance once the
    /// font is loaded.
    pub fn from_session(session: &Session, scale: f32) -> Self {
        let settings = &session.document.settings;
        let font_size = settings.font_size * scale;
        Self {
            font_size,
            line_height: settings.effective_line_height() * scale,
            cell_width: (font_size * 0.6).max(1.0),
            padding: 8.0 * scale,
        }
    }
}

/// An axis-aligned rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

/// One laid-out line of the document.
#[derive(Debug, Clone, PartialEq)]
pub struct LaidOutLine {
    /// The document line this came from.
    pub line: usize,
    /// Top edge in pixels.
    pub y: f32,
    /// The gutter label, already padded.
    pub gutter: String,
    /// The line's text, tabs expanded.
    pub text: String,
    /// Whether this is the line the primary cursor is on.
    pub is_cursor_line: bool,
}

/// Everything needed to draw one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// The visible lines.
    pub lines: Vec<LaidOutLine>,
    /// Selection highlight rectangles.
    pub selections: Vec<Rect>,
    /// The caret, if it is on screen.
    pub cursor: Option<Rect>,
    /// The line-highlight rectangle behind the cursor's line.
    pub current_line: Option<Rect>,
    /// Where the text column starts, in pixels.
    pub text_left: f32,
    /// Colours resolved from the theme.
    pub colors: Colors,
}

/// The colours one frame needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    /// Editor background.
    pub background: Rgba,
    /// Text.
    pub foreground: Rgba,
    /// Gutter text.
    pub gutter: Rgba,
    /// Gutter text on the cursor's line.
    pub gutter_active: Rgba,
    /// Selection highlight.
    pub selection: Rgba,
    /// Current-line highlight.
    pub current_line: Rgba,
    /// The caret.
    pub cursor: Rgba,
}

impl Colors {
    /// Reads the colours from a session's theme.
    pub fn from_session(session: &Session) -> Self {
        let theme = &session.theme;
        let background = theme.color("editor.background").unwrap_or(Rgba::BLACK);
        let foreground = theme.color("editor.foreground").unwrap_or(Rgba::WHITE);
        Self {
            background,
            foreground,
            gutter: theme
                .color("editorLineNumber.foreground")
                .unwrap_or(foreground),
            gutter_active: theme
                .color("editorLineNumber.activeForeground")
                .unwrap_or(foreground),
            // Unlike the terminal, the GPU can blend, so translucent theme
            // colours are kept as-is and composited at draw time.
            selection: theme
                .color("editor.selectionBackground")
                .unwrap_or(foreground),
            current_line: theme
                .color("editor.lineHighlightBackground")
                .unwrap_or(Rgba::TRANSPARENT),
            cursor: theme.color("editorCursor.foreground").unwrap_or(foreground),
        }
    }
}

/// Expands tabs in `text` to `tab_size` stops.
fn expand_tabs(text: &str, tab_size: usize) -> String {
    if !text.contains('\t') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut column = 0usize;
    for c in text.chars() {
        if c == '\t' {
            let stop = tab_size - (column % tab_size);
            out.extend(std::iter::repeat_n(' ', stop));
            column += stop;
        } else {
            out.push(c);
            column += 1;
        }
    }
    out
}

/// Lays out one frame.
pub fn layout(session: &Session, width: f32, height: f32, metrics: Metrics) -> Layout {
    let buffer = &session.document.buffer;
    let tab_size = session.document.settings.tab_size;
    let colors = Colors::from_session(session);

    let digits = buffer.line_count().to_string().len().max(2);
    let gutter_columns = if session.document.settings.line_numbers == deco_config::LineNumbers::Off
    {
        0
    } else {
        digits + 2
    };
    let text_left = metrics.padding + gutter_columns as f32 * metrics.cell_width;

    let rows = ((height / metrics.line_height).ceil() as usize).max(1);
    let cursor = session.view.cursor();
    let cursor_line = cursor.line as usize;

    let mut lines = Vec::new();
    let mut selections = Vec::new();

    for row in 0..rows {
        let line = session.view.scroll_top + row;
        if line >= buffer.line_count() {
            break;
        }
        let y = row as f32 * metrics.line_height;
        let raw = buffer
            .line_content(line)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let text = expand_tabs(&raw, tab_size);

        let gutter = if gutter_columns == 0 {
            String::new()
        } else {
            let label = match session.document.settings.line_numbers {
                deco_config::LineNumbers::Relative if line != cursor_line => (line as i64
                    - cursor_line as i64)
                    .unsigned_abs()
                    .to_string(),
                _ => (line + 1).to_string(),
            };
            format!("{label:>digits$}", digits = gutter_columns - 1)
        };

        // One rectangle per selection per line; a selection ending past the end
        // of the line is drawn one cell wider so the newline is visible.
        for selection in session.view.selections.iter().filter(|s| !s.is_empty()) {
            let (start, end) = (selection.start(), selection.end());
            if (line as u32) < start.line || (line as u32) > end.line {
                continue;
            }
            let from = if start.line == line as u32 {
                start.character
            } else {
                0
            };
            let line_end = buffer.line_len_utf16(line);
            let to = if end.line == line as u32 {
                end.character
            } else {
                line_end
            };
            let extra = if (line as u32) < end.line { 1.0 } else { 0.0 };

            let x0 = display_column(&raw, from, tab_size) as f32;
            let x1 = display_column(&raw, to, tab_size) as f32 + extra;
            selections.push(Rect {
                x: text_left + x0 * metrics.cell_width,
                y,
                width: (x1 - x0).max(0.0) * metrics.cell_width,
                height: metrics.line_height,
            });
        }

        lines.push(LaidOutLine {
            line,
            y,
            gutter,
            text,
            is_cursor_line: line == cursor_line,
        });
    }

    let cursor_row = cursor_line.checked_sub(session.view.scroll_top);
    let on_screen = cursor_row.is_some_and(|row| row < rows);
    let caret = on_screen.then(|| {
        let raw = buffer
            .line_content(cursor_line)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let column = display_column(&raw, cursor.character, tab_size) as f32;
        Rect {
            x: text_left + column * metrics.cell_width,
            y: cursor_row.unwrap_or(0) as f32 * metrics.line_height,
            // A thin caret regardless of DPI; block cursors are a setting away.
            width: (metrics.cell_width * 0.12).max(1.0),
            height: metrics.line_height,
        }
    });

    let current_line = on_screen.then(|| Rect {
        x: 0.0,
        y: cursor_row.unwrap_or(0) as f32 * metrics.line_height,
        width,
        height: metrics.line_height,
    });

    Layout {
        lines,
        selections,
        cursor: caret,
        current_line,
        text_left,
        colors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_core::{Position, Selection, SelectionSet};
    use std::path::PathBuf;

    fn session(text: &str) -> Session {
        let mut session = Session::with_defaults();
        session.open(PathBuf::from("/w/file.rs"), text);
        session
    }

    fn metrics() -> Metrics {
        Metrics {
            font_size: 14.0,
            line_height: 20.0,
            cell_width: 8.0,
            padding: 8.0,
        }
    }

    #[test]
    fn lines_are_spaced_by_the_line_height() {
        let laid = layout(&session("a\nb\nc"), 400.0, 200.0, metrics());
        assert_eq!(laid.lines[0].y, 0.0);
        assert_eq!(laid.lines[1].y, 20.0);
        assert_eq!(laid.lines[2].y, 40.0);
    }

    #[test]
    fn only_visible_lines_are_laid_out() {
        let laid = layout(&session(&"x\n".repeat(1000)), 400.0, 100.0, metrics());
        // 100px at 20px per line is five rows.
        assert_eq!(laid.lines.len(), 5);
    }

    #[test]
    fn layout_starts_at_the_scroll_position() {
        let mut session = session(&"x\n".repeat(100));
        session.view.scroll_top = 40;
        let laid = layout(&session, 400.0, 100.0, metrics());
        assert_eq!(laid.lines[0].line, 40);
    }

    #[test]
    fn the_gutter_is_right_aligned_and_widens_with_the_file() {
        let narrow = layout(&session("a\nb"), 400.0, 100.0, metrics());
        // Two digits' worth of width plus one column of padding, with the
        // label right-aligned in it.
        assert_eq!(narrow.lines[0].gutter, "  1");

        let wide = layout(&session(&"x\n".repeat(1000)), 400.0, 100.0, metrics());
        assert!(wide.text_left > narrow.text_left);
    }

    #[test]
    fn relative_line_numbers_count_from_the_cursor() {
        let mut session = session("a\nb\nc\nd");
        session.document.settings.line_numbers = deco_config::LineNumbers::Relative;
        session.view.selections = SelectionSet::caret(Position::new(2, 0));
        let laid = layout(&session, 400.0, 200.0, metrics());
        assert_eq!(laid.lines[0].gutter.trim(), "2");
        assert_eq!(
            laid.lines[2].gutter.trim(),
            "3",
            "the cursor line stays absolute"
        );
        assert!(laid.lines[2].is_cursor_line);
    }

    #[test]
    fn turning_off_line_numbers_removes_the_gutter() {
        let mut session = session("a");
        session.document.settings.line_numbers = deco_config::LineNumbers::Off;
        let laid = layout(&session, 400.0, 100.0, metrics());
        assert_eq!(laid.lines[0].gutter, "");
        assert_eq!(laid.text_left, metrics().padding);
    }

    #[test]
    fn tabs_are_expanded_for_drawing() {
        let laid = layout(&session("\tx"), 400.0, 100.0, metrics());
        assert_eq!(laid.lines[0].text, "    x");
    }

    #[test]
    fn tab_expansion_respects_tab_stops() {
        assert_eq!(expand_tabs("ab\tx", 4), "ab  x");
        assert_eq!(expand_tabs("abcd\tx", 4), "abcd    x");
        assert_eq!(expand_tabs("no tabs", 4), "no tabs");
    }

    #[test]
    fn the_caret_sits_after_the_gutter() {
        let mut session = session("hello");
        session.view.selections = SelectionSet::caret(Position::new(0, 3));
        let laid = layout(&session, 400.0, 100.0, metrics());
        let caret = laid.cursor.unwrap();
        assert_eq!(caret.x, laid.text_left + 3.0 * 8.0);
        assert_eq!(caret.y, 0.0);
    }

    #[test]
    fn the_caret_accounts_for_tab_expansion() {
        let mut session = session("\tx");
        session.view.selections = SelectionSet::caret(Position::new(0, 1));
        let laid = layout(&session, 400.0, 100.0, metrics());
        assert_eq!(laid.cursor.unwrap().x, laid.text_left + 4.0 * 8.0);
    }

    #[test]
    fn the_caret_is_absent_when_scrolled_off_screen() {
        let mut session = session(&"x\n".repeat(100));
        session.view.selections = SelectionSet::caret(Position::new(90, 0));
        session.view.scroll_top = 0;
        let laid = layout(&session, 400.0, 100.0, metrics());
        assert_eq!(laid.cursor, None);
        assert_eq!(laid.current_line, None);
    }

    #[test]
    fn a_selection_becomes_a_rectangle() {
        let mut session = session("hello world");
        session.view.selections =
            SelectionSet::single(Selection::new(Position::new(0, 2), Position::new(0, 7)));
        let laid = layout(&session, 400.0, 100.0, metrics());
        assert_eq!(laid.selections.len(), 1);
        let rect = laid.selections[0];
        assert_eq!(rect.x, laid.text_left + 2.0 * 8.0);
        assert_eq!(rect.width, 5.0 * 8.0);
        assert_eq!(rect.height, 20.0);
    }

    #[test]
    fn a_multi_line_selection_produces_one_rectangle_per_line() {
        let mut session = session("aaa\nbbb\nccc");
        session.view.selections =
            SelectionSet::single(Selection::new(Position::new(0, 1), Position::new(2, 1)));
        let laid = layout(&session, 400.0, 100.0, metrics());
        assert_eq!(laid.selections.len(), 3);
        // The middle line covers its whole text plus the newline.
        assert!(laid.selections[1].width > 3.0 * 8.0);
    }

    #[test]
    fn multiple_cursors_each_get_their_own_rectangle() {
        let mut session = session("aaaa\nbbbb");
        session.view.selections = SelectionSet::from_vec(
            vec![
                Selection::new(Position::new(0, 0), Position::new(0, 2)),
                Selection::new(Position::new(1, 0), Position::new(1, 2)),
            ],
            0,
        );
        assert_eq!(
            layout(&session, 400.0, 100.0, metrics()).selections.len(),
            2
        );
    }

    #[test]
    fn an_empty_selection_draws_nothing() {
        let laid = layout(&session("hello"), 400.0, 100.0, metrics());
        assert!(laid.selections.is_empty());
    }

    #[test]
    fn the_current_line_highlight_spans_the_window() {
        let laid = layout(&session("a\nb"), 640.0, 100.0, metrics());
        let highlight = laid.current_line.unwrap();
        assert_eq!(highlight.x, 0.0);
        assert_eq!(highlight.width, 640.0);
    }

    #[test]
    fn colours_come_from_the_theme() {
        let session = session("a");
        let colors = Colors::from_session(&session);
        assert_eq!(
            colors.background,
            session.theme.color("editor.background").unwrap()
        );
        assert_ne!(colors.background, colors.foreground);
    }

    #[test]
    fn metrics_scale_with_the_display() {
        let session = session("a");
        let single = Metrics::from_session(&session, 1.0);
        let double = Metrics::from_session(&session, 2.0);
        assert_eq!(double.font_size, single.font_size * 2.0);
        assert_eq!(double.line_height, single.line_height * 2.0);
    }

    #[test]
    fn an_empty_document_still_lays_out() {
        let session = Session::with_defaults();
        let laid = layout(&session, 400.0, 100.0, metrics());
        assert_eq!(laid.lines.len(), 1);
        assert!(laid.cursor.is_some());
    }
}
