//! What a terminal shows: a grid of cells, a cursor, and the lines that have
//! scrolled off the top.
//!
//! [`Screen`] implements [`Perform`], so it applies the actions the parser
//! finds. The control functions follow xterm's behaviour as documented in
//! "XTerm Control Sequences" (<https://invisible-island.net/xterm/ctlseqs/ctlseqs.html>),
//! for the subset that programs using `TERM=xterm-256color` rely on: cursor
//! movement, erasing, insertion and deletion, scroll regions, SGR colours
//! including 256-colour and direct colour, the alternate screen, and the
//! status reports a shell or a full-screen program asks for.
//!
//! Lines are not reflowed when the width changes. A line keeps its cells and
//! is cut or padded, as in xterm.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

use crate::parser::{Params, Perform};

/// A colour as the program asked for it. The frontend maps it to the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// The terminal's default foreground or background.
    #[default]
    Default,
    /// One of the 256 indexed colours. 0-15 are the theme's ANSI colours.
    Indexed(u8),
    /// A direct colour.
    Rgb(u8, u8, u8),
}

/// Character attributes set by SGR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attributes {
    /// SGR 1.
    pub bold: bool,
    /// SGR 2: faint.
    pub dim: bool,
    /// SGR 3.
    pub italic: bool,
    /// SGR 4, of any underline style.
    pub underline: bool,
    /// SGR 5 and 6. Not drawn as blinking.
    pub blink: bool,
    /// SGR 7: foreground and background swapped.
    pub inverse: bool,
    /// SGR 8: drawn in the background colour.
    pub hidden: bool,
    /// SGR 9.
    pub strikethrough: bool,
}

/// How a cell is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// The foreground colour.
    pub fg: Color,
    /// The background colour.
    pub bg: Color,
    /// The SGR attributes.
    pub attributes: Attributes,
}

/// One character cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The character, or a space for an empty cell.
    pub c: char,
    /// Combining characters that follow `c`, if any.
    pub combining: Option<Box<str>>,
    /// 1 for a normal cell, 2 for the left half of a wide character, and 0
    /// for the right half, which holds no character of its own.
    pub width: u8,
    /// How the cell is drawn.
    pub style: Style,
}

impl Cell {
    /// An empty cell with `style`'s colours. Erasing uses the current
    /// background, as xterm does (`bce`).
    pub fn blank(style: Style) -> Self {
        Self {
            c: ' ',
            combining: None,
            width: 1,
            style: Style {
                fg: style.fg,
                bg: style.bg,
                attributes: Attributes::default(),
            },
        }
    }

    /// Whether this is the right half of a wide character.
    pub fn is_continuation(&self) -> bool {
        self.width == 0
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank(Style::default())
    }
}

/// One row of cells.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line {
    /// The line's cells, as many as the screen is wide.
    pub cells: Vec<Cell>,
    /// Whether the text continues on the next line because it wrapped.
    pub wrapped: bool,
}

impl Line {
    fn blank(width: usize, style: Style) -> Self {
        Self {
            cells: vec![Cell::blank(style); width],
            wrapped: false,
        }
    }

    /// The line's text, without trailing blanks.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for cell in &self.cells {
            if cell.is_continuation() {
                continue;
            }
            text.push(cell.c);
            if let Some(combining) = &cell.combining {
                text.push_str(combining);
            }
        }
        text.trim_end_matches(' ').to_owned()
    }

    fn resize(&mut self, width: usize) {
        self.cells.resize(width, Cell::default());
        // A wide character cut in half leaves a left half with nothing after it.
        if let Some(last) = self.cells.last_mut() {
            if last.width == 2 {
                *last = Cell::blank(last.style);
            }
        }
    }
}

/// The modes a program can switch, as far as the frontend needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    /// DECCKM: arrow keys send `ESC O` rather than `ESC [`.
    pub application_cursor: bool,
    /// DECKPAM: the keypad sends application sequences.
    pub application_keypad: bool,
    /// Mode 2004: pasted text is bracketed.
    pub bracketed_paste: bool,
    /// DECTCEM: the cursor is shown.
    pub cursor_visible: bool,
    /// DECAWM: printing past the last column continues on the next line.
    pub autowrap: bool,
    /// DECOM: cursor positions are relative to the scroll region.
    pub origin: bool,
    /// IRM: printing shifts the rest of the line right.
    pub insert: bool,
    /// Which mouse events the program asked to be sent (modes 1000, 1002 and
    /// 1003).
    pub mouse: MouseTracking,
    /// Mode 1006: mouse events are sent in the SGR form, which has no limit
    /// on the column and tells a release from a press.
    pub mouse_sgr: bool,
}

/// Which mouse events a program in the terminal receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseTracking {
    /// None: the mouse belongs to the editor.
    #[default]
    Off,
    /// Mode 1000: presses, releases and the wheel.
    Press,
    /// Mode 1002: those, and moves while a button is held.
    Drag,
    /// Mode 1003: those, and every move.
    Any,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: false,
            cursor_visible: true,
            autowrap: true,
            origin: false,
            insert: false,
            mouse: MouseTracking::Off,
            mouse_sgr: false,
        }
    }
}

/// The cursor's position and the style new characters get.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    /// The row, from 0 at the top.
    pub row: usize,
    /// The column, from 0 at the left.
    pub col: usize,
    style: Style,
    /// The last column was printed and the next character goes to the next
    /// line. Moving the cursor cancels it.
    pending_wrap: bool,
}

/// What DECSC saves and DECRC restores.
#[derive(Debug, Clone, Copy, Default)]
struct Saved {
    cursor: Cursor,
    origin: bool,
    autowrap: bool,
    charsets: Charsets,
}

/// The G0 and G1 character sets, which select the DEC line-drawing set.
#[derive(Debug, Clone, Copy, Default)]
struct Charsets {
    g0_drawing: bool,
    g1_drawing: bool,
    /// SO selected G1; SI selects G0 again.
    shifted: bool,
}

impl Charsets {
    fn map(&self, c: char) -> char {
        let drawing = if self.shifted {
            self.g1_drawing
        } else {
            self.g0_drawing
        };
        if !drawing {
            return c;
        }
        // The DEC Special Graphics set, for `0x5f` to `0x7e`.
        const DRAWING: &str = " ◆▒␉␌␍␊°±␤␋┘┐┌└┼⎺⎻─⎼⎽├┤┴┬│≤≥π≠£·";
        match c {
            '_'..='~' => DRAWING.chars().nth(c as usize - '_' as usize).unwrap_or(c),
            _ => c,
        }
    }
}

/// The primary screen's lines while the alternate screen is shown.
#[derive(Debug, Clone)]
struct Stashed {
    lines: Vec<Line>,
    /// The cursor saved by mode 1049, restored when the program leaves.
    cursor: Option<Saved>,
}

/// A terminal's screen.
#[derive(Debug, Clone)]
pub struct Screen {
    cols: usize,
    rows: usize,
    lines: Vec<Line>,
    /// Lines that scrolled off the top of the primary screen, oldest first.
    scrollback: VecDeque<Line>,
    scrollback_limit: usize,
    /// How many lines have moved into the history since the screen was made,
    /// including lines the limit has since dropped.
    scrolled: u64,
    /// Set while the alternate screen is shown.
    primary: Option<Stashed>,
    cursor: Cursor,
    saved: Saved,
    /// The scroll region, inclusive.
    top: usize,
    bottom: usize,
    tabs: Vec<bool>,
    modes: Modes,
    charsets: Charsets,
    title: String,
    /// The last printed character, for REP.
    last_printed: Option<char>,
    /// Replies to status requests, to be written back to the program.
    responses: Vec<u8>,
}

impl Screen {
    /// An empty screen of `cols` by `rows` cells that keeps up to
    /// `scrollback` lines of history.
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Self {
            cols,
            rows,
            lines: vec![Line::blank(cols, Style::default()); rows],
            scrollback: VecDeque::new(),
            scrollback_limit: scrollback,
            primary: None,
            cursor: Cursor::default(),
            saved: Saved {
                autowrap: true,
                ..Saved::default()
            },
            top: 0,
            bottom: rows - 1,
            tabs: default_tabs(cols),
            modes: Modes::default(),
            charsets: Charsets::default(),
            title: String::new(),
            last_printed: None,
            scrolled: 0,
            responses: Vec::new(),
        }
    }

    /// The width in cells.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// The height in cells.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The cursor's row and column.
    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor.row, self.cursor.col)
    }

    /// The modes the program has set.
    pub fn modes(&self) -> Modes {
        self.modes
    }

    /// The title the program set with OSC 0 or 2.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Whether the alternate screen is shown, which has no scrollback.
    pub fn is_alternate(&self) -> bool {
        self.primary.is_some()
    }

    /// How many lines output has moved into the history so far.
    ///
    /// A count rather than the history's length, which stops growing at the
    /// limit while lines keep moving through it.
    pub fn scrolled(&self) -> u64 {
        self.scrolled
    }

    /// How many lines of history there are.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Row `row` of the view scrolled `back` lines into the history.
    ///
    /// With `back` at 0 this is the screen itself. A line from the history
    /// may be narrower or wider than the screen, if the screen was resized
    /// after it scrolled off.
    pub fn line(&self, row: usize, back: usize) -> Option<&Line> {
        let back = back.min(self.scrollback.len());
        let index = self.scrollback.len() - back + row;
        if index < self.scrollback.len() {
            self.scrollback.get(index)
        } else {
            self.lines.get(index - self.scrollback.len())
        }
    }

    /// The text on the screen, one line per row, without trailing blanks.
    pub fn text(&self) -> String {
        let lines: Vec<String> = self.lines.iter().map(Line::text).collect();
        lines.join("\n").trim_end_matches('\n').to_owned()
    }

    /// Takes the replies to status requests the program made, which must be
    /// written to its input.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// Changes the size, keeping the cursor's line on screen.
    ///
    /// When the screen gets shorter, blank lines below the cursor are dropped
    /// first, and then lines at the top move into the history. When it gets
    /// taller, lines come back from the history before blank ones are added
    /// below.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        let alternate = self.primary.is_some();
        if rows < self.rows {
            let mut excess = self.rows - rows;
            while excess > 0
                && self.lines.len() - 1 > self.cursor.row
                && self.lines.last().is_some_and(|line| line.text().is_empty())
            {
                self.lines.pop();
                excess -= 1;
            }
            for line in self.lines.drain(..excess) {
                if !alternate {
                    self.scrollback.push_back(line);
                }
            }
            self.cursor.row = self.cursor.row.saturating_sub(excess);
        } else {
            for _ in self.rows..rows {
                match (alternate, self.scrollback.pop_back()) {
                    (false, Some(line)) => {
                        self.lines.insert(0, line);
                        self.cursor.row += 1;
                    }
                    (_, line) => {
                        if let Some(line) = line {
                            self.scrollback.push_back(line);
                        }
                        self.lines.push(Line::blank(cols, Style::default()));
                    }
                }
            }
        }
        self.trim_scrollback();
        for line in &mut self.lines {
            line.resize(cols);
        }
        if let Some(stashed) = &mut self.primary {
            stashed
                .lines
                .resize(rows, Line::blank(cols, Style::default()));
            for line in &mut stashed.lines {
                line.resize(cols);
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.tabs = default_tabs(cols);
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.pending_wrap = false;
        self.saved.cursor.row = self.saved.cursor.row.min(rows - 1);
        self.saved.cursor.col = self.saved.cursor.col.min(cols - 1);
    }

    // ---- printing ---------------------------------------------------------

    fn print_char(&mut self, c: char) {
        let c = self.charsets.map(c);
        let width = match c.width() {
            Some(width) => width,
            // Control characters have no width and are not printed.
            None => return,
        };
        if width == 0 {
            self.combine(c);
            return;
        }
        if self.cursor.pending_wrap {
            self.wrap();
        }
        if width == 2 && self.cursor.col + 1 >= self.cols {
            if self.cols < 2 {
                return;
            }
            if self.modes.autowrap {
                // The wide character does not fit: the last cell stays empty
                // and the character starts the next line.
                let style = self.cursor.style;
                let (row, col) = (self.cursor.row, self.cursor.col);
                self.lines[row].cells[col] = Cell::blank(style);
                self.wrap();
            } else {
                self.cursor.col = self.cols - 2;
            }
        }
        let (row, col) = (self.cursor.row, self.cursor.col);
        if self.modes.insert {
            self.insert_blanks(width);
        }
        self.clear_wide_at(row, col);
        if width == 2 {
            self.clear_wide_at(row, col + 1);
        }
        let style = self.cursor.style;
        let cells = &mut self.lines[row].cells;
        cells[col] = Cell {
            c,
            combining: None,
            width: width as u8,
            style,
        };
        if width == 2 {
            cells[col + 1] = Cell {
                c: ' ',
                combining: None,
                width: 0,
                style,
            };
        }
        self.last_printed = Some(c);
        let next = col + width;
        if next >= self.cols {
            self.cursor.col = self.cols - 1;
            self.cursor.pending_wrap = self.modes.autowrap;
        } else {
            self.cursor.col = next;
        }
    }

    /// Adds a combining character to the character before the cursor.
    fn combine(&mut self, c: char) {
        let row = self.cursor.row;
        let mut col = if self.cursor.pending_wrap {
            self.cursor.col
        } else if self.cursor.col > 0 {
            self.cursor.col - 1
        } else {
            return;
        };
        if self.lines[row].cells[col].is_continuation() && col > 0 {
            col -= 1;
        }
        let cell = &mut self.lines[row].cells[col];
        let mut combining = cell.combining.take().map(String::from).unwrap_or_default();
        // Bounded, so a stream of combining marks cannot grow a cell forever.
        if combining.len() < 32 {
            combining.push(c);
        }
        cell.combining = Some(combining.into_boxed_str());
    }

    /// Moves to the start of the next line after printing in the last column.
    fn wrap(&mut self) {
        self.lines[self.cursor.row].wrapped = true;
        self.cursor.col = 0;
        self.cursor.pending_wrap = false;
        self.index();
    }

    /// Breaks a wide character that `col` is half of, so that overwriting one
    /// half does not leave the other.
    fn clear_wide_at(&mut self, row: usize, col: usize) {
        let cells = &mut self.lines[row].cells;
        let Some(cell) = cells.get(col) else { return };
        if cell.is_continuation() && col > 0 {
            let style = cells[col - 1].style;
            cells[col - 1] = Cell::blank(style);
            cells[col] = Cell::blank(style);
        } else if cell.width == 2 && col + 1 < cells.len() {
            let style = cell.style;
            cells[col + 1] = Cell::blank(style);
        }
    }

    // ---- cursor movement --------------------------------------------------

    fn set_col(&mut self, col: usize) {
        self.cursor.col = col.min(self.cols - 1);
        self.cursor.pending_wrap = false;
    }

    /// Moves to `row`, kept within the scroll region if the cursor is in it,
    /// as CUU and CUD do.
    fn move_vertically(&mut self, row: isize) {
        let (low, high) = if (self.top..=self.bottom).contains(&self.cursor.row) {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        };
        self.cursor.row = row.clamp(low as isize, high as isize) as usize;
        self.cursor.pending_wrap = false;
    }

    /// CUP: `row` and `col` are 0-based, relative to the scroll region in
    /// origin mode.
    fn go_to(&mut self, row: usize, col: usize) {
        let (offset, limit) = if self.modes.origin {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        };
        self.cursor.row = (row + offset).min(limit);
        self.set_col(col);
    }

    /// LF and IND: down one line, scrolling at the bottom of the region.
    fn index(&mut self) {
        if self.cursor.row == self.bottom {
            self.scroll_up(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    /// RI: up one line, scrolling at the top of the region.
    fn reverse_index(&mut self) {
        if self.cursor.row == self.top {
            self.scroll_down(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    fn tab_forward(&mut self, count: usize) {
        for _ in 0..count {
            let next = (self.cursor.col + 1..self.cols).find(|&col| self.tabs[col]);
            self.cursor.col = next.unwrap_or(self.cols - 1);
        }
        self.cursor.pending_wrap = false;
    }

    fn tab_backward(&mut self, count: usize) {
        for _ in 0..count {
            let previous = (0..self.cursor.col).rev().find(|&col| self.tabs[col]);
            self.cursor.col = previous.unwrap_or(0);
        }
        self.cursor.pending_wrap = false;
    }

    // ---- scrolling and editing -------------------------------------------

    /// Moves the lines of the scroll region up by `count`. Lines leaving the
    /// top of a full-screen region on the primary screen go to the history.
    fn scroll_up(&mut self, count: usize) {
        let count = count.min(self.bottom - self.top + 1);
        let blank = Line::blank(self.cols, self.cursor.style);
        let removed: Vec<Line> = self.lines.drain(self.top..self.top + count).collect();
        let at = self.bottom + 1 - count;
        for _ in 0..count {
            self.lines.insert(at, blank.clone());
        }
        if self.top == 0 && self.primary.is_none() {
            self.scrolled += removed.len() as u64;
            self.scrollback.extend(removed);
            self.trim_scrollback();
        }
    }

    fn scroll_down(&mut self, count: usize) {
        let count = count.min(self.bottom - self.top + 1);
        let blank = Line::blank(self.cols, self.cursor.style);
        self.lines.drain(self.bottom + 1 - count..=self.bottom);
        for _ in 0..count {
            self.lines.insert(self.top, blank.clone());
        }
    }

    fn trim_scrollback(&mut self) {
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
    }

    /// IL: blank lines at the cursor, pushing the rest of the region down.
    fn insert_lines(&mut self, count: usize) {
        if !(self.top..=self.bottom).contains(&self.cursor.row) {
            return;
        }
        let count = count.min(self.bottom - self.cursor.row + 1);
        let blank = Line::blank(self.cols, self.cursor.style);
        self.lines.drain(self.bottom + 1 - count..=self.bottom);
        for _ in 0..count {
            self.lines.insert(self.cursor.row, blank.clone());
        }
        self.set_col(0);
    }

    /// DL: removes lines at the cursor, pulling the rest of the region up.
    fn delete_lines(&mut self, count: usize) {
        if !(self.top..=self.bottom).contains(&self.cursor.row) {
            return;
        }
        let count = count.min(self.bottom - self.cursor.row + 1);
        let blank = Line::blank(self.cols, self.cursor.style);
        self.lines.drain(self.cursor.row..self.cursor.row + count);
        for _ in 0..count {
            self.lines.insert(self.bottom + 1 - count, blank.clone());
        }
        self.set_col(0);
    }

    /// ICH: blank cells at the cursor, pushing the rest of the line right.
    fn insert_blanks(&mut self, count: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.clear_wide_at(row, col);
        let count = count.min(self.cols - col);
        let style = self.cursor.style;
        let cells = &mut self.lines[row].cells;
        cells.truncate(self.cols - count);
        for _ in 0..count {
            cells.insert(col, Cell::blank(style));
        }
        // A wide character pushed off the end loses its right half.
        if cells[self.cols - 1].width == 2 {
            cells[self.cols - 1] = Cell::blank(style);
        }
    }

    /// DCH: removes cells at the cursor, pulling the rest of the line left.
    fn delete_chars(&mut self, count: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.clear_wide_at(row, col);
        let count = count.min(self.cols - col);
        self.clear_wide_at(row, col + count);
        let style = self.cursor.style;
        let cells = &mut self.lines[row].cells;
        cells.drain(col..col + count);
        cells.resize(self.cols, Cell::blank(style));
    }

    /// Blanks cells `from..to` of `row`.
    fn erase_cells(&mut self, row: usize, from: usize, to: usize) {
        let to = to.min(self.cols);
        if from >= to {
            return;
        }
        self.clear_wide_at(row, from);
        self.clear_wide_at(row, to - 1);
        let blank = Cell::blank(self.cursor.style);
        for cell in &mut self.lines[row].cells[from..to] {
            *cell = blank.clone();
        }
    }

    fn erase_line(&mut self, row: usize) {
        self.lines[row] = Line::blank(self.cols, self.cursor.style);
    }

    /// ED.
    fn erase_display(&mut self, mode: u16) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        match mode {
            0 => {
                self.erase_cells(row, col, self.cols);
                self.lines[row].wrapped = false;
                for below in row + 1..self.rows {
                    self.erase_line(below);
                }
            }
            1 => {
                for above in 0..row {
                    self.erase_line(above);
                }
                self.erase_cells(row, 0, col + 1);
            }
            2 => {
                for any in 0..self.rows {
                    self.erase_line(any);
                }
            }
            3 => self.scrollback.clear(),
            _ => {}
        }
    }

    /// EL.
    fn erase_in_line(&mut self, mode: u16) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        match mode {
            0 => {
                self.erase_cells(row, col, self.cols);
                self.lines[row].wrapped = false;
            }
            1 => self.erase_cells(row, 0, col + 1),
            2 => self.erase_line(row),
            _ => {}
        }
    }

    // ---- modes and state --------------------------------------------------

    fn save_cursor(&mut self) {
        self.saved = Saved {
            cursor: self.cursor,
            origin: self.modes.origin,
            autowrap: self.modes.autowrap,
            charsets: self.charsets,
        };
    }

    fn restore_cursor(&mut self) {
        let saved = self.saved;
        self.cursor = saved.cursor;
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
        self.modes.origin = saved.origin;
        self.modes.autowrap = saved.autowrap;
        self.charsets = saved.charsets;
    }

    fn enter_alternate(&mut self, save_cursor: bool) {
        if self.primary.is_some() {
            return;
        }
        let cursor = save_cursor.then(|| {
            self.save_cursor();
            self.saved
        });
        let blank = vec![Line::blank(self.cols, Style::default()); self.rows];
        let lines = std::mem::replace(&mut self.lines, blank);
        self.primary = Some(Stashed { lines, cursor });
    }

    fn leave_alternate(&mut self) {
        let Some(stashed) = self.primary.take() else {
            return;
        };
        self.lines = stashed.lines;
        if let Some(saved) = stashed.cursor {
            self.saved = saved;
            self.restore_cursor();
        }
    }

    fn set_private_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.modes.application_cursor = on,
            6 => {
                self.modes.origin = on;
                self.go_to(0, 0);
            }
            7 => self.modes.autowrap = on,
            25 => self.modes.cursor_visible = on,
            47 | 1047 => {
                if on {
                    self.enter_alternate(false);
                } else {
                    self.leave_alternate();
                }
            }
            1048 => {
                if on {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            1049 => {
                if on {
                    self.enter_alternate(true);
                    self.go_to(0, 0);
                } else {
                    self.leave_alternate();
                }
            }
            2004 => self.modes.bracketed_paste = on,
            // Turning one tracking mode off turns tracking off, as in xterm.
            1000 | 1002 | 1003 => {
                self.modes.mouse = match (mode, on) {
                    (_, false) => MouseTracking::Off,
                    (1000, true) => MouseTracking::Press,
                    (1002, true) => MouseTracking::Drag,
                    _ => MouseTracking::Any,
                };
            }
            1006 => self.modes.mouse_sgr = on,
            // Focus events, synchronized output, the other mouse encodings
            // and the rest are accepted and have no effect.
            _ => {}
        }
    }

    /// DECSTR: the soft reset.
    fn soft_reset(&mut self) {
        let visible = self.modes.cursor_visible;
        self.modes = Modes::default();
        self.modes.cursor_visible = visible;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.cursor.style = Style::default();
        self.charsets = Charsets::default();
        self.saved = Saved {
            autowrap: true,
            ..Saved::default()
        };
    }

    /// RIS: everything back to the start, except the history.
    fn reset(&mut self) {
        let scrollback = std::mem::take(&mut self.scrollback);
        *self = Self::new(self.cols, self.rows, self.scrollback_limit);
        self.scrollback = scrollback;
    }

    fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        let top = usize::from(top.max(1)) - 1;
        let bottom = match bottom {
            0 => self.rows - 1,
            bottom => (usize::from(bottom) - 1).min(self.rows - 1),
        };
        if top < bottom {
            self.top = top;
            self.bottom = bottom;
            self.go_to(0, 0);
        }
    }

    fn select_graphic_rendition(&mut self, params: &Params) {
        let style = &mut self.cursor.style;
        if params.is_empty() {
            *style = Style::default();
            return;
        }
        let groups: Vec<&[u16]> = params.groups().collect();
        let mut index = 0;
        while index < groups.len() {
            let group = groups[index];
            index += 1;
            let attributes = &mut style.attributes;
            match group[0] {
                0 => *style = Style::default(),
                1 => attributes.bold = true,
                2 => attributes.dim = true,
                3 => attributes.italic = true,
                // `4:0` turns underlining off; the other styles draw a line.
                4 => attributes.underline = group.get(1).is_none_or(|&kind| kind != 0),
                5 | 6 => attributes.blink = true,
                7 => attributes.inverse = true,
                8 => attributes.hidden = true,
                9 => attributes.strikethrough = true,
                21 => attributes.underline = true,
                22 => {
                    attributes.bold = false;
                    attributes.dim = false;
                }
                23 => attributes.italic = false,
                24 => attributes.underline = false,
                25 => attributes.blink = false,
                27 => attributes.inverse = false,
                28 => attributes.hidden = false,
                29 => attributes.strikethrough = false,
                code @ 30..=37 => style.fg = Color::Indexed((code - 30) as u8),
                code @ 40..=47 => style.bg = Color::Indexed((code - 40) as u8),
                code @ 90..=97 => style.fg = Color::Indexed((code - 90 + 8) as u8),
                code @ 100..=107 => style.bg = Color::Indexed((code - 100 + 8) as u8),
                39 => style.fg = Color::Default,
                49 => style.bg = Color::Default,
                code @ (38 | 48 | 58) => {
                    let color = if group.len() > 1 {
                        extended_color(&group[1..], true)
                    } else {
                        // The `;` form: the colour is in the following
                        // parameters.
                        let rest: Vec<u16> = groups[index..].iter().map(|group| group[0]).collect();
                        let (color, used) = extended_color_separate(&rest);
                        index += used;
                        color
                    };
                    match (code, color) {
                        (38, Some(color)) => style.fg = color,
                        (48, Some(color)) => style.bg = color,
                        // The underline colour is not drawn.
                        _ => {}
                    }
                }
                59 => {}
                _ => {}
            }
        }
    }

    fn report(&mut self, reply: &str) {
        self.responses.extend_from_slice(reply.as_bytes());
    }
}

/// A tab stop every eight columns.
fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|col| col % 8 == 0 && col > 0).collect()
}

/// The colour after `38:`: `5:n`, or `2:[space]:r:g:b` where the colour
/// space id may be omitted.
fn extended_color(values: &[u16], colon: bool) -> Option<Color> {
    match values {
        [5, index, ..] => Some(Color::Indexed(clamp_u8(*index))),
        [2, _space, r, g, b, ..] if colon => {
            Some(Color::Rgb(clamp_u8(*r), clamp_u8(*g), clamp_u8(*b)))
        }
        [2, r, g, b] => Some(Color::Rgb(clamp_u8(*r), clamp_u8(*g), clamp_u8(*b))),
        _ => None,
    }
}

/// The colour after `38;`, taking as many following parameters as it needs.
/// Returns the colour and how many parameters it used.
fn extended_color_separate(rest: &[u16]) -> (Option<Color>, usize) {
    match rest {
        [5, index, ..] => (Some(Color::Indexed(clamp_u8(*index))), 2),
        [2, r, g, b, ..] => (
            Some(Color::Rgb(clamp_u8(*r), clamp_u8(*g), clamp_u8(*b))),
            4,
        ),
        // Malformed: the rest of the sequence is not read as attributes.
        _ => (None, rest.len()),
    }
}

fn clamp_u8(value: u16) -> u8 {
    value.min(255) as u8
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                if self.cursor.pending_wrap {
                    self.cursor.pending_wrap = false;
                } else if self.cursor.col > 0 {
                    self.cursor.col -= 1;
                }
            }
            0x09 => self.tab_forward(1),
            0x0a..=0x0c => {
                self.index();
                self.cursor.pending_wrap = false;
            }
            0x0d => self.set_col(0),
            0x0e => self.charsets.shifted = true,
            0x0f => self.charsets.shifted = false,
            // BEL and the rest have no visible effect.
            _ => {}
        }
    }

    fn csi(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: u8) {
        if ignore {
            return;
        }
        let count = |index| usize::from(params.count(index));
        match (intermediates, action) {
            ([], b'@') => self.insert_blanks(count(0)),
            ([], b'A') => self.move_vertically(self.cursor.row as isize - count(0) as isize),
            ([], b'B' | b'e') => self.move_vertically(self.cursor.row as isize + count(0) as isize),
            ([], b'C' | b'a') => self.set_col(self.cursor.col + count(0)),
            ([], b'D') => self.set_col(self.cursor.col.saturating_sub(count(0))),
            ([], b'E') => {
                self.move_vertically(self.cursor.row as isize + count(0) as isize);
                self.set_col(0);
            }
            ([], b'F') => {
                self.move_vertically(self.cursor.row as isize - count(0) as isize);
                self.set_col(0);
            }
            ([], b'G' | b'`') => self.set_col(count(0) - 1),
            ([], b'H' | b'f') => self.go_to(count(0) - 1, count(1) - 1),
            ([], b'I') => self.tab_forward(count(0)),
            ([], b'J') => self.erase_display(params.get(0)),
            ([b'?'], b'J') => self.erase_display(params.get(0)),
            ([], b'K') => self.erase_in_line(params.get(0)),
            ([b'?'], b'K') => self.erase_in_line(params.get(0)),
            ([], b'L') => self.insert_lines(count(0)),
            ([], b'M') => self.delete_lines(count(0)),
            ([], b'P') => self.delete_chars(count(0)),
            ([], b'S') => self.scroll_up(count(0)),
            ([], b'T') if params.len() <= 1 => self.scroll_down(count(0)),
            ([], b'X') => {
                let (row, col) = (self.cursor.row, self.cursor.col);
                self.erase_cells(row, col, col + count(0));
            }
            ([], b'Z') => self.tab_backward(count(0)),
            ([], b'b') => {
                if let Some(c) = self.last_printed {
                    // Bounded by the screen, so a huge count is cheap.
                    for _ in 0..count(0).min(self.cols * self.rows) {
                        self.print_char(c);
                    }
                }
            }
            ([], b'c') if params.get(0) == 0 => self.report("\x1b[?62;22c"),
            ([b'>'], b'c') if params.get(0) == 0 => self.report("\x1b[>0;10;1c"),
            ([], b'd') => {
                let offset = if self.modes.origin { self.top } else { 0 };
                self.cursor.row = (count(0) - 1 + offset).min(self.rows - 1);
                self.cursor.pending_wrap = false;
            }
            ([], b'g') => match params.get(0) {
                0 => self.tabs[self.cursor.col] = false,
                3 => self.tabs.iter_mut().for_each(|tab| *tab = false),
                _ => {}
            },
            ([], b'h' | b'l') => {
                let on = action == b'h';
                for group in params.groups() {
                    if group[0] == 4 {
                        self.modes.insert = on;
                    }
                }
            }
            ([b'?'], b'h' | b'l') => {
                let on = action == b'h';
                for group in params.groups() {
                    self.set_private_mode(group[0], on);
                }
            }
            ([], b'm') => self.select_graphic_rendition(params),
            ([], b'n') => match params.get(0) {
                5 => self.report("\x1b[0n"),
                6 => {
                    let row = if self.modes.origin {
                        self.cursor.row - self.top
                    } else {
                        self.cursor.row
                    };
                    let reply = format!("\x1b[{};{}R", row + 1, self.cursor.col + 1);
                    self.report(&reply);
                }
                _ => {}
            },
            ([b'!'], b'p') => self.soft_reset(),
            ([], b'r') => self.set_scroll_region(params.get(0), params.get(1)),
            ([], b's') => self.save_cursor(),
            ([], b'u') => self.restore_cursor(),
            // Cursor shape, window operations, keyboard protocol queries and
            // the rest are accepted and have no effect.
            _ => {}
        }
    }

    fn esc(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore {
            return;
        }
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => {
                self.index();
                self.cursor.pending_wrap = false;
            }
            ([], b'E') => {
                self.index();
                self.set_col(0);
            }
            ([], b'H') => self.tabs[self.cursor.col] = true,
            ([], b'M') => {
                self.reverse_index();
                self.cursor.pending_wrap = false;
            }
            ([], b'c') => self.reset(),
            ([], b'=') => self.modes.application_keypad = true,
            ([], b'>') => self.modes.application_keypad = false,
            ([b'('], set) => self.charsets.g0_drawing = set == b'0',
            ([b')'], set) => self.charsets.g1_drawing = set == b'0',
            ([b'#'], b'8') => {
                // DECALN: the screen filled with `E`, for alignment.
                for line in &mut self.lines {
                    for cell in &mut line.cells {
                        *cell = Cell {
                            c: 'E',
                            ..Cell::default()
                        };
                    }
                }
            }
            _ => {}
        }
    }

    fn osc(&mut self, fields: &[&[u8]]) {
        // Only the title is used. Clipboard writes (52), colour changes and
        // hyperlinks are ignored: a program in the terminal does not get to
        // change the editor's clipboard or theme.
        if let [b"0" | b"2", title @ ..] = fields {
            let title = title.join(&b';');
            self.title = String::from_utf8_lossy(&title)
                .chars()
                .filter(|c| !c.is_control())
                .take(256)
                .collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;

    fn screen(cols: usize, rows: usize, input: &str) -> Screen {
        let mut screen = Screen::new(cols, rows, 100);
        Parser::new().advance(&mut screen, input.as_bytes());
        screen
    }

    #[test]
    fn text_wraps_at_the_last_column_only_when_more_follows() {
        let s = screen(5, 3, "abcde");
        assert_eq!(s.text(), "abcde");
        assert_eq!(s.cursor(), (0, 4), "waits in the last column");
        let s = screen(5, 3, "abcdef");
        assert_eq!(s.text(), "abcde\nf");
        assert!(s.lines[0].wrapped);
        // A carriage return in the last column does not wrap.
        let s = screen(5, 3, "abcde\r\nx");
        assert_eq!(s.text(), "abcde\nx");
        assert!(!s.lines[0].wrapped);
    }

    #[test]
    fn lines_scroll_into_the_history() {
        let s = screen(4, 2, "1\r\n2\r\n3\r\n4");
        assert_eq!(s.text(), "3\n4");
        assert_eq!(s.scrollback_len(), 2);
        assert_eq!(s.line(0, 2).map(Line::text).as_deref(), Some("1"));
        assert_eq!(s.line(1, 1).map(Line::text).as_deref(), Some("3"));
        let mut s = Screen::new(4, 2, 1);
        Parser::new().advance(&mut s, b"1\r\n2\r\n3\r\n4");
        assert_eq!(s.scrollback_len(), 1, "the limit holds");
        assert_eq!(s.scrolled(), 2, "and every line that passed is counted");
    }

    #[test]
    fn cursor_movement_and_erasing() {
        let s = screen(10, 3, "hello\x1b[2;3Hxy\x1b[1;2H\x1b[K");
        assert_eq!(s.text(), "h\n  xy");
        let s = screen(10, 3, "abc\r\ndef\r\nghi\x1b[2;2H\x1b[J");
        assert_eq!(s.text(), "abc\nd");
        let s = screen(10, 3, "abc\r\ndef\r\nghi\x1b[2;2H\x1b[1J");
        assert_eq!(s.text(), "\n  f\nghi");
        let s = screen(10, 2, "abcdef\x1b[1;3H\x1b[2X");
        assert_eq!(s.text(), "ab  ef");
    }

    #[test]
    fn insert_and_delete_characters_and_lines() {
        let s = screen(6, 1, "abcdef\x1b[1;2H\x1b[2@");
        assert_eq!(s.text(), "a  bcd");
        let s = screen(6, 1, "abcdef\x1b[1;2H\x1b[2P");
        assert_eq!(s.text(), "adef");
        let s = screen(4, 4, "a\r\nb\r\nc\r\nd\x1b[2;1H\x1b[L");
        assert_eq!(s.text(), "a\n\nb\nc");
        let s = screen(4, 4, "a\r\nb\r\nc\r\nd\x1b[2;1H\x1b[2M");
        assert_eq!(s.text(), "a\nd");
    }

    #[test]
    fn a_scroll_region_keeps_the_lines_outside_it() {
        // Lines 2 and 3 scroll; 1 and 4 stay, and nothing reaches the history.
        let s = screen(4, 4, "top\r\nb\r\nc\r\nbot\x1b[2;3r\x1b[3;1H\nnew");
        assert_eq!(s.text(), "top\nc\nnew\nbot");
        assert_eq!(s.scrollback_len(), 0);
        let s = screen(4, 4, "top\r\nb\r\nc\r\nbot\x1b[2;3r\x1b[2;1H\x1bMup");
        assert_eq!(s.text(), "top\nup\nb\nbot");
    }

    #[test]
    fn colours_and_attributes() {
        let s = screen(
            10,
            1,
            "\x1b[1;31ma\x1b[38;5;200mb\x1b[38;2;1;2;3mc\x1b[48:2::4:5:6md\x1b[0me",
        );
        let cells = &s.lines[0].cells;
        assert_eq!(cells[0].style.fg, Color::Indexed(1));
        assert!(cells[0].style.attributes.bold);
        assert_eq!(cells[1].style.fg, Color::Indexed(200));
        assert_eq!(cells[2].style.fg, Color::Rgb(1, 2, 3));
        assert_eq!(cells[3].style.bg, Color::Rgb(4, 5, 6));
        assert_eq!(cells[4].style, Style::default());
        let s = screen(4, 1, "\x1b[94;101mx");
        assert_eq!(s.lines[0].cells[0].style.fg, Color::Indexed(12));
        assert_eq!(s.lines[0].cells[0].style.bg, Color::Indexed(9));
    }

    #[test]
    fn erasing_uses_the_current_background() {
        let s = screen(3, 1, "\x1b[44m\x1b[2K");
        assert_eq!(s.lines[0].cells[2].style.bg, Color::Indexed(4));
    }

    #[test]
    fn wide_characters_take_two_cells_and_wrap_whole() {
        let s = screen(5, 2, "a日本");
        assert_eq!(s.text(), "a日本");
        assert_eq!(s.lines[0].cells[2].width, 0);
        assert_eq!(s.cursor(), (0, 4));
        let s = screen(4, 2, "abc日");
        assert_eq!(s.text(), "abc\n日", "no room for both halves");
        // Overwriting the right half clears the left.
        let s = screen(4, 1, "日\x1b[1;2Hx");
        assert_eq!(s.text(), " x");
        // A combining mark joins the character before it.
        let s = screen(4, 1, "e\u{301}x");
        assert_eq!(s.text(), "e\u{301}x");
    }

    #[test]
    fn the_alternate_screen_keeps_the_primary_one() {
        let s = screen(6, 2, "shell\x1b[?1049hvim\x1b[?1049l");
        assert_eq!(s.text(), "shell");
        assert_eq!(s.cursor(), (0, 5), "the cursor is restored");
        let s = screen(6, 2, "shell\x1b[?1049hvim");
        assert!(s.is_alternate());
        assert_eq!(s.text(), "vim");
        // The alternate screen does not add to the history.
        let s = screen(6, 2, "\x1b[?1049h1\r\n2\r\n3");
        assert_eq!(s.scrollback_len(), 0);
    }

    #[test]
    fn status_requests_are_answered() {
        let mut s = screen(10, 5, "\x1b[3;4H\x1b[6n\x1b[5n\x1b[c");
        assert_eq!(
            String::from_utf8(s.take_responses()).unwrap(),
            "\x1b[3;4R\x1b[0n\x1b[?62;22c"
        );
        assert!(s.take_responses().is_empty());
    }

    #[test]
    fn modes_and_title() {
        let s = screen(10, 2, "\x1b[?1h\x1b[?2004h\x1b[?25l\x1b]0;my title\x07");
        let modes = s.modes();
        assert!(modes.application_cursor && modes.bracketed_paste);
        assert!(!modes.cursor_visible);
        assert_eq!(s.title(), "my title");
    }

    #[test]
    fn tabs_and_line_drawing() {
        let s = screen(20, 1, "a\tb");
        assert_eq!(s.text(), "a       b");
        let s = screen(10, 1, "\x1b(0lqk\x1b(Bq");
        assert_eq!(s.text(), "┌─┐q");
    }

    #[test]
    fn resizing_keeps_the_cursor_line_and_uses_the_history() {
        let mut s = screen(10, 4, "1\r\n2\r\n3\r\n4");
        s.resize(10, 2);
        assert_eq!(s.text(), "3\n4");
        assert_eq!(s.cursor(), (1, 1));
        s.resize(10, 4);
        assert_eq!(s.text(), "1\n2\n3\n4", "lines come back from the history");
        // Blank lines under the cursor go first.
        let mut s = screen(10, 4, "only");
        s.resize(5, 2);
        assert_eq!(s.text(), "only");
        assert_eq!(s.cursor(), (0, 4));
    }

    #[test]
    fn reset_and_repeat() {
        let s = screen(10, 2, "x\x1b[3b");
        assert_eq!(s.text(), "xxxx");
        let s = screen(10, 2, "\x1b[31mabc\x1bc");
        assert_eq!(s.text(), "");
        assert_eq!(s.cursor.style, Style::default());
    }
}
