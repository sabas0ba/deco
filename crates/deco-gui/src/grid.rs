//! Turning the terminal frontend's frame into positioned text and rectangles.
//!
//! The GPU frontend draws the same [`Frame`] the terminal frontend paints:
//! [`deco_tui::Driver::frame`] lays out the tabs, the text, the side bar, the
//! panel, the prompts and the status bar as rows of coloured cells. This
//! module places those cells in pixels, so the two frontends show the same
//! editor and one renderer decides what is on screen.
//!
//! It is a pure function of the frame and the font measurements, so what is
//! drawn where is tested without a graphics device. [`crate::app`] only
//! uploads the result.
//!
//! The grid assumes a monospace font: a column is [`Metrics::cell_width`]
//! pixels. A character two columns wide (CJK, most emoji) is placed on its own
//! at its column, because a monospace font's wide glyphs are rarely exactly two
//! cells, and text after it would otherwise drift from the column the
//! renderer gave it.

use deco_config::CursorStyle;
use deco_editor::Session;
use deco_theme::Rgba;
use deco_tui::Frame;
use unicode_width::UnicodeWidthChar;

/// Font and spacing measurements for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// Font size in pixels.
    pub font_size: f32,
    /// Row height in pixels.
    pub line_height: f32,
    /// Advance width of one column.
    pub cell_width: f32,
}

impl Metrics {
    /// Derives metrics from the document's settings at `scale`.
    ///
    /// The cell width is estimated as 0.6 times the font size, which
    /// approximates a monospace face. [`crate::app`] replaces it with the
    /// loaded font's advance through [`Metrics::with_cell_width`], because a
    /// glyph whose advance differs from the cell drifts away from its column.
    pub fn from_session(session: &Session, scale: f32) -> Self {
        let settings = &session.document.settings;
        let font_size = settings.font_size * scale;
        Self {
            font_size,
            line_height: (settings.effective_line_height() * scale).max(1.0),
            cell_width: (font_size * 0.6).max(1.0),
        }
    }

    /// The same metrics with a measured cell width. A width that is not a
    /// positive finite number is ignored, so a font that could not be
    /// measured keeps the estimate.
    pub fn with_cell_width(self, width: f32) -> Self {
        if width.is_finite() && width > 0.0 {
            Self {
                cell_width: width,
                ..self
            }
        } else {
            self
        }
    }

    /// How many columns and rows fit in a window of `width` by `height`
    /// pixels: the size the renderer is asked for. At least one of each, so a
    /// window shrunk to nothing still has a frame.
    pub fn cells(self, width: f32, height: f32) -> (u16, u16) {
        let count =
            |pixels: f32, cell: f32| (pixels / cell).floor().clamp(1.0, f32::from(u16::MAX));
        (
            count(width, self.cell_width) as u16,
            count(height, self.line_height) as u16,
        )
    }

    /// The cell under the pixel at `x`, `y`, or `None` outside the grid.
    pub fn cell_at(self, x: f32, y: f32, (cols, rows): (u16, u16)) -> Option<(u16, u16)> {
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let col = (x / self.cell_width).floor();
        let row = (y / self.line_height).floor();
        (col < f32::from(cols) && row < f32::from(rows)).then_some((col as u16, row as u16))
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

/// A rectangle filled with one colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    /// Where.
    pub rect: Rect,
    /// What colour.
    pub color: Rgba,
}

/// Text that starts at one pixel position and has one style.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Left edge of its first column.
    pub x: f32,
    /// Top of its row.
    pub y: f32,
    /// The characters, each one column wide unless the run is a single
    /// wide character.
    pub text: String,
    /// Text colour.
    pub fg: Rgba,
    /// Drawn in italics.
    pub italic: bool,
}

/// Everything one frame draws.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    /// The colour the window is cleared to: every cell's background that is
    /// not drawn as a rectangle.
    pub background: Rgba,
    /// Backgrounds that differ from [`Grid::background`], then the caret, back
    /// to front.
    pub quads: Vec<Quad>,
    /// The text, over the rectangles.
    pub runs: Vec<Run>,
}

/// The caret the terminal frontend leaves to the terminal: where the frame's
/// cursor is, in `style` and `color`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caret {
    /// Its shape.
    pub style: CursorStyle,
    /// Its colour.
    pub color: Rgba,
}

/// Places `frame` in pixels.
///
/// `background` is the colour most cells have; only the others become
/// rectangles. Runs of spaces are not drawn as text.
pub fn grid(frame: &Frame, metrics: Metrics, background: Rgba, caret: Option<Caret>) -> Grid {
    let mut quads = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    for (row_index, row) in frame.rows.iter().enumerate() {
        let y = row_index as f32 * metrics.line_height;
        let mut col = 0usize;
        for span in &row.spans {
            let start = col;
            let mut run: Option<Run> = None;
            let flush = |run: &mut Option<Run>, runs: &mut Vec<Run>| {
                if let Some(done) = run.take() {
                    if !done.text.trim().is_empty() {
                        runs.push(done);
                    }
                }
            };
            for c in span.text.chars() {
                let width = c.width().unwrap_or(1);
                match width {
                    // A combining mark joins the character before it.
                    0 => match run.as_mut() {
                        Some(run) => run.text.push(c),
                        None => runs
                            .last_mut()
                            .filter(|last| last.y == y)
                            .map_or((), |last| last.text.push(c)),
                    },
                    1 => {
                        let run = run.get_or_insert_with(|| Run {
                            x: col as f32 * metrics.cell_width,
                            y,
                            text: String::new(),
                            fg: span.fg,
                            italic: span.italic,
                        });
                        run.text.push(c);
                        col += 1;
                    }
                    _ => {
                        flush(&mut run, &mut runs);
                        runs.push(Run {
                            x: col as f32 * metrics.cell_width,
                            y,
                            text: c.to_string(),
                            fg: span.fg,
                            italic: span.italic,
                        });
                        col += width;
                    }
                }
            }
            flush(&mut run, &mut runs);
            if span.bg != background && col > start {
                quads.push(Quad {
                    rect: Rect {
                        x: start as f32 * metrics.cell_width,
                        y,
                        width: (col - start) as f32 * metrics.cell_width,
                        height: metrics.line_height,
                    },
                    color: span.bg,
                });
            }
        }
    }
    if let (Some(caret), Some((col, row))) = (caret, frame.cursor) {
        let x = f32::from(col) * metrics.cell_width;
        let y = f32::from(row) * metrics.line_height;
        quads.extend(
            caret_rects(caret.style, x, y, metrics)
                .into_iter()
                .map(|rect| Quad {
                    rect,
                    color: caret.color,
                }),
        );
    }
    Grid {
        background,
        quads,
        runs,
    }
}

/// The rectangles of a caret in `style` in the cell whose top-left corner is
/// `x`, `y`. An outline is its four edges. Every part is at least one pixel,
/// so the caret is visible at any scale.
pub fn caret_rects(style: CursorStyle, x: f32, y: f32, metrics: Metrics) -> Vec<Rect> {
    let cell = metrics.cell_width;
    let height = metrics.line_height;
    let bar = |ratio: f32| Rect {
        x,
        y,
        width: (cell * ratio).max(1.0),
        height,
    };
    let underline = |ratio: f32| {
        let thickness = (height * ratio).max(1.0);
        Rect {
            x,
            y: y + height - thickness,
            width: cell,
            height: thickness,
        }
    };
    match style {
        CursorStyle::Line => vec![bar(0.12)],
        CursorStyle::LineThin => vec![bar(0.06)],
        CursorStyle::Block => vec![Rect {
            x,
            y,
            width: cell,
            height,
        }],
        CursorStyle::BlockOutline => {
            let edge = (cell * 0.08).max(1.0);
            vec![
                Rect {
                    x,
                    y,
                    width: cell,
                    height: edge,
                },
                Rect {
                    x,
                    y: y + height - edge,
                    width: cell,
                    height: edge,
                },
                Rect {
                    x,
                    y,
                    width: edge,
                    height,
                },
                Rect {
                    x: x + cell - edge,
                    y,
                    width: edge,
                    height,
                },
            ]
        }
        CursorStyle::Underline => vec![underline(0.1)],
        CursorStyle::UnderlineThin => vec![underline(0.05)],
    }
}

/// The caret for `session`'s frame: the editor's style while text has the
/// keyboard, and a block in the terminal, as a terminal draws its own cursor.
pub fn caret_for(session: &Session) -> Caret {
    let theme = &session.theme;
    let foreground = theme.color("editor.foreground").unwrap_or(Rgba::WHITE);
    let in_terminal = session.focus() == deco_editor::Focus::Panel
        && session.panel_view() == deco_editor::PanelView::Terminal;
    if in_terminal {
        return Caret {
            style: CursorStyle::Block,
            color: theme
                .color("terminalCursor.foreground")
                .or_else(|| theme.color("terminal.foreground"))
                .unwrap_or(foreground),
        };
    }
    Caret {
        style: session.document.settings.cursor_style,
        color: theme.color("editorCursor.foreground").unwrap_or(foreground),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_tui::{Row, Span};

    const BG: Rgba = Rgba::new(0, 0, 0, 255);
    const FG: Rgba = Rgba::new(200, 200, 200, 255);
    const BLUE: Rgba = Rgba::new(0, 0, 200, 255);

    fn metrics() -> Metrics {
        Metrics {
            font_size: 10.0,
            line_height: 20.0,
            cell_width: 10.0,
        }
    }

    fn span(text: &str, bg: Rgba) -> Span {
        Span {
            text: text.to_owned(),
            fg: FG,
            bg,
            italic: false,
        }
    }

    fn frame(rows: Vec<Vec<Span>>) -> Frame {
        Frame {
            rows: rows.into_iter().map(|spans| Row { spans }).collect(),
            cursor: None,
        }
    }

    #[test]
    fn text_is_placed_by_column_and_row_and_spaces_are_not_drawn() {
        let grid = grid(
            &frame(vec![vec![span("ab", BG)], vec![span("  c", BG)]]),
            metrics(),
            BG,
            None,
        );
        let placed: Vec<(f32, f32, &str)> = grid
            .runs
            .iter()
            .map(|run| (run.x, run.y, run.text.as_str()))
            .collect();
        assert_eq!(placed, [(0.0, 0.0, "ab"), (0.0, 20.0, "  c")]);
        assert!(grid.quads.is_empty(), "every cell has the window's colour");
    }

    #[test]
    fn a_background_other_than_the_window_s_is_a_rectangle_over_its_columns() {
        let grid = grid(
            &frame(vec![vec![span("ab", BG), span("cde", BLUE)]]),
            metrics(),
            BG,
            None,
        );
        assert_eq!(
            grid.quads,
            [Quad {
                rect: Rect {
                    x: 20.0,
                    y: 0.0,
                    width: 30.0,
                    height: 20.0
                },
                color: BLUE
            }]
        );
    }

    #[test]
    fn a_wide_character_is_placed_alone_and_moves_the_rest_two_columns() {
        let grid = grid(&frame(vec![vec![span("a日b", BG)]]), metrics(), BG, None);
        let placed: Vec<(f32, &str)> = grid
            .runs
            .iter()
            .map(|run| (run.x, run.text.as_str()))
            .collect();
        assert_eq!(placed, [(0.0, "a"), (10.0, "日"), (30.0, "b")]);
    }

    #[test]
    fn a_combining_mark_stays_with_its_character() {
        let grid = grid(
            &frame(vec![vec![span("e\u{301}x", BG)]]),
            metrics(),
            BG,
            None,
        );
        assert_eq!(grid.runs[0].text, "e\u{301}x");
    }

    #[test]
    fn the_caret_is_drawn_at_the_frame_s_cursor_in_its_style() {
        let mut frame = frame(vec![vec![span("ab", BG)]]);
        frame.cursor = Some((1, 0));
        let caret = Caret {
            style: CursorStyle::Line,
            color: BLUE,
        };
        let grid = grid(&frame, metrics(), BG, Some(caret));
        let last = grid.quads.last().expect("a caret");
        assert_eq!((last.rect.x, last.rect.y, last.color), (10.0, 0.0, BLUE));
        assert!(last.rect.width < 10.0, "a bar, not a block");
    }

    #[test]
    fn the_grid_size_and_the_cell_under_a_pixel_agree() {
        let metrics = metrics();
        let size = metrics.cells(105.0, 41.0);
        assert_eq!(size, (10, 2));
        assert_eq!(metrics.cell_at(15.0, 25.0, size), Some((1, 1)));
        assert_eq!(
            metrics.cell_at(101.0, 0.0, size),
            None,
            "past the last column"
        );
        assert_eq!(metrics.cells(0.0, 0.0), (1, 1));
    }
}

#[cfg(test)]
mod shared {
    //! The window draws what the terminal frontend paints, chrome included.

    use super::*;

    #[test]
    fn the_window_shows_the_chrome_the_terminal_frontend_shows() {
        let mut session = Session::with_defaults();
        session.open(std::path::PathBuf::from("/w/notes.txt"), "hello\n");
        let mut driver = deco_tui::Driver::start(
            &mut session,
            deco_tui::Options {
                extension_roots: Vec::new(),
                permissions_file: None,
                size: (60, 12),
                ..deco_tui::Options::default()
            },
        );
        // The command palette, which the window could not show before.
        driver
            .key(
                &mut session,
                deco_keymap::keys::Chord::parse("ctrl+shift+p").unwrap(),
                0,
            )
            .unwrap();
        let frame = driver.frame(&mut session);
        let background = session.theme.color("editor.background").unwrap();
        let grid = grid(
            &frame,
            Metrics::from_session(&session, 1.0),
            background,
            None,
        );
        let text: String = grid.runs.iter().map(|run| run.text.as_str()).collect();
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("Command:"), "the palette: {text}");
        assert!(text.contains("notes.txt"), "the tab: {text}");
        driver.shutdown();
    }
}
