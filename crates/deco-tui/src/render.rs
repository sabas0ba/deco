//! Turning a session into a grid of styled cells.
//!
//! Rendering is a pure function of the session plus the terminal size, and
//! produces a [`Frame`] rather than writing to the terminal. That split is what
//! lets the layout — gutter width, selection highlighting, tab expansion,
//! status bar — be asserted in CI with no terminal attached.

use deco_config::LineNumbers;
use deco_core::movement::display_column;
use deco_editor::Session;
use deco_theme::Rgba;
use unicode_width::UnicodeWidthChar;

/// A run of characters sharing one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The characters.
    pub text: String,
    /// Foreground colour.
    pub fg: Rgba,
    /// Background colour.
    pub bg: Rgba,
}

/// One terminal row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    /// The row's spans, left to right.
    pub spans: Vec<Span>,
}

impl Row {
    /// The row's text with styling discarded, for assertions and for tests.
    pub fn plain(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// A complete frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The rows, top to bottom.
    pub rows: Vec<Row>,
    /// Where to put the terminal cursor, if it is on screen.
    pub cursor: Option<(u16, u16)>,
}

/// Colours pulled out of the theme once per frame.
struct Palette {
    fg: Rgba,
    bg: Rgba,
    gutter_fg: Rgba,
    gutter_active_fg: Rgba,
    selection_bg: Rgba,
    status_fg: Rgba,
    status_bg: Rgba,
}

impl Palette {
    fn from(session: &Session) -> Self {
        let theme = &session.theme;
        let bg = theme.color("editor.background").unwrap_or(Rgba::BLACK);
        let fg = theme.color("editor.foreground").unwrap_or(Rgba::WHITE);
        Self {
            fg,
            bg,
            gutter_fg: theme.color("editorLineNumber.foreground").unwrap_or(fg),
            gutter_active_fg: theme
                .color("editorLineNumber.activeForeground")
                .unwrap_or(fg),
            // Selections are usually translucent, and a terminal has no alpha,
            // so they are composited against the editor background here.
            selection_bg: theme
                .color("editor.selectionBackground")
                .map(|c| c.over(bg))
                .unwrap_or(fg),
            status_fg: theme.color("statusBar.foreground").unwrap_or(fg),
            status_bg: theme.color("statusBar.background").unwrap_or(bg),
        }
    }
}

/// Number of columns the line-number gutter needs.
pub fn gutter_width(session: &Session) -> usize {
    if session.document.settings.line_numbers == LineNumbers::Off {
        return 0;
    }
    let digits = session.document.buffer.line_count().to_string().len();
    // One space of padding on each side keeps the text off the numbers.
    digits.max(2) + 2
}

/// Renders `session` into a `width` x `height` frame.
pub fn render(session: &Session, width: usize, height: usize) -> Frame {
    let palette = Palette::from(session);
    let gutter = gutter_width(session);
    // The last row is the status bar, so the text area is one shorter.
    let text_height = height.saturating_sub(1);
    let text_width = width.saturating_sub(gutter);

    let mut rows = Vec::with_capacity(height);
    let buffer = &session.document.buffer;
    let tab_size = session.document.settings.tab_size;
    let cursor_line = session.view.cursor().line as usize;

    let mut cursor_cell = None;

    for row_index in 0..text_height {
        let line = session.view.scroll_top + row_index;
        if line >= buffer.line_count() {
            rows.push(Row {
                spans: vec![blank(width, palette.bg)],
            });
            continue;
        }

        let mut spans = Vec::new();
        if gutter > 0 {
            let label = match session.document.settings.line_numbers {
                LineNumbers::Relative if line != cursor_line => (line as i64 - cursor_line as i64)
                    .unsigned_abs()
                    .to_string(),
                _ => (line + 1).to_string(),
            };
            spans.push(Span {
                text: format!("{label:>width$} ", width = gutter - 1),
                fg: if line == cursor_line {
                    palette.gutter_active_fg
                } else {
                    palette.gutter_fg
                },
                bg: palette.bg,
            });
        }

        let text = buffer
            .line_content(line)
            .map(|s| s.to_string())
            .unwrap_or_default();
        spans.extend(line_spans(
            session, &text, line, text_width, tab_size, &palette,
        ));
        rows.push(Row { spans });

        if line == cursor_line {
            let column = display_column(&text, session.view.cursor().character, tab_size);
            if column < text_width {
                cursor_cell = Some(((gutter + column) as u16, row_index as u16));
            }
        }
    }

    rows.push(status_bar(session, width, &palette));
    Frame {
        rows,
        cursor: cursor_cell,
    }
}

/// A run of spaces, used to pad rows to the full width.
fn blank(width: usize, bg: Rgba) -> Span {
    Span {
        text: " ".repeat(width),
        fg: bg,
        bg,
    }
}

/// Builds the styled spans for one line of text.
fn line_spans(
    session: &Session,
    text: &str,
    line: usize,
    width: usize,
    tab_size: usize,
    palette: &Palette,
) -> Vec<Span> {
    // Expand tabs first: the terminal has no tab stops of its own once we are
    // positioning the cursor by column. `column` counts *terminal columns*, not
    // characters — a CJK character occupies two, so padding by character count
    // would push every row past the right edge.
    let mut cells: Vec<(char, bool)> = Vec::new();
    let mut column = 0usize;
    let mut utf16 = 0u32;

    let selected_ranges: Vec<(u32, u32)> = session
        .view
        .selections
        .iter()
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let (start, end) = (s.start(), s.end());
            if (line as u32) < start.line || (line as u32) > end.line {
                return None;
            }
            let from = if start.line == line as u32 {
                start.character
            } else {
                0
            };
            let to = if end.line == line as u32 {
                end.character
            } else {
                u32::MAX
            };
            Some((from, to))
        })
        .collect();

    let is_selected = |utf16: u32| {
        selected_ranges
            .iter()
            .any(|(from, to)| utf16 >= *from && utf16 < *to)
    };

    for c in text.chars() {
        let selected = is_selected(utf16);
        let advance = if c == '\t' {
            tab_size - (column % tab_size)
        } else {
            c.width().unwrap_or(1).max(1)
        };
        // Stop before a character that would straddle the right edge rather
        // than half-drawing it.
        if column + advance > width {
            break;
        }
        if c == '\t' {
            for _ in 0..advance {
                cells.push((' ', selected));
            }
        } else {
            cells.push((c, selected));
        }
        column += advance;
        utf16 += c.len_utf16() as u32;
    }

    // A selection that runs past the end of the line is drawn one cell wide, so
    // that selecting a line break is visible rather than invisible.
    if is_selected(utf16) && column < width {
        cells.push((' ', true));
        column += 1;
    }
    while column < width {
        cells.push((' ', false));
        column += 1;
    }

    // Coalesce runs sharing a style; one span per character would be correct
    // but would make the terminal writer do far more work than it needs to.
    let mut spans: Vec<Span> = Vec::new();
    for (c, selected) in cells {
        let bg = if selected {
            palette.selection_bg
        } else {
            palette.bg
        };
        match spans.last_mut() {
            Some(last) if last.bg == bg => last.text.push(c),
            _ => spans.push(Span {
                text: c.to_string(),
                fg: palette.fg,
                bg,
            }),
        }
    }
    spans
}

/// The status bar.
fn status_bar(session: &Session, width: usize, palette: &Palette) -> Row {
    let cursor = session.view.cursor();
    let dirty = if session.document.dirty { "*" } else { "" };
    let language = session.document.language().unwrap_or("plain text");

    let left = match session.view.chord.pending() {
        Some(chord) => format!(" {chord} was pressed. Waiting for the second key… "),
        None => match &session.status {
            Some(message) => format!(" {message} "),
            None => format!(" {}{} ", session.document.title(), dirty),
        },
    };
    let right = format!(
        " {}  Ln {}, Col {} ",
        language,
        cursor.line + 1,
        cursor.character + 1
    );

    let used = left.chars().count() + right.chars().count();
    let padding = width.saturating_sub(used);
    // Build the full bar, then force it to exactly `width`. Truncating `left`
    // alone under-fills the row whenever `left` is itself shorter than the
    // terminal, which leaves the previous frame's pixels showing.
    let mut text: String = format!("{left}{}{right}", " ".repeat(padding))
        .chars()
        .take(width)
        .collect();
    while text.chars().count() < width {
        text.push(' ');
    }

    Row {
        spans: vec![Span {
            text,
            fg: palette.status_fg,
            bg: palette.status_bg,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_core::{Position, Selection, SelectionSet};
    use std::path::PathBuf;

    /// A session pinned to the Linux keymap.
    ///
    /// Not `Session::with_defaults()`: that builds the keymap for the host, and
    /// a test that presses `ctrl+k` would be pressing an unbound key on macOS,
    /// where the default is `cmd+k`.
    fn session(text: &str) -> Session {
        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );
        session.open(PathBuf::from("/w/file.rs"), text);
        session
    }

    #[test]
    fn a_frame_is_exactly_the_requested_size() {
        let frame = render(&session("a\nb\nc"), 40, 10);
        assert_eq!(frame.rows.len(), 10);
        for row in &frame.rows {
            assert_eq!(row.plain().chars().count(), 40, "row was {:?}", row.plain());
        }
    }

    #[test]
    fn line_numbers_are_right_aligned_in_the_gutter() {
        let session = session("a\nb\nc");
        let frame = render(&session, 40, 5);
        assert!(frame.rows[0].plain().starts_with("  1 "));
        assert!(frame.rows[1].plain().starts_with("  2 "));
    }

    #[test]
    fn the_gutter_widens_for_longer_files() {
        let narrow = session("a\nb");
        let wide = session(&"x\n".repeat(1000));
        assert!(gutter_width(&wide) > gutter_width(&narrow));
    }

    #[test]
    fn the_gutter_disappears_when_line_numbers_are_off() {
        let mut session = session("a\nb");
        session.document.settings.line_numbers = LineNumbers::Off;
        assert_eq!(gutter_width(&session), 0);
        assert!(render(&session, 20, 3).rows[0].plain().starts_with('a'));
    }

    #[test]
    fn relative_line_numbers_count_from_the_cursor() {
        let mut session = session("a\nb\nc\nd");
        session.document.settings.line_numbers = LineNumbers::Relative;
        session.view.selections = SelectionSet::caret(Position::new(2, 0));
        let frame = render(&session, 20, 6);
        assert!(
            frame.rows[0].plain().starts_with("  2 "),
            "{:?}",
            frame.rows[0].plain()
        );
        assert!(
            frame.rows[2].plain().starts_with("  3 "),
            "the cursor line is absolute"
        );
        assert!(frame.rows[3].plain().starts_with("  1 "));
    }

    #[test]
    fn text_appears_after_the_gutter() {
        let frame = render(&session("hello"), 40, 3);
        assert!(frame.rows[0].plain().starts_with("  1 hello"));
    }

    #[test]
    fn rows_past_the_end_of_the_document_are_blank() {
        let frame = render(&session("only"), 20, 5);
        assert_eq!(frame.rows[3].plain().trim(), "");
    }

    #[test]
    fn the_cursor_sits_after_the_gutter() {
        let mut session = session("hello");
        session.view.selections = SelectionSet::caret(Position::new(0, 2));
        let frame = render(&session, 40, 3);
        assert_eq!(frame.cursor, Some((gutter_width(&session) as u16 + 2, 0)));
    }

    #[test]
    fn the_cursor_accounts_for_tab_expansion() {
        let mut session = session("\tx");
        session.view.selections = SelectionSet::caret(Position::new(0, 1));
        let frame = render(&session, 40, 3);
        // One tab renders as four columns at the default tab size.
        assert_eq!(frame.cursor, Some((gutter_width(&session) as u16 + 4, 0)));
    }

    #[test]
    fn tabs_are_expanded_in_the_rendered_text() {
        let frame = render(&session("\tx"), 40, 3);
        let gutter = gutter_width(&session("\tx"));
        let text: String = frame.rows[0].plain().chars().skip(gutter).collect();
        assert!(text.starts_with("    x"), "got {text:?}");
    }

    #[test]
    fn the_cursor_follows_the_scroll_position() {
        let mut session = session(&"x\n".repeat(50));
        session.view.scroll_top = 20;
        session.view.selections = SelectionSet::caret(Position::new(25, 0));
        let frame = render(&session, 40, 10);
        assert_eq!(frame.cursor.map(|(_, y)| y), Some(5));
    }

    #[test]
    fn a_selection_is_drawn_with_a_different_background() {
        let mut session = session("hello world");
        session.view.selections =
            SelectionSet::single(Selection::new(Position::new(0, 0), Position::new(0, 5)));
        let frame = render(&session, 40, 3);

        let backgrounds: Vec<Rgba> = frame.rows[0]
            .spans
            .iter()
            .flat_map(|s| std::iter::repeat_n(s.bg, s.text.chars().count()))
            .collect();
        let gutter = gutter_width(&session);
        // The five selected characters differ from the sixth.
        assert_eq!(backgrounds[gutter], backgrounds[gutter + 4]);
        assert_ne!(backgrounds[gutter], backgrounds[gutter + 5]);
    }

    #[test]
    fn a_multi_line_selection_covers_the_whole_middle_line() {
        let mut session = session("aaa\nbbb\nccc");
        session.view.selections =
            SelectionSet::single(Selection::new(Position::new(0, 1), Position::new(2, 1)));
        let frame = render(&session, 20, 5);
        let gutter = gutter_width(&session);

        let middle = &frame.rows[1];
        let backgrounds: Vec<Rgba> = middle
            .spans
            .iter()
            .flat_map(|s| std::iter::repeat_n(s.bg, s.text.chars().count()))
            .collect();
        // All three characters of "bbb" are selected.
        assert_eq!(backgrounds[gutter], backgrounds[gutter + 2]);
    }

    #[test]
    fn the_status_bar_shows_the_file_and_position() {
        let mut session = session("hello");
        session.view.selections = SelectionSet::caret(Position::new(0, 3));
        let frame = render(&session, 60, 3);
        let status = frame.rows.last().unwrap().plain();
        assert!(status.contains("file.rs"), "{status}");
        assert!(status.contains("Ln 1, Col 4"), "{status}");
        assert!(status.contains("rust"), "{status}");
    }

    #[test]
    fn the_status_bar_marks_unsaved_changes() {
        let mut session = session("hello");
        session.run("type", Some(&serde_json::json!({"text": "x"})), 0);
        let frame = render(&session, 60, 3);
        assert!(frame.rows.last().unwrap().plain().contains("file.rs*"));
    }

    #[test]
    fn the_status_bar_announces_a_pending_chord() {
        let mut session = session("hello");
        session.handle_chord(deco_keymap::keys::Chord::parse("ctrl+k").unwrap(), 0);
        let status = render(&session, 80, 3).rows.last().unwrap().plain();
        assert!(status.contains("ctrl+k"), "{status}");
    }

    #[test]
    fn a_narrow_terminal_truncates_rather_than_overflowing() {
        let frame = render(&session("a long line of text here"), 12, 3);
        for row in &frame.rows {
            assert_eq!(row.plain().chars().count(), 12);
        }
    }

    #[test]
    fn a_one_row_terminal_still_renders_the_status_bar() {
        let frame = render(&session("x"), 20, 1);
        assert_eq!(frame.rows.len(), 1);
        assert_eq!(frame.cursor, None);
    }

    /// Terminal columns a string occupies, counting CJK as two.
    fn columns(text: &str) -> usize {
        text.chars().map(|c| c.width().unwrap_or(1).max(1)).sum()
    }

    #[test]
    fn wide_characters_do_not_overflow_the_row() {
        // Ten CJK characters are twenty columns; padding by character count
        // would push this row well past the right edge.
        let frame = render(&session("漢字漢字漢字漢字漢字"), 20, 3);
        for row in &frame.rows {
            assert_eq!(columns(&row.plain()), 20, "row was {:?}", row.plain());
        }
    }

    #[test]
    fn a_wide_character_straddling_the_edge_is_dropped_not_halved() {
        // The text area is an odd number of columns wide, so the last CJK
        // character cannot fit; the row is padded with a space instead.
        let session = session("漢漢漢");
        let gutter = gutter_width(&session);
        let frame = render(&session, gutter + 5, 3);
        assert_eq!(columns(&frame.rows[0].plain()), gutter + 5);
    }
}
