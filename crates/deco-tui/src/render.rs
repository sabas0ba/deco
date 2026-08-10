//! Turning a session into a grid of styled cells.
//!
//! Rendering is a pure function of the session plus the terminal size, and
//! produces a [`Frame`] rather than writing to the terminal. That split is what
//! lets the layout — gutter width, selection highlighting, tab expansion,
//! status bar — be asserted in CI with no terminal attached.

use deco_config::LineNumbers;
use deco_core::movement::display_column;
use deco_core::position::Range;
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
    find_match_bg: Rgba,
    find_highlight_bg: Rgba,
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
            // Composited for the same reason as the selection: both are
            // translucent in every theme that ships with VS Code.
            find_match_bg: theme
                .color("editor.findMatchBackground")
                .map(|c| c.over(bg))
                .unwrap_or(fg),
            find_highlight_bg: theme
                .color("editor.findMatchHighlightBackground")
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
    render_with_overlays(session, width, height, None, None)
}

/// Renders, optionally overlaying a hover box near the cursor.
///
/// A separate entry point rather than a field on `Session`, because a hover is
/// the frontend's business: it belongs to a screen with a cursor on it, and the
/// core has neither.
pub fn render_with_hover(
    session: &Session,
    width: usize,
    height: usize,
    hover: Option<&deco_lsp::Hover>,
) -> Frame {
    render_with_overlays(session, width, height, hover, None)
}

/// Renders with both overlays.
///
/// Only one is ever drawn: a completion list and a hover box would occupy the
/// same space beside the cursor, and the list is the one the user is interacting
/// with. `Suggest` therefore wins.
pub fn render_with_overlays(
    session: &Session,
    width: usize,
    height: usize,
    hover: Option<&deco_lsp::Hover>,
    suggest: Option<&crate::suggest::Suggest>,
) -> Frame {
    let mut frame = render_text(session, width, height);
    match suggest {
        Some(suggest) => overlay_suggest(&mut frame, session, width, height, suggest),
        None => {
            if let Some(hover) = hover {
                overlay_hover(&mut frame, session, width, height, hover);
            }
        }
    }
    frame
}

/// How many rows the chrome below the text takes: the status bar, plus the find
/// bar when it is open.
///
/// The frontend needs this to tell the session how tall the text area is, so
/// exported rather than folded into the renderer.
pub fn chrome_height(session: &Session) -> usize {
    1 + usize::from(session.find.visible())
}

fn render_text(session: &Session, width: usize, height: usize) -> Frame {
    let palette = Palette::from(session);
    let gutter = gutter_width(session);
    let text_height = height.saturating_sub(chrome_height(session));
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

    // Between the text and the status bar, so that the bar the user is typing
    // into sits next to the text it is searching and never covers the place the
    // editor reports errors.
    if session.find.visible() {
        let (row, caret) = find_bar(session, width, &palette);
        rows.push(row);
        // The caret belongs in the query while the bar has the keyboard: the
        // document's cursor is on the current match, which is highlighted, and
        // two visible carets would be a lie about where typing goes.
        cursor_cell = Some((caret as u16, rows.len() as u16 - 1));
    }

    rows.push(status_bar(session, width, &palette));
    Frame {
        rows,
        cursor: cursor_cell,
    }
}

/// The find bar, and the column its caret sits in.
///
/// One line, because that is what fits: the query, the two toggles as the
/// letters VS Code puts on its buttons, and the match count.
fn find_bar(session: &Session, width: usize, palette: &Palette) -> (Row, usize) {
    const PROMPT: &str = " Find: ";

    let find = &session.find;
    let options = find.options();
    let toggles = format!(
        "[{}a {}w] ",
        if options.case_sensitive { 'A' } else { 'a' },
        if options.whole_word { 'W' } else { 'w' },
    );
    // `Aa`/`ab` on VS Code's buttons; here the capital says the option is on.
    // Spelled out in the status bar the first time either is toggled, so the
    // letters do not have to be guessed at.
    let count = if find.query().is_empty() {
        String::new()
    } else if find.matches().is_empty() {
        "No results ".to_owned()
    } else {
        let primary = session.view.selections.primary();
        match find.ordinal(Range::new(primary.start(), primary.end())) {
            Some(ordinal) => format!("{ordinal} of {} ", find.matches().len()),
            // The cursor was moved off the match, so claiming a position in the
            // list would be wrong. The total is still true.
            None => format!("{} results ", find.matches().len()),
        }
    };

    // The query is the last thing to go, because it is what the user is typing:
    // a search term you cannot see is a search term you cannot correct. On a
    // terminal too narrow for all three the count is dropped first and the
    // toggles second, both recoverable by widening the window.
    let mut right = format!("{count}{toggles}");
    let fits = |right: &str| width.saturating_sub(columns(PROMPT) + columns(right)) >= MIN_QUERY;
    if !fits(&right) {
        right = toggles;
    }
    if !fits(&right) {
        right = String::new();
    }

    let room = width
        .saturating_sub(columns(PROMPT))
        .saturating_sub(columns(&right));
    let query = visible_query(find.query(), find.caret(), room);

    let used = columns(PROMPT) + columns(&query.text) + columns(&right);
    let mut text = format!(
        "{PROMPT}{}{}{right}",
        query.text,
        " ".repeat(width.saturating_sub(used))
    );
    text = truncate_to(&text, width);
    while columns(&text) < width {
        text.push(' ');
    }

    let caret = (columns(PROMPT) + query.caret_column).min(width.saturating_sub(1));
    (
        Row {
            spans: vec![Span {
                text,
                fg: palette.status_fg,
                bg: palette.status_bg,
            }],
        },
        caret,
    )
}

/// The fewest columns the query is given before the readouts beside it are
/// dropped to make room.
///
/// Eight is enough to see a short search term whole and enough of a long one to
/// recognise it.
const MIN_QUERY: usize = 8;

/// The window of a query that fits in `room` columns, and where its caret is.
struct VisibleQuery {
    text: String,
    caret_column: usize,
}

/// Scrolls a long query so the caret stays on screen.
///
/// A query wider than the bar has to be windowed rather than truncated: an input
/// that stops showing what is being typed is worse than one that scrolls.
fn visible_query(query: &str, caret: usize, room: usize) -> VisibleQuery {
    if room == 0 {
        return VisibleQuery {
            text: String::new(),
            caret_column: 0,
        };
    }
    let chars: Vec<char> = query.chars().collect();
    // One column reserved so the caret has somewhere to sit at the end of the
    // text rather than on top of the last character.
    let visible = room.saturating_sub(1).max(1);
    let start = caret.saturating_sub(visible);
    let end = (start + visible).min(chars.len());
    VisibleQuery {
        text: chars[start..end].iter().collect(),
        caret_column: caret - start,
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
    let mut cells: Vec<(char, Cell)> = Vec::new();
    let mut column = 0usize;
    let mut utf16 = 0u32;

    let selected_ranges = clipped_to_line(
        session
            .view
            .selections
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| Range::new(s.start(), s.end())),
        line,
    );
    // Empty whenever the find bar is closed, which is what makes this free for
    // everyone not searching.
    let match_ranges = clipped_to_line(session.find.matches().iter().copied(), line);

    let covers = |ranges: &[(u32, u32)], utf16: u32| {
        ranges
            .iter()
            .any(|(from, to)| utf16 >= *from && utf16 < *to)
    };
    let cell_at = |utf16: u32| match (
        covers(&selected_ranges, utf16),
        covers(&match_ranges, utf16),
    ) {
        // The current match is both: `Session` selects the match it moves to, so
        // this is where VS Code's distinct current-match colour comes from.
        (true, true) => Cell::CurrentMatch,
        (true, false) => Cell::Selected,
        (false, true) => Cell::OtherMatch,
        (false, false) => Cell::Plain,
    };

    for c in text.chars() {
        let cell = cell_at(utf16);
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
                cells.push((' ', cell));
            }
        } else {
            cells.push((c, cell));
        }
        column += advance;
        utf16 += c.len_utf16() as u32;
    }

    // A selection that runs past the end of the line is drawn one cell wide, so
    // that selecting a line break is visible rather than invisible.
    let trailing = cell_at(utf16);
    if trailing != Cell::Plain && column < width {
        cells.push((' ', trailing));
        column += 1;
    }
    while column < width {
        cells.push((' ', Cell::Plain));
        column += 1;
    }

    // Coalesce runs sharing a style; one span per character would be correct
    // but would make the terminal writer do far more work than it needs to.
    let mut spans: Vec<Span> = Vec::new();
    for (c, cell) in cells {
        let bg = match cell {
            Cell::Plain => palette.bg,
            Cell::Selected => palette.selection_bg,
            Cell::CurrentMatch => palette.find_match_bg,
            Cell::OtherMatch => palette.find_highlight_bg,
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

/// What one cell of a rendered line is part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    /// Just text.
    Plain,
    /// Inside a selection.
    Selected,
    /// Inside the find match the editor is sitting on.
    CurrentMatch,
    /// Inside one of the other find matches.
    OtherMatch,
}

/// The parts of `ranges` that fall on `line`, as UTF-16 column pairs.
///
/// A range that starts above the line begins at column zero, and one that ends
/// below it runs to `u32::MAX` — which the caller draws as one cell past the end
/// of the text, so that a selected line break is visible.
fn clipped_to_line(ranges: impl Iterator<Item = Range>, line: usize) -> Vec<(u32, u32)> {
    let line = line as u32;
    ranges
        .filter_map(|range| {
            if line < range.start.line || line > range.end.line {
                return None;
            }
            let from = if range.start.line == line {
                range.start.character
            } else {
                0
            };
            let to = if range.end.line == line {
                range.end.character
            } else {
                u32::MAX
            };
            Some((from, to))
        })
        .collect()
}

/// The status bar.
/// The error and warning tallies, or nothing at all when the file is clean.
///
/// `×`/`⚠` rather than VS Code's icon font, and omitted entirely at zero: a
/// permanent `0 errors` is noise, and the absence of the marker is the signal.
fn problem_summary(session: &Session) -> String {
    let counts = session.diagnostic_counts();
    if counts.is_empty() {
        return String::new();
    }
    // Information and hints are folded into neither tally. They are not
    // problems the user has to act on, and a status bar has room for the two
    // that are.
    format!("×{} ⚠{}  ", counts.errors, counts.warnings)
}

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
        " {}{}  Ln {}, Col {} ",
        problem_summary(session),
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

/// Draws a hover box over the text, anchored to the cursor.
///
/// Below the cursor when there is room, above it otherwise — a box that would
/// hang off the bottom of the terminal is worse than one that covers the line
/// above. The status bar is never covered: it is where the editor reports
/// everything else, including why a hover might be wrong.
fn overlay_hover(
    frame: &mut Frame,
    session: &Session,
    width: usize,
    height: usize,
    hover: &deco_lsp::Hover,
) {
    let palette = Palette::from(session);
    // Never over the chrome: the status bar is where the editor reports things,
    // and the find bar is where the user is typing.
    let text_height = height.saturating_sub(chrome_height(session));
    if text_height < 3 || width < 8 {
        // Not enough screen to draw a box that says anything. The status bar
        // still carries the first line, so nothing is lost silently.
        return;
    }

    // Two columns of border plus one of padding on each side.
    let inner_width = width.saturating_sub(4).max(1);
    let lines = wrap(&hover.contents, inner_width, MAX_HOVER_LINES);
    if lines.is_empty() {
        return;
    }

    let box_height = lines.len() + 2;
    let box_width = lines
        .iter()
        .map(|line| line.chars().map(|c| c.width().unwrap_or(1)).sum::<usize>())
        .max()
        .unwrap_or(0)
        .min(inner_width)
        + 4;

    // The cursor's row within the text area, which is what the box is anchored
    // to rather than the buffer line.
    let cursor_row = frame
        .cursor
        .map(|(_, y)| y as usize)
        .unwrap_or(text_height / 2);

    let top = if cursor_row + 1 + box_height <= text_height {
        // Below the cursor, the usual case.
        cursor_row + 1
    } else {
        // Above it, and saturating at zero: a box taller than the space above
        // the cursor fits nowhere relative to it, so it goes at the top, where
        // at least it does not cover the line being edited when the cursor is
        // low on the screen.
        cursor_row.saturating_sub(box_height)
    };

    for (index, row) in (top..(top + box_height).min(text_height)).enumerate() {
        let content = match index {
            0 => border_line(box_width, '\u{250c}', '\u{2510}'),
            i if i == box_height - 1 => border_line(box_width, '\u{2514}', '\u{2518}'),
            i => {
                let text = lines.get(i - 1).map(String::as_str).unwrap_or("");
                let used: usize = text.chars().map(|c| c.width().unwrap_or(1)).sum();
                let pad = box_width.saturating_sub(used + 4);
                format!("\u{2502} {text}{} \u{2502}", " ".repeat(pad))
            }
        };
        frame.rows[row] = Row {
            spans: vec![
                Span {
                    text: content,
                    fg: palette.status_fg,
                    bg: palette.status_bg,
                },
                // The rest of the row keeps the editor background, so the box
                // reads as floating rather than as a full-width banner.
                Span {
                    text: " ".repeat(width.saturating_sub(box_width)),
                    fg: palette.bg,
                    bg: palette.bg,
                },
            ],
        };
    }
}

/// The most lines a hover box will show.
///
/// A long doc comment would otherwise cover the whole file. Ten is enough for a
/// signature and the first paragraph, which is what a hover is for.
const MAX_HOVER_LINES: usize = 10;

fn border_line(box_width: usize, left: char, right: char) -> String {
    let mut line = String::with_capacity(box_width);
    line.push(left);
    for _ in 0..box_width.saturating_sub(2) {
        line.push('\u{2500}');
    }
    line.push(right);
    line
}

/// Breaks text to `width` columns, at whitespace where possible.
///
/// Column-aware rather than character-aware: a CJK identifier in a signature
/// would otherwise push the box's right border past the terminal edge.
fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for paragraph in text.lines() {
        if out.len() >= max_lines {
            break;
        }
        if paragraph.trim().is_empty() {
            // Blank lines separate a signature from its documentation, so they
            // are worth keeping — but not at the very top of the box.
            if !out.is_empty() {
                out.push(String::new());
            }
            continue;
        }

        let mut line = String::new();
        let mut columns = 0usize;
        for word in paragraph.split_whitespace() {
            let word_width: usize = word.chars().map(|c| c.width().unwrap_or(1)).sum();
            let needed = if line.is_empty() {
                word_width
            } else {
                word_width + 1
            };
            if columns + needed > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                columns = 0;
                if out.len() >= max_lines {
                    return trimmed(out, max_lines);
                }
            }
            if !line.is_empty() {
                line.push(' ');
                columns += 1;
            }
            // A single word longer than the box is cut rather than allowed to
            // overflow; the alternative is a broken border.
            if word_width > width {
                let mut used = 0;
                for c in word.chars() {
                    let w = c.width().unwrap_or(1);
                    if used + w > width {
                        break;
                    }
                    line.push(c);
                    used += w;
                }
                columns += used;
            } else {
                line.push_str(word);
                columns += word_width;
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }

    trimmed(out, max_lines)
}

fn trimmed(mut lines: Vec<String>, max_lines: usize) -> Vec<String> {
    lines.truncate(max_lines);
    // A trailing blank line inside a box is just a gap above the border.
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Draws the completion list over the text, anchored to the cursor.
///
/// Shares the placement rule with the hover box — below the cursor, above when
/// it will not fit, never over the status bar — so the two feel like the same
/// widget in different clothes. The selected row is inverted rather than marked
/// with a character, because a marker column costs width the labels need.
fn overlay_suggest(
    frame: &mut Frame,
    session: &Session,
    width: usize,
    height: usize,
    suggest: &crate::suggest::Suggest,
) {
    let palette = Palette::from(session);
    // Never over the chrome: the status bar is where the editor reports things,
    // and the find bar is where the user is typing.
    let text_height = height.saturating_sub(chrome_height(session));
    let rows = suggest.rows();
    if rows.is_empty() || text_height < 2 || width < 10 {
        return;
    }

    // One column of padding either side. No border: the list is taller than a
    // hover box and a frame around it would cost two more rows of the file.
    let inner = width.saturating_sub(2).max(1);
    let entries: Vec<String> = rows
        .iter()
        .map(|(marker, label, detail)| -> String {
            let head = format!("{marker} {label}");
            match detail {
                Some(detail) => {
                    // The detail is trimmed first, so a long signature loses its
                    // tail rather than pushing the label off the row: the label
                    // is what is being chosen between.
                    let room = inner.saturating_sub(columns(&head) + 2);
                    if room >= 4 {
                        format!("{head}  {}", truncate_to(detail, room))
                    } else {
                        head
                    }
                }
                None => head,
            }
        })
        // Trimming the detail is not enough on its own: a label longer than the
        // terminal still overflows, and an unbounded row breaks every row after
        // it because the padding is computed from a saturating subtraction.
        .map(|entry| truncate_to(&entry, inner))
        .collect();

    let box_width = entries
        .iter()
        .map(|entry| columns(entry))
        .max()
        .unwrap_or(0)
        + 2;
    let box_height = entries.len();

    let cursor_row = frame
        .cursor
        .map(|(_, y)| y as usize)
        .unwrap_or(text_height / 2);
    let top = if cursor_row + 1 + box_height <= text_height {
        cursor_row + 1
    } else {
        cursor_row.saturating_sub(box_height)
    };

    for (index, row) in (top..(top + box_height).min(text_height)).enumerate() {
        let entry = &entries[index];
        let pad = box_width.saturating_sub(columns(entry) + 2);
        let text = format!(" {entry}{} ", " ".repeat(pad));
        // Inverted for the selection, which reads as "this one" without a
        // character of its own.
        let (fg, bg) = if index == suggest.selected_row() {
            (palette.status_bg, palette.status_fg)
        } else {
            (palette.status_fg, palette.status_bg)
        };
        frame.rows[row] = Row {
            spans: vec![
                Span { text, fg, bg },
                Span {
                    text: " ".repeat(width.saturating_sub(box_width)),
                    fg: palette.bg,
                    bg: palette.bg,
                },
            ],
        };
    }
}

/// A string's width in terminal columns.
fn columns(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(1)).sum()
}

/// Cuts `text` to at most `width` columns, on a character boundary.
fn truncate_to(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(1);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out
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

    /// The status bar's text, joined across spans.
    fn status_text(session: &Session) -> String {
        let frame = render(session, 80, 10);
        frame
            .rows
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.text.as_str())
            .collect()
    }

    fn problem(line: u32, severity: deco_lsp::Severity) -> deco_lsp::Diagnostic {
        deco_lsp::Diagnostic {
            range: deco_core::position::Range::new(
                deco_core::Position::new(line, 0),
                deco_core::Position::new(line, 3),
            ),
            severity,
            code: None,
            source: None,
            message: "boom".into(),
        }
    }

    #[test]
    fn a_clean_file_shows_no_problem_counters() {
        // A permanent `0 errors` is noise; the absence of the marker is the
        // signal.
        let session = session("fn main() {}\n");
        let text = status_text(&session);
        assert!(
            !text.contains('\u{d7}'),
            "unexpected error marker: {text:?}"
        );
        assert!(
            !text.contains('\u{26a0}'),
            "unexpected warning marker: {text:?}"
        );
    }

    #[test]
    fn the_status_bar_tallies_errors_and_warnings() {
        let mut session = session("fn main() {}\n");
        session.set_diagnostics(vec![
            problem(0, deco_lsp::Severity::Error),
            problem(0, deco_lsp::Severity::Error),
            problem(0, deco_lsp::Severity::Warning),
        ]);
        let text = status_text(&session);
        assert!(text.contains("\u{d7}2"), "expected two errors in {text:?}");
        assert!(
            text.contains("\u{26a0}1"),
            "expected one warning in {text:?}"
        );
    }

    #[test]
    fn hints_do_not_appear_in_the_tally() {
        // They are not problems the user has to act on, and the bar has room
        // for the two that are.
        let mut session = session("fn main() {}\n");
        session.set_diagnostics(vec![problem(0, deco_lsp::Severity::Hint)]);
        let text = status_text(&session);
        assert!(text.contains("\u{d7}0 \u{26a0}0"), "{text:?}");
    }

    #[test]
    fn the_status_bar_still_fills_the_width_with_problems_shown() {
        // The counters lengthen the right-hand side; the row must still be
        // exactly as wide as the terminal or the previous frame shows through.
        let mut session = session("fn main() {}\n");
        session.set_diagnostics(vec![problem(0, deco_lsp::Severity::Error)]);
        for width in [20, 40, 80] {
            let frame = render(&session, width, 10);
            let row: String = frame
                .rows
                .last()
                .unwrap()
                .spans
                .iter()
                .map(|s| s.text.as_str())
                .collect();
            assert_eq!(row.chars().count(), width, "width {width}: {row:?}");
        }
    }

    fn hover(contents: &str) -> deco_lsp::Hover {
        deco_lsp::Hover {
            contents: contents.to_owned(),
            range: None,
        }
    }

    /// Every row's text, for asserting on the overlay's placement.
    fn rows_of(frame: &Frame) -> Vec<String> {
        frame.rows.iter().map(Row::plain).collect()
    }

    #[test]
    fn a_hover_box_is_drawn_below_the_cursor_when_there_is_room() {
        let session = session("fn main() {}\n");
        let frame = render_with_hover(&session, 40, 10, Some(&hover("the entry point")));
        let rows = rows_of(&frame);

        // The cursor is on row 0, so the box starts on row 1.
        assert!(rows[1].starts_with('\u{250c}'), "{:?}", rows[1]);
        assert!(rows[2].contains("the entry point"), "{:?}", rows[2]);
        assert!(rows[3].starts_with('\u{2514}'), "{:?}", rows[3]);
    }

    #[test]
    fn a_hover_box_goes_above_the_cursor_when_it_would_not_fit_below() {
        // A box hanging off the bottom of the terminal is worse than one
        // covering the line above.
        let mut session = session(&"line\n".repeat(20));
        session.resize(40, 9);
        session.view.selections = deco_core::SelectionSet::caret(deco_core::Position::new(8, 0));
        session
            .view
            .reveal_cursor(&session.document.buffer, &session.document.settings);

        let frame = render_with_hover(&session, 40, 10, Some(&hover("above")));
        let rows = rows_of(&frame);
        let cursor_row = frame.cursor.expect("the cursor is on screen").1 as usize;

        let top = rows
            .iter()
            .position(|row| row.starts_with('\u{250c}'))
            .expect("a box was drawn");
        assert!(top < cursor_row, "box at {top}, cursor at {cursor_row}");
    }

    #[test]
    fn the_status_bar_is_never_covered_by_a_hover() {
        // It is where the editor reports everything else, including why a hover
        // might be wrong.
        let session = session("fn main() {}\n");
        let frame = render_with_hover(&session, 40, 6, Some(&hover(&"x\n".repeat(20))));
        let last = frame.rows.last().unwrap().plain();
        assert!(
            last.contains("Ln 1"),
            "the status bar was overwritten: {last:?}"
        );
    }

    #[test]
    fn a_hover_box_never_exceeds_the_terminal_width() {
        for width in [12, 20, 40, 80] {
            let session = session("fn main() {}\n");
            let frame = render_with_hover(
                &session,
                width,
                10,
                Some(&hover(
                    "a very long single line of documentation that will not fit",
                )),
            );
            for (index, row) in frame.rows.iter().enumerate() {
                let columns: usize = row.plain().chars().map(|c| c.width().unwrap_or(1)).sum();
                assert_eq!(columns, width, "row {index} at width {width}");
            }
        }
    }

    #[test]
    fn a_wide_character_does_not_push_the_border_off_the_edge() {
        // Wrapping by character count rather than by column would break the
        // right border on any CJK identifier.
        let session = session("fn main() {}\n");
        let frame = render_with_hover(
            &session,
            24,
            10,
            Some(&hover("日本語の説明がここに入ります")),
        );
        for row in &frame.rows {
            let columns: usize = row.plain().chars().map(|c| c.width().unwrap_or(1)).sum();
            assert_eq!(columns, 24, "{:?}", row.plain());
        }
    }

    #[test]
    fn a_long_hover_is_truncated_rather_than_covering_the_file() {
        let session = session(&"line\n".repeat(40));
        let frame = render_with_hover(&session, 40, 30, Some(&hover(&"detail\n".repeat(50))));
        let box_rows = rows_of(&frame)
            .iter()
            .filter(|row| row.starts_with('\u{2502}'))
            .count();
        assert!(box_rows <= MAX_HOVER_LINES, "{box_rows} content rows");
    }

    #[test]
    fn a_terminal_too_small_for_a_box_draws_none_rather_than_a_broken_one() {
        let session = session("fn main() {}\n");
        for (width, height) in [(40, 2), (6, 10)] {
            let frame = render_with_hover(&session, width, height, Some(&hover("x")));
            let plain = rows_of(&frame).join("");
            assert!(
                !plain.contains('\u{250c}'),
                "a box was drawn at {width}x{height}: {plain:?}"
            );
        }
    }

    #[test]
    fn rendering_without_a_hover_is_unchanged() {
        // `render` is the same function with no overlay, so every existing
        // assertion about layout still holds.
        let session = session("fn main() {}\n");
        assert_eq!(
            render(&session, 40, 10),
            render_with_hover(&session, 40, 10, None)
        );
    }

    #[test]
    fn wrapping_breaks_at_whitespace_and_keeps_paragraph_breaks() {
        let lines = wrap("alpha beta gamma\n\nsecond para", 11, 10);
        assert_eq!(lines, vec!["alpha beta", "gamma", "", "second para"]);
    }

    #[test]
    fn a_word_longer_than_the_box_is_cut_rather_than_overflowing() {
        // The alternative is a broken border.
        let lines = wrap("supercalifragilistic", 8, 10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].chars().count(), 8);
    }

    #[test]
    fn wrapping_drops_a_leading_blank_line_and_any_trailing_ones() {
        // A gap above the top border, or below the last line, is just a hole.
        assert_eq!(wrap("\n\nx\n\n\n", 10, 10), vec!["x"]);
    }

    fn completion(labels: &[&str]) -> crate::suggest::Suggest {
        let items = labels
            .iter()
            .map(|label| deco_lsp::requests::CompletionItem {
                label: (*label).to_owned(),
                kind: deco_lsp::requests::CompletionKind::Function,
                detail: None,
                insert: (*label).to_owned(),
                replace: None,
                filter: (*label).to_owned(),
                sort: None,
                preselect: false,
                was_snippet: false,
            })
            .collect();
        crate::suggest::Suggest::new(items, deco_core::Position::ZERO, false)
    }

    #[test]
    fn a_completion_list_is_drawn_below_the_cursor() {
        let session = session("fn main() {}\n");
        let frame =
            render_with_overlays(&session, 40, 10, None, Some(&completion(&["push", "pop"])));
        let rows: Vec<String> = frame.rows.iter().map(Row::plain).collect();
        assert!(rows[1].contains("pop"), "{:?}", rows[1]);
        assert!(rows[2].contains("push"), "{:?}", rows[2]);
    }

    #[test]
    fn the_completion_list_wins_over_a_hover() {
        // Both want the space beside the cursor, and the list is what the user
        // is interacting with.
        let session = session("fn main() {}\n");
        let frame = render_with_overlays(
            &session,
            40,
            10,
            Some(&hover("documentation")),
            Some(&completion(&["push"])),
        );
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(all.contains("push"));
        assert!(!all.contains("documentation"), "the hover was also drawn");
    }

    #[test]
    fn the_selected_row_is_inverted_rather_than_marked() {
        // A marker column would cost width the labels need.
        let session = session("fn main() {}\n");
        let mut suggest = completion(&["a", "b"]);
        suggest.next();
        let frame = render_with_overlays(&session, 40, 10, None, Some(&suggest));

        let selected = &frame.rows[2].spans[0];
        let unselected = &frame.rows[1].spans[0];
        assert_eq!(
            selected.fg, unselected.bg,
            "the selected row swaps foreground and background"
        );
        assert_eq!(selected.bg, unselected.fg);
    }

    #[test]
    fn a_completion_list_never_exceeds_the_terminal_width() {
        for width in [14, 30, 80] {
            let session = session("fn main() {}\n");
            let frame = render_with_overlays(
                &session,
                width,
                12,
                None,
                Some(&completion(&["a_very_long_completion_label_indeed"])),
            );
            for (index, row) in frame.rows.iter().enumerate() {
                assert_eq!(columns(&row.plain()), width, "row {index} at width {width}");
            }
        }
    }

    #[test]
    fn a_long_detail_is_trimmed_before_the_label() {
        // The label is what is being chosen between, so it keeps its room.
        let session = session("fn main() {}\n");
        let mut suggest = completion(&["push"]);
        // Rebuild with a detail long enough to need cutting.
        suggest = {
            let mut items: Vec<_> = suggest
                .visible()
                .into_iter()
                .map(|shown| shown.item.clone())
                .collect();
            items[0].detail = Some("fn(&mut self, value: T) -> Result<(), OverflowError>".into());
            crate::suggest::Suggest::new(items, deco_core::Position::ZERO, false)
        };

        let frame = render_with_overlays(&session, 30, 10, None, Some(&suggest));
        let row = frame.rows[1].plain();
        assert!(row.contains("push"), "the label survived: {row:?}");
        assert_eq!(columns(&row), 30);
    }

    #[test]
    fn a_completion_list_goes_above_the_cursor_when_it_would_not_fit_below() {
        let mut session = session(&"line\n".repeat(20));
        session.resize(40, 9);
        session.view.selections = deco_core::SelectionSet::caret(deco_core::Position::new(8, 0));
        session
            .view
            .reveal_cursor(&session.document.buffer, &session.document.settings);

        let frame = render_with_overlays(
            &session,
            40,
            10,
            None,
            Some(&completion(&["a", "b", "c", "d", "e", "f"])),
        );
        let cursor_row = frame.cursor.expect("on screen").1 as usize;
        let first = frame
            .rows
            .iter()
            .position(|row| row.plain().contains(" f a"))
            .expect("a row was drawn");
        assert!(
            first < cursor_row,
            "list at {first}, cursor at {cursor_row}"
        );
    }

    #[test]
    fn an_empty_completion_list_draws_nothing() {
        let session = session("fn main() {}\n");
        let plain_before: Vec<String> = render(&session, 40, 10)
            .rows
            .iter()
            .map(Row::plain)
            .collect();
        let frame = render_with_overlays(&session, 40, 10, None, Some(&completion(&[])));
        let plain_after: Vec<String> = frame.rows.iter().map(Row::plain).collect();
        assert_eq!(plain_before, plain_after);
    }

    #[test]
    fn a_terminal_too_narrow_for_the_list_draws_none() {
        let session = session("fn main() {}\n");
        let frame = render_with_overlays(&session, 8, 10, None, Some(&completion(&["push"])));
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(!all.contains("push"));
    }

    // ---- The find bar ---------------------------------------------------

    /// A session searching `text` for `query`, as though the user had typed it.
    fn searching(text: &str, query: &str) -> Session {
        let mut session = session(text);
        session.resize(40, 8);
        session.run("actions.find", None, 0);
        for c in query.chars() {
            session.handle_chord(deco_keymap::keys::Chord::parse(&c.to_string()).unwrap(), 0);
        }
        session
    }

    /// The find bar's row: the second from the bottom while it is open.
    fn find_row(frame: &Frame) -> String {
        frame.rows[frame.rows.len() - 2].plain()
    }

    #[test]
    fn the_find_bar_is_drawn_above_the_status_bar() {
        let session = searching("foo\n", "foo");
        let frame = render(&session, 40, 8);
        assert!(
            find_row(&frame).contains("Find: foo"),
            "{:?}",
            find_row(&frame)
        );
        // The status bar is still the last row and still says where the cursor is.
        assert!(frame.rows.last().unwrap().plain().contains("Ln 1"));
    }

    #[test]
    fn the_find_bar_costs_the_text_area_a_row() {
        let text = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let closed = render(&session(text), 40, 8);
        let open = render(&searching(text, "zzz"), 40, 8);
        assert_eq!(closed.rows.len(), open.rows.len(), "both fill the terminal");
        // The line that was on the last text row is no longer drawn.
        assert!(closed.rows[6].plain().contains('g'));
        assert!(
            !open.rows[6].plain().contains('g'),
            "{:?}",
            open.rows[6].plain()
        );
    }

    #[test]
    fn every_row_is_still_exactly_the_terminal_width() {
        let frame = render(&searching("foo\n", "foo"), 40, 8);
        for row in &frame.rows {
            assert_eq!(row.plain().chars().count(), 40, "row was {:?}", row.plain());
        }
    }

    #[test]
    fn the_bar_counts_the_matches_and_says_which_one_is_current() {
        let session = searching("foo\nfoo\nfoo\n", "foo");
        assert!(find_row(&render(&session, 40, 8)).contains("1 of 3"));
    }

    #[test]
    fn the_bar_says_when_nothing_matches() {
        let session = searching("foo\n", "zzz");
        assert!(find_row(&render(&session, 40, 8)).contains("No results"));
    }

    #[test]
    fn an_empty_query_is_counted_neither_way() {
        let session = searching("foo\n", "");
        let row = find_row(&render(&session, 40, 8));
        assert!(!row.contains("No results"), "{row:?}");
        assert!(!row.contains(" of "), "{row:?}");
    }

    #[test]
    fn moving_off_the_match_reports_the_total_without_claiming_a_position() {
        let mut session = searching("foo\nfoo\n", "foo");
        session.find.close();
        session.find.refresh(&session.document.buffer);
        session.view.selections = SelectionSet::caret(Position::new(1, 2));
        session.find.open(None, Position::ZERO);
        // Reopened without re-searching, so the matches stand but the cursor is
        // not on one.
        session.find.refresh(&session.document.buffer);
        let row = find_row(&render(&session, 40, 8));
        assert!(row.contains("2 results"), "{row:?}");
        assert!(!row.contains(" of "), "{row:?}");
    }

    #[test]
    fn the_toggles_show_which_options_are_on() {
        let mut session = searching("foo\n", "foo");
        let row = find_row(&render(&session, 40, 8));
        assert!(row.contains("[aa ww]") || row.contains("[a"), "{row:?}");
        session.run("toggleFindCaseSensitive", None, 0);
        session.run("toggleFindWholeWord", None, 0);
        let row = find_row(&render(&session, 40, 8));
        assert!(row.contains("[Aa Ww]"), "{row:?}");
    }

    #[test]
    fn the_cursor_sits_in_the_query_while_the_bar_has_the_keyboard() {
        let session = searching("foo\n", "fo");
        let frame = render(&session, 40, 8);
        let (x, y) = frame.cursor.expect("the caret has to be somewhere");
        assert_eq!(y as usize, frame.rows.len() - 2, "on the find bar's row");
        // " Find: " is seven columns, then two typed characters.
        assert_eq!(x, 9);
    }

    #[test]
    fn the_cursor_returns_to_the_document_when_the_bar_closes() {
        let mut session = searching("foo\n", "foo");
        session.run("closeFindWidget", None, 0);
        let frame = render(&session, 40, 8);
        let (_, y) = frame.cursor.unwrap();
        assert!(
            (y as usize) < frame.rows.len() - 1,
            "the caret should be back in the text"
        );
    }

    #[test]
    fn a_query_longer_than_the_bar_scrolls_to_keep_the_caret_visible() {
        let session = searching("x\n", "abcdefghijklmnopqrstuvwxyz");
        let frame = render(&session, 20, 8);
        let row = find_row(&frame);
        assert_eq!(row.chars().count(), 20);
        // The tail is what matters: the caret is at the end of what was typed.
        assert!(row.contains('z'), "{row:?}");
        assert!(
            !row.contains("abc"),
            "the head should have scrolled off: {row:?}"
        );
        let (x, _) = frame.cursor.unwrap();
        assert!((x as usize) < 20, "the caret must stay on screen");
    }

    #[test]
    fn a_narrow_bar_drops_the_readouts_before_the_query() {
        // A search term you cannot see is a search term you cannot correct, so
        // the count goes first and the toggles second.
        let session = searching("foo\n", "foo");
        let wide = find_row(&render(&session, 40, 8));
        assert!(
            wide.contains("1 of 1") && wide.contains("[aa ww]"),
            "{wide:?}"
        );

        let narrow = find_row(&render(&session, 24, 8));
        assert!(
            !narrow.contains("1 of 1"),
            "the count should go first: {narrow:?}"
        );
        assert!(narrow.contains("[aa ww]"), "{narrow:?}");
        assert!(narrow.contains("foo"), "{narrow:?}");

        let tiny = find_row(&render(&session, 16, 8));
        assert!(!tiny.contains("[aa ww]"), "the toggles go next: {tiny:?}");
        assert!(tiny.contains("foo"), "the query survives: {tiny:?}");
    }

    #[test]
    fn every_match_is_highlighted_and_the_current_one_differently() {
        let session = searching("foo bar foo\n", "foo");
        let palette = Palette::from(&session);
        let frame = render(&session, 40, 8);
        let backgrounds: Vec<Rgba> = frame.rows[0]
            .spans
            .iter()
            .flat_map(|span| span.text.chars().map(move |_| span.bg))
            .collect();
        let gutter = gutter_width(&session);
        // The first `foo` is where the cursor is; the second is a plain highlight.
        assert_eq!(backgrounds[gutter], palette.find_match_bg);
        assert_eq!(backgrounds[gutter + 3], palette.bg, "the space between");
        assert_eq!(backgrounds[gutter + 8], palette.find_highlight_bg);
    }

    #[test]
    fn nothing_is_highlighted_once_the_bar_is_closed() {
        let mut session = searching("foo foo\n", "foo");
        session.run("closeFindWidget", None, 0);
        let palette = Palette::from(&session);
        let frame = render(&session, 40, 8);
        let gutter = gutter_width(&session);
        let backgrounds: Vec<Rgba> = frame.rows[0]
            .spans
            .iter()
            .flat_map(|span| span.text.chars().map(move |_| span.bg))
            .collect();
        // The first match is still selected — closing the bar does not deselect —
        // but the second is back to plain text.
        assert_eq!(backgrounds[gutter], palette.selection_bg);
        assert_eq!(backgrounds[gutter + 4], palette.bg);
    }

    #[test]
    fn a_completion_list_is_not_drawn_over_the_find_bar() {
        let session = searching("fn main() {}\n", "main");
        let frame = render_with_overlays(
            &session,
            40,
            8,
            None,
            Some(&completion(&["a", "b", "c", "d", "e", "f", "g", "h"])),
        );
        assert!(
            find_row(&frame).contains("Find: main"),
            "{:?}",
            find_row(&frame)
        );
    }

    #[test]
    fn a_one_row_terminal_still_renders_something() {
        // Not enough room for both bars; the status bar wins, since it is where
        // the editor says what is wrong.
        let frame = render(&searching("foo\n", "foo"), 40, 1);
        assert_eq!(frame.rows.len(), 2);
    }
}
