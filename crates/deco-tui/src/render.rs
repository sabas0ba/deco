//! Turning a session into a grid of styled cells.
//!
//! Rendering is a pure function of the session plus the terminal size, and
//! produces a [`Frame`] rather than writing to the terminal. That split is what
//! lets the layout — gutter width, selection highlighting, tab expansion,
//! status bar — be asserted in CI with no terminal attached.

use deco_config::{LineNumbers, RenderWhitespace};
use deco_core::position::Range;
// The gutter width and the column division are the session's, not the renderer's:
// it needs the same answers to know how many columns are left for text, which is
// what decides where a wrapped line breaks. Two implementations would be free to
// disagree about the width, and a disagreement there draws a caret beside the
// character it is on rather than under it.
use deco_editor::find::Field;
use deco_editor::layout::{column_widths, gutter_width as gutter_width_of, Rect};
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
    whitespace_fg: Rgba,
    ruler_bg: Rgba,
    status_fg: Rgba,
    status_bg: Rgba,
    added_fg: Rgba,
    modified_fg: Rgba,
    deleted_fg: Rgba,
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
            // The git marks. A theme with nothing to say about them falls back
            // through `deco-theme`'s chain to its own diagnostic colours
            // before it ever reaches the line-number colour here.
            added_fg: theme.color("editorGutter.addedBackground").unwrap_or(fg),
            modified_fg: theme.color("editorGutter.modifiedBackground").unwrap_or(fg),
            deleted_fg: theme.color("editorGutter.deletedBackground").unwrap_or(fg),
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
            whitespace_fg: theme
                .color("editorWhitespace.foreground")
                .map(|c| c.over(bg))
                .unwrap_or(fg),
            // A ruler is a hairline between two columns in VS Code, and a terminal
            // has no space between cells to put one in. So it becomes a tint of the
            // cell instead, at a quarter strength: strong enough to follow down the
            // screen, weak enough to read the code sitting on it — which is the
            // column a ruler is there to warn about in the first place.
            ruler_bg: theme
                .color("editorRuler.foreground")
                .map(|c| Rgba { a: 0x40, ..c }.over(bg))
                .unwrap_or(bg),
            status_fg: theme.color("statusBar.foreground").unwrap_or(fg),
            status_bg: theme.color("statusBar.background").unwrap_or(bg),
        }
    }
}

/// How often `editor.lineNumbers: "interval"` draws a number.
///
/// Ten, as VS Code does. The setting names no interval of its own, so this is not a
/// number anybody can configure — in either editor.
const LINE_NUMBER_INTERVAL: usize = 10;

/// Number of columns the line-number gutter needs.
pub fn gutter_width(session: &Session) -> usize {
    gutter_width_of(&session.document)
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
/// bar's one or two rows when it is open.
///
/// The frontend needs this to tell the session how tall the text area is, so
/// exported rather than folded into the renderer.
pub fn chrome_height(session: &Session, height: usize) -> usize {
    fixed_chrome_height(session) + prompt_rows(session, height)
}

/// The rows of chrome whose count does not depend on the terminal's height: the
/// status bar, the find bar's one or two, the tab bar, and the prompt's own input
/// line. Everything here is a row the editor cannot do without while it is
/// showing — which is what makes the prompt's *list* the part that gives way when
/// the terminal is too short to hold all of it.
fn fixed_chrome_height(session: &Session) -> usize {
    let find = if session.find.visible() {
        1 + usize::from(session.find.replacing())
    } else {
        0
    };
    let prompt = usize::from(session.prompt.is_some());
    1 + find + prompt + tab_bar_height(session)
}

/// One row for the tab bar, or none while a single document is open.
///
/// Hidden for a single tab so that opening one file looks exactly as it always
/// did — the bar earns its row only once there is a choice to show.
pub fn tab_bar_height(session: &Session) -> usize {
    // Any group showing more than one tab earns the row, and the row spans the
    // window — so one group with two tabs makes the bar appear for all of them.
    usize::from(session.panes().iter().any(|pane| pane.tabs.len() > 1))
}

/// Rows the open prompt's list of choices takes.
///
/// Bounded three ways: by how many choices there are, so a prompt with two
/// matches costs two rows and not eight; by [`deco_editor::prompt::MAX_ROWS`],
/// because the file is what the user is trying to look at; and by the rows the
/// terminal actually has left after the chrome that cannot be shortened.
///
/// That third bound is not a nicety. Eight choices, an input line and a status
/// bar are ten rows, and a terminal can be five — at which point the frame was
/// twice the height of the window it was painted into, which on a real terminal
/// scrolls the screen and walks the whole editor upwards. The list is the part
/// that gives way because it is the only part that can: it is already a window
/// onto a longer list, and it already scrolls with the selection.
fn prompt_rows(session: &Session, height: usize) -> usize {
    let Some(prompt) = &session.prompt else {
        return 0;
    };
    let wanted = prompt.matches().min(deco_editor::prompt::MAX_ROWS);
    wanted.min(height.saturating_sub(fixed_chrome_height(session)))
}

fn render_text(session: &Session, width: usize, height: usize) -> Frame {
    let palette = Palette::from(session);
    let text_height = height.saturating_sub(chrome_height(session, height));
    // Screen rows the text area starts below: the tab bar, when it is showing.
    let top = tab_bar_height(session);

    let mut rows = Vec::with_capacity(height);
    if top > 0 {
        rows.push(tab_bar(session, &session.panes(), width, &palette));
    }

    // What is left of the text area once the chrome regions have taken theirs.
    // Computed from the height actually being drawn rather than from the one the
    // session was last resized to — see `Session::regions_for`.
    let regions = session.regions_for(width, text_height);
    let editor = regions.editor;

    let panes = session.panes();
    let widths = column_widths(editor.width, panes.len());
    let mut cursor_cell = None;
    let drawn: Vec<Frame> = panes
        .iter()
        .zip(&widths)
        .map(|(pane, column)| pane_rows(session, pane, *column, editor.height, &palette))
        .collect();

    // The caret belongs to one group, and its column is offset by everything to
    // the left of that group — including the separators, and now the side bar.
    let mut left = editor.x;
    for (index, frame) in drawn.iter().enumerate() {
        if let Some((x, y)) = frame.cursor {
            cursor_cell = Some((x + left as u16, y + top as u16));
        }
        left += widths[index] + usize::from(index + 1 < widths.len());
    }
    // A region with the keyboard means the text does not have it, and two
    // carets — or one in a place typing does not go — is a lie about where the
    // next keystroke lands.
    if session.focus() != deco_editor::Focus::Editor {
        cursor_cell = None;
    }

    let side_bar = regions
        .side_bar
        .map(|rect| region_rows(session, rect, Region::SideBar, &palette));
    let panel = regions
        .panel
        .map(|rect| region_rows(session, rect, Region::Panel, &palette));

    for row_index in 0..text_height {
        // The middle of the row: the groups, the rule above the panel, or the
        // panel itself, depending how far down we are.
        let middle = if let Some(rule) = regions.panel_rule.filter(|rule| *rule == row_index) {
            let _ = rule;
            Row {
                spans: vec![Span {
                    text: "─".repeat(editor.width),
                    fg: palette.gutter_fg,
                    bg: palette.bg,
                }],
            }
        } else if row_index < editor.height {
            stitch(&drawn, row_index, &palette)
        } else {
            let rect = regions.panel.expect("rows past the editor are the panel's");
            panel
                .as_ref()
                .and_then(|rows| rows.get(row_index - rect.y))
                .cloned()
                .unwrap_or_else(|| Row {
                    spans: vec![blank(editor.width, palette.bg)],
                })
        };

        rows.push(match (regions.side_bar, &side_bar) {
            (Some(rect), Some(bar)) => {
                let cells = bar.get(row_index).cloned().unwrap_or_else(|| Row {
                    spans: vec![blank(rect.width, palette.bg)],
                });
                // Where the panel's rule meets the side bar's, the two join
                // rather than crossing: a `│` butted against a run of `─` reads
                // as two borders that happen to touch.
                let joins = regions.panel_rule == Some(row_index);
                let rule = Span {
                    text: match (joins, rect.x == 0) {
                        (true, true) => "├",
                        (true, false) => "┤",
                        (false, _) => "│",
                    }
                    .to_owned(),
                    fg: palette.gutter_fg,
                    bg: palette.bg,
                };
                let mut spans = Vec::new();
                if rect.x == 0 {
                    spans.extend(cells.spans);
                    spans.push(rule);
                    spans.extend(middle.spans);
                } else {
                    spans.extend(middle.spans);
                    spans.push(rule);
                    spans.extend(cells.spans);
                }
                Row { spans }
            }
            _ => middle,
        });
    }

    // Between the text and the status bar, so that the bar the user is typing
    // into sits next to the text it is searching and never covers the place the
    // editor reports errors.
    if session.find.visible() {
        // The caret belongs in whichever input has the keyboard: the document's
        // cursor is on the current match, which is highlighted, and two visible
        // carets would be a lie about where typing goes.
        let focus = session.find.field();
        let (row, caret) = find_bar(session, width, &palette);
        rows.push(row);
        if focus == Field::Query {
            cursor_cell = Some((caret as u16, rows.len() as u16 - 1));
        }
        if session.find.replacing() {
            let (row, caret) = replace_bar(session, width, &palette);
            rows.push(row);
            if focus == Field::Replace {
                cursor_cell = Some((caret as u16, rows.len() as u16 - 1));
            }
        }
    }

    // Below the find bar, because the prompt is the thing that just opened and so
    // is the thing holding the keyboard.
    if let Some(prompt) = &session.prompt {
        // Only as many as `prompt_rows` said would fit — the same count the text
        // area was sized against, so the two cannot disagree about how tall the
        // frame is.
        let listed = prompt_rows(session, height);
        for (index, entry) in prompt.visible().iter().take(listed).enumerate() {
            rows.push(choice_row(
                entry,
                index == prompt.selected_row(),
                width,
                &palette,
            ));
        }
        let (row, caret) = prompt_row(prompt, width, &palette);
        rows.push(row);
        cursor_cell = Some((caret as u16, rows.len() as u16 - 1));
    }

    rows.push(status_bar(session, width, &palette));

    // The backstop, for a terminal shorter than the chrome that cannot be
    // shortened: two rows of find bar and a status bar do not fit in one row,
    // however little else is drawn. Rows go from the top, because what is at the
    // bottom is what has the keyboard — the input being typed into and the line
    // that reports what happened — and a frame taller than its window does not
    // merely lose a row, it scrolls the terminal and walks the editor off the
    // screen.
    if rows.len() > height {
        let excess = rows.len() - height;
        rows.drain(..excess);
        cursor_cell = cursor_cell.map(|(x, y)| (x, y.saturating_sub(excess as u16)));
    }

    Frame {
        rows,
        cursor: cursor_cell,
    }
}

/// Which region is being drawn, for the two things that differ between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    SideBar,
    Panel,
}

impl Region {
    /// The heading, in the case VS Code puts its view titles in.
    fn title(self, session: &Session) -> &'static str {
        match self {
            // Named for its tenant once it has one, as VS Code names the view
            // rather than the container it is in.
            Self::SideBar if session.side_bar_view() == deco_editor::SideBarView::SourceControl => {
                "SOURCE CONTROL"
            }
            Self::SideBar if session.explorer().is_some() => "EXPLORER",
            Self::SideBar => "SIDE BAR",
            Self::Panel => "PANEL",
        }
    }

    /// What is going to live here, for as long as nothing does.
    ///
    /// A region that opens empty and says nothing is indistinguishable from one
    /// that failed to draw. deco names commands it has not built rather than
    /// letting the key do nothing silently, and this is the same rule one layer
    /// out: the chrome is real, what goes in it is named, and neither is
    /// pretended about.
    fn waiting_on(self) -> &'static str {
        match self {
            Self::SideBar => "search and source control",
            Self::Panel => "the terminal, problems and output",
        }
    }

    /// Whether this region currently has the keyboard.
    fn has_focus(self, session: &Session) -> bool {
        match self {
            Self::SideBar => session.focus() == deco_editor::Focus::SideBar,
            Self::Panel => session.focus() == deco_editor::Focus::Panel,
        }
    }
}

/// One region's rows: a heading, then what it is waiting for, then blank.
///
/// Exactly `rect.height` rows of exactly `rect.width` columns, so the caller can
/// stitch them beside the editor's without measuring anything.
fn region_rows(session: &Session, rect: Rect, region: Region, palette: &Palette) -> Vec<Row> {
    let theme = &session.theme;
    // The side bar's own colours when the theme has them, so a region looks like
    // part of the editor it was themed for rather than a hole in it.
    let bg = theme
        .color("sideBar.background")
        .unwrap_or(palette.status_bg);
    let fg = theme
        .color("sideBar.foreground")
        .unwrap_or(palette.status_fg);
    let title_fg = theme.color("sideBarTitle.foreground").unwrap_or(fg);
    // The one thing that has to be visible without a tenant to show it: which
    // region the keyboard is in.
    let title_bg = if region.has_focus(session) {
        theme
            .color("focusBorder")
            .unwrap_or(palette.gutter_active_fg)
    } else {
        bg
    };

    let mut rows: Vec<Row> = Vec::with_capacity(rect.height);
    rows.push(region_line(
        region.title(session),
        rect.width,
        title_fg,
        title_bg,
    ));
    if rect.height > 1 {
        rows.push(region_line("", rect.width, fg, bg));
    }

    // The side bar has two tenants now; the panel has none.
    if region == Region::SideBar
        && session.side_bar_view() == deco_editor::SideBarView::SourceControl
    {
        scm_rows(session, rect, &mut rows, palette, fg, bg);
        while rows.len() < rect.height {
            rows.push(region_line("", rect.width, fg, bg));
        }
        rows.truncate(rect.height);
        return rows;
    }
    if region == Region::SideBar {
        if let Some(explorer) = session.explorer() {
            tree_rows(session, explorer, rect, &mut rows, palette, fg, bg);
            while rows.len() < rect.height {
                rows.push(region_line("", rect.width, fg, bg));
            }
            rows.truncate(rect.height);
            return rows;
        }
    }

    // Wrapped, because what a region is waiting for does not fit in thirty
    // columns and a region that ends mid-word reads as broken.
    let mut body = wrap(region.waiting_on(), rect.width.saturating_sub(1), 4);
    body.push("will live here".to_owned());
    for text in body {
        if rows.len() >= rect.height {
            break;
        }
        rows.push(region_line(&text, rect.width, fg, bg));
    }
    while rows.len() < rect.height {
        rows.push(region_line("", rect.width, fg, bg));
    }
    rows.truncate(rect.height);
    rows
}

/// The source-control view: a heading per group, then its files.
///
/// The headings are drawn from the rows rather than being rows themselves, so
/// the selection can never land on one — which is what keeps `git.stage` from
/// ever being asked to stage a word.
fn scm_rows(
    session: &Session,
    rect: Rect,
    rows: &mut Vec<Row>,
    palette: &Palette,
    fg: Rgba,
    bg: Rgba,
) {
    let view = session.source_control();
    if view.is_empty() {
        // Told apart from "no repository" by the status bar beside it, which
        // shows a branch in one case and nothing in the other.
        let message = match session.scm_status() {
            Some(_) => "no changes",
            None => "not a git repository",
        };
        rows.push(region_line(message, rect.width, dim(fg, bg), bg));
        return;
    }

    let focused = session.focus() == deco_editor::Focus::SideBar
        && session.side_bar_view() == deco_editor::SideBarView::SourceControl;
    let selected_at = view.selected_index();

    // Every line the list would draw, headings included, and then the window
    // the model has scrolled to. Built whole rather than drawn straight into
    // `rows`, because the scroll is counted in *lines* — a heading takes one —
    // and slicing at the end is the only way the two agree about where the
    // selection is.
    // The group and whether a line is its heading are retained until the
    // window is chosen. When a long group scrolls, its real heading can be
    // above the window; that metadata lets it be repeated at the top rather
    // than leaving rows whose stage/unstage meaning is no longer visible.
    let mut lines: Vec<(deco_editor::ScmGroup, bool, Row)> = Vec::new();
    let mut selected_line = 0;
    let mut group = None;
    for (index, row) in view.rows().iter().enumerate() {
        if group != Some(row.group) {
            group = Some(row.group);
            let count = view
                .rows()
                .iter()
                .filter(|other| other.group == row.group)
                .count();
            lines.push((
                row.group,
                true,
                region_line(
                    &format!("{} {count}", row.group.title()),
                    rect.width,
                    dim(fg, bg),
                    bg,
                ),
            ));
        }

        // `M src/main.rs` — the letter, then the name, then the directory in
        // the dimmer colour when there is room. VS Code puts the letter on the
        // right; here it is on the left, because a column that moves with the
        // name length is not a column you can read down.
        let name = row.name();
        let directory = row.directory().unwrap_or_default();
        let left = format!(" {} {name}", row.letter());
        let room = rect.width.saturating_sub(columns(&left) + 1);
        let tail = if directory.is_empty() || room < 3 {
            String::new()
        } else {
            format!(" {}", truncate_to(&directory, room))
        };

        let chosen = index == selected_at;
        if chosen {
            selected_line = lines.len();
        }
        let (row_fg, row_bg) = match (chosen, focused) {
            // The selection is drawn as the tree's is: inverted when the view
            // has the keyboard, and merely marked when it does not, so a
            // glance says where typing would go.
            (true, true) => (bg, palette.status_fg),
            (true, false) => (palette.status_fg, bg),
            (false, _) => (fg, bg),
        };
        let mut text = format!("{left}{tail}");
        text = truncate_to(&text, rect.width);
        while columns(&text) < rect.width {
            text.push(' ');
        }
        lines.push((
            row.group,
            false,
            Row {
                spans: vec![Span {
                    text,
                    fg: row_fg,
                    bg: row_bg,
                }],
            },
        ));
    }

    // The title and spacer are already in rows. Use only what remains, which
    // is the same height the session gave scroll_into_view.
    let list_height = rect.height.saturating_sub(rows.len());
    let mut start = view.scroll().min(lines.len().saturating_sub(1));
    let mut sticky = None;
    let available_below_sticky = list_height.saturating_sub(1);
    if available_below_sticky > 0 && lines.get(start).is_some_and(|(_, heading, _)| !*heading) {
        // Repeating the heading costs a row. Shift the content window when
        // necessary so the selected file remains visible at either edge.
        if selected_line < start {
            start = selected_line;
        } else if selected_line >= start + available_below_sticky {
            start = selected_line + 1 - available_below_sticky;
        }
        if let Some((group, heading, _)) = lines.get(start) {
            if !*heading {
                sticky = Some(*group);
            }
        }
    }

    if let Some(group) = sticky {
        let count = view.rows().iter().filter(|row| row.group == group).count();
        rows.push(region_line(
            &format!("{} {count}", group.title()),
            rect.width,
            dim(fg, bg),
            bg,
        ));
        rows.extend(
            lines
                .into_iter()
                .skip(start)
                .take(available_below_sticky)
                .map(|(_, _, row)| row),
        );
    } else {
        rows.extend(
            lines
                .into_iter()
                .skip(start)
                .take(list_height)
                .map(|(_, _, row)| row),
        );
    }
}

/// A quieter version of `fg` against `bg`, for headings and second columns.
fn dim(fg: Rgba, bg: Rgba) -> Rgba {
    Rgba {
        r: ((fg.r as u16 + bg.r as u16) / 2) as u8,
        g: ((fg.g as u16 + bg.g as u16) / 2) as u8,
        b: ((fg.b as u16 + bg.b as u16) / 2) as u8,
        a: fg.a,
    }
}

/// The file tree's rows, indented, with the selection highlighted.
///
/// A name too long for the side bar is cut with an ellipsis rather than wrapped:
/// a file name that ran onto a second line would look like two files, and the
/// tree's whole job is to be countable at a glance.
#[allow(clippy::too_many_arguments)]
fn tree_rows(
    session: &Session,
    explorer: &deco_editor::Explorer,
    rect: Rect,
    rows: &mut Vec<Row>,
    palette: &Palette,
    fg: Rgba,
    bg: Rgba,
) {
    let theme = &session.theme;
    let focused = session.focus() == deco_editor::Focus::SideBar;
    let selected_bg = theme
        .color(if focused {
            "list.activeSelectionBackground"
        } else {
            "list.inactiveSelectionBackground"
        })
        .unwrap_or(palette.selection_bg);
    let selected_fg = theme
        .color(if focused {
            "list.activeSelectionForeground"
        } else {
            "list.inactiveSelectionForeground"
        })
        .unwrap_or(fg);

    let height = rect.height.saturating_sub(rows.len());
    if height == 0 {
        return;
    }

    // Two different silences: a workspace nobody has read yet, and one that is
    // genuinely empty. Both would otherwise be a blank panel that reads as a
    // failure to draw.
    if !explorer.loaded() {
        rows.push(region_line("reading the workspace…", rect.width, fg, bg));
        return;
    }
    if explorer.is_empty() {
        rows.push(region_line("this workspace is empty", rect.width, fg, bg));
        return;
    }

    for row in explorer.visible(height) {
        // A chevron for a directory, two spaces for a file, so names at one
        // level start in the same column whatever their kind.
        let marker = match (row.is_dir, row.expanded) {
            (true, true) => "▾ ",
            (true, false) => "▸ ",
            (false, _) => "  ",
        };
        let indent = "  ".repeat(row.depth);
        let prefix = format!("{indent}{marker}");
        let room = rect.width.saturating_sub(prefix.chars().count() + 1);
        let text = format!("{prefix}{}", ellipsise(&row.name, room));
        let (row_fg, row_bg) = if row.selected {
            (selected_fg, selected_bg)
        } else {
            (fg, bg)
        };
        rows.push(region_line(&text, rect.width, row_fg, row_bg));
    }
}

/// `text` cut to `width` columns, with an ellipsis when it was cut.
fn ellipsise(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    text.chars().take(width - 1).collect::<String>() + "…"
}

/// One row of a region: a column of padding, the text, and blank to the edge.
fn region_line(text: &str, width: usize, fg: Rgba, bg: Rgba) -> Row {
    let clipped = clip(text, width.saturating_sub(1));
    let used = columns(&clipped);
    let mut spans = vec![Span {
        text: format!(" {clipped}"),
        fg,
        bg,
    }];
    if used + 1 < width {
        spans.push(blank(width - used - 1, bg));
    }
    Row { spans }
}

/// `text` cut to `limit` columns.
fn clip(text: &str, limit: usize) -> String {
    if columns(text) <= limit {
        return text.to_owned();
    }
    text.chars().take(limit).collect()
}

/// Joins one row of every group into the row that goes on screen.
///
/// The separator is a full-height rule in the gutter's colour: something has to
/// mark where one file ends and the next begins, and a blank column reads as part
/// of whichever file has short lines.
fn stitch(frames: &[Frame], row_index: usize, palette: &Palette) -> Row {
    let mut spans = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        if index > 0 {
            spans.push(Span {
                text: "│".to_owned(),
                fg: palette.gutter_fg,
                bg: palette.bg,
            });
        }
        match frame.rows.get(row_index) {
            Some(row) => spans.extend(row.spans.iter().cloned()),
            // A group with fewer rows than another cannot happen — they are all
            // asked for the same height — but leaving a hole would be worse than
            // padding it.
            None => spans.push(blank(0, palette.bg)),
        }
    }
    Row { spans }
}

/// One group's rows, and where its caret is within them.
///
/// The caret's row is relative to the first text row rather than to the screen,
/// since where the text area starts is the caller's business.
fn pane_rows(
    session: &Session,
    pane: &deco_editor::Pane<'_>,
    width: usize,
    height: usize,
    palette: &Palette,
) -> Frame {
    let gutter = gutter_width_of(pane.document);
    let text_width = width.saturating_sub(gutter);
    let buffer = &pane.document.buffer;
    let tab_size = pane.document.settings.tab_size;
    let caret = pane.view.cursor();
    let cursor_line = caret.line as usize;

    // One entry per row on screen rather than one per line: a wrapped line
    // occupies several, and which part of it each row shows is the view's answer
    // and not the renderer's — the same answer the view scrolls and moves the
    // caret by.
    let visible = pane.view.visible_rows(buffer, &pane.document.settings);

    let mut rows = Vec::with_capacity(height);
    let mut cursor = None;
    for row_index in 0..height {
        let Some(visual) = visible.get(row_index) else {
            rows.push(Row {
                spans: vec![blank(width, palette.bg)],
            });
            continue;
        };
        let line = visual.line;

        let mut spans = Vec::new();
        if gutter > 0 {
            // Blank on a continuation row: repeating the number would read as a
            // second line that is not there, and VS Code leaves it blank too.
            let label = if visual.numbered() {
                match pane.document.settings.line_numbers {
                    LineNumbers::Relative if line != cursor_line => (line as i64
                        - cursor_line as i64)
                        .unsigned_abs()
                        .to_string(),
                    // Every tenth, plus the line the caret is on — which is the one
                    // you are about to quote in a stack trace, and the only line
                    // worth an exception to the interval.
                    LineNumbers::Interval
                        if (line + 1) % LINE_NUMBER_INTERVAL != 0 && line != cursor_line =>
                    {
                        String::new()
                    }
                    _ => (line + 1).to_string(),
                }
            } else {
                String::new()
            };
            let number_fg = if line == cursor_line {
                palette.gutter_active_fg
            } else {
                palette.gutter_fg
            };
            // The column between the numbers and the text is where VS Code
            // puts its git marks, and it is already spare here. On a
            // continuation row it stays blank: a wrapped line is one line, and
            // repeating its mark would read as several.
            let mark = visual
                .numbered()
                .then(|| git_mark(session, pane, line, palette))
                .flatten();
            match mark {
                // Two spans, because the mark is a different colour from the
                // number beside it.
                Some((glyph, fg)) => {
                    spans.push(Span {
                        text: format!("{label:>width$}", width = gutter - 1),
                        fg: number_fg,
                        bg: palette.bg,
                    });
                    spans.push(Span {
                        text: glyph.to_string(),
                        fg,
                        bg: palette.bg,
                    });
                }
                // One, as it was before there were marks. Worth the branch:
                // most lines of most files have nothing beside them, and a
                // span per row that only ever holds a space is a span per row.
                None => spans.push(Span {
                    text: format!("{label:>width$} ", width = gutter - 1),
                    fg: number_fg,
                    bg: palette.bg,
                }),
            }
        }

        let text = buffer
            .line_content(line)
            .map(|s| s.to_string())
            .unwrap_or_default();
        spans.extend(line_spans(
            session, pane, &text, visual, text_width, tab_size, palette,
        ));
        rows.push(Row { spans });

        // Only the group with the keyboard draws one: two carets would be a lie
        // about where typing goes.
        if pane.focused && line == cursor_line && visual.holds(caret.character) {
            // Measured from the row's own start, because that is where this row's
            // tab stops are counted from — the same measurement the wrap used to
            // decide the row ends here.
            let column = visual.indent
                + deco_core::wrap::width_between_from(
                    &text,
                    visual.start,
                    caret.character,
                    tab_size,
                    visual.indent,
                );
            if column < text_width {
                cursor = Some(((gutter + column) as u16, row_index as u16));
            }
        }
    }
    Frame { rows, cursor }
}

/// The tab bar: every open document, the active one set apart.
fn tab_bar(
    session: &Session,
    panes: &[deco_editor::Pane<'_>],
    width: usize,
    palette: &Palette,
) -> Row {
    let theme = &session.theme;
    let bar_bg = theme
        .color("editorGroupHeader.tabsBackground")
        .unwrap_or(palette.status_bg);
    let active_bg = theme.color("tab.activeBackground").unwrap_or(palette.bg);
    let active_fg = theme.color("tab.activeForeground").unwrap_or(palette.fg);
    let inactive_bg = theme.color("tab.inactiveBackground").unwrap_or(bar_bg);
    let inactive_fg = theme
        .color("tab.inactiveForeground")
        .unwrap_or(palette.status_fg);

    let mut spans = Vec::new();
    let mut used = 0usize;
    for label in panes.iter().flat_map(|pane| pane.tabs.iter()) {
        // The same marker the status bar uses for the active document, so the
        // two read as one vocabulary.
        let dirty = if label.dirty { "*" } else { "" };
        let text = format!(" {}{dirty} ", label.title);
        let cells = columns(&text);
        if used + cells > width {
            // Out of room. The bar truncates rather than scrolling; with the
            // active tab always reachable by ctrl+tab, a scrolling bar is not
            // worth its complexity yet.
            break;
        }
        let (fg, bg) = if label.active {
            (active_fg, active_bg)
        } else {
            (inactive_fg, inactive_bg)
        };
        spans.push(Span { text, fg, bg });
        used += cells;
    }
    if used < width {
        spans.push(Span {
            text: " ".repeat(width - used),
            fg: inactive_fg,
            bg: bar_bg,
        });
    }
    Row { spans }
}

/// The prompt's own row, and the column its caret sits in.
fn prompt_row(prompt: &deco_editor::Prompt, width: usize, palette: &Palette) -> (Row, usize) {
    let label = format!(" {} ", prompt.kind().label());
    // The match count, for a prompt that has a list to count. A go-to-line box
    // has nothing to say here.
    let right = if prompt.has_list() {
        let count = prompt.matches();
        let noun = prompt.kind().noun(count);
        match count {
            0 => format!("No {noun} "),
            count => format!("{count} {noun} "),
        }
    } else {
        String::new()
    };
    input_row(
        &label,
        prompt.text(),
        prompt.caret(),
        &right,
        width,
        prompt.text_selected(),
        palette,
    )
}

/// One offered choice: its title, and the key bound to it on the right.
fn choice_row(
    entry: &deco_editor::commands::PaletteEntry,
    selected: bool,
    width: usize,
    palette: &Palette,
) -> Row {
    let left = format!("  {} ", entry.title);
    // The second column, for the entries whose title does not say everything —
    // see `PaletteEntry::detail`. An entry without one gets no column at all,
    // rather than `/home/you/src/main.rs` repeated beside `src/main.rs:2: …`.
    let right = match &entry.detail {
        Some(detail) => format!(" {detail} "),
        None => String::new(),
    };
    let room = width.saturating_sub(columns(&left));
    let right = if room >= columns(&right) + 2 {
        truncate_to(&right, room)
    } else {
        String::new()
    };

    let used = columns(&left) + columns(&right);
    let mut text = format!("{left}{}{right}", " ".repeat(width.saturating_sub(used)));
    text = truncate_to(&text, width);
    while columns(&text) < width {
        text.push(' ');
    }

    // The selected row is drawn as the status bar is, inverted against the
    // editor's own colours, which is how the completion list marks its selection.
    let (fg, bg) = if selected {
        (palette.bg, palette.status_fg)
    } else {
        (palette.status_fg, palette.status_bg)
    };
    Row {
        spans: vec![Span { text, fg, bg }],
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

    let caret = match find.field() {
        Field::Query => find.caret(),
        // The unfocused input still has to be drawn, and its caret is not
        // visible, so the end of the text is as good a window as any.
        Field::Replace => find.query().chars().count(),
    };
    let selected = find.field() == Field::Query && find.text_selected();
    input_row(
        PROMPT,
        find.query(),
        caret,
        &right,
        width,
        selected,
        palette,
    )
}

/// The replacement input, and the column its caret sits in.
///
/// The same width of prompt as the query's, so the two inputs line up and read
/// as one sentence: `Find: foo` / `With: bar`.
fn replace_bar(session: &Session, width: usize, palette: &Palette) -> (Row, usize) {
    const PROMPT: &str = " With: ";

    let find = &session.find;
    let caret = match find.field() {
        Field::Replace => find.caret(),
        Field::Query => find.replace().chars().count(),
    };
    let selected = find.field() == Field::Replace && find.text_selected();
    input_row(PROMPT, find.replace(), caret, "", width, selected, palette)
}

/// One row of the bar: a prompt, an editable field, and a right-aligned readout.
///
/// `selected` draws the field inverted, the way the chosen row of a list is
/// drawn. A field whose next keystroke replaces everything in it has to look
/// different from one that appends, or the difference is something the user only
/// discovers by losing what they typed.
fn input_row(
    prompt: &str,
    value: &str,
    caret: usize,
    right: &str,
    width: usize,
    selected: bool,
    palette: &Palette,
) -> (Row, usize) {
    let room = width
        .saturating_sub(columns(prompt))
        .saturating_sub(columns(right));
    let field = visible_query(value, caret, room);

    let used = columns(prompt) + columns(&field.text) + columns(right);
    let mut text = format!(
        "{prompt}{}{}{right}",
        field.text,
        " ".repeat(width.saturating_sub(used))
    );
    text = truncate_to(&text, width);
    while columns(&text) < width {
        text.push(' ');
    }

    let caret_column = (columns(prompt) + field.caret_column).min(width.saturating_sub(1));
    (
        Row {
            spans: highlighted(&text, prompt, &field.text, selected, palette),
        },
        caret_column,
    )
}

/// The row's spans, with the field inverted when it is selected.
///
/// One span unless there is a selection to show, and one span again if
/// truncation ate the field the offsets were computed from: a highlight drawn
/// over the wrong bytes would be worse than none, and a narrow terminal is the
/// case where it is least useful anyway.
fn highlighted(
    text: &str,
    prompt: &str,
    field: &str,
    selected: bool,
    palette: &Palette,
) -> Vec<Span> {
    let plain = |text: &str| Span {
        text: text.to_owned(),
        fg: palette.status_fg,
        bg: palette.status_bg,
    };
    let end = prompt.len() + field.len();
    if !selected || field.is_empty() || text.len() < end || !text.starts_with(prompt) {
        return vec![plain(text)];
    }
    vec![
        plain(prompt),
        Span {
            text: field.to_owned(),
            // Inverted against the editor's own colours, as the chosen row of a
            // list and the status bar are.
            fg: palette.bg,
            bg: palette.status_fg,
        },
        plain(&text[end..]),
    ]
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

/// Builds the styled spans for one row of one line.
///
/// `text` is the whole document line and `visual` says which part of it this row
/// shows, so highlighting, selections and matches are still looked up by their
/// column in the line — a wrapped row is a window onto the line, not a line of
/// its own.
fn line_spans(
    session: &Session,
    pane: &deco_editor::Pane<'_>,
    text: &str,
    visual: &deco_editor::document::VisualRow,
    width: usize,
    tab_size: usize,
    palette: &Palette,
) -> Vec<Span> {
    let line = visual.line;
    // Expand tabs first: the terminal has no tab stops of its own once we are
    // positioning the cursor by column. `column` counts *terminal columns*, not
    // characters — a CJK character occupies two, so padding by character count
    // would push every row past the right edge.
    let mut cells: Vec<(char, Cell, Rgba)> = Vec::new();
    // The blank a continuation row is pushed in by. Part of the cell run rather than
    // a span of its own, so a selection or a ruler crossing the indent still draws:
    // the columns are real, they simply hold nothing.
    let mut column = 0usize;
    while column < visual.indent.min(width) {
        cells.push((' ', Cell::Plain, palette.fg));
        column += 1;
    }
    let mut utf16 = 0u32;

    // The scope stack the theme resolves against: the language's `source.*` scope,
    // then the token's own. Two elements, so the theme's parent selectors work.
    let source = pane.document.syntax.source_scope();
    let highlights = pane.document.syntax.spans(&pane.document.buffer, line);

    let selected_ranges = clipped_to_line(
        pane.view
            .selections
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| Range::new(s.start(), s.end())),
        line,
    );
    // Empty whenever the find bar is closed, which is what makes this free for
    // everyone not searching — and empty for an unfocused group, whose document
    // the query was never run against.
    let match_ranges = if pane.focused {
        clipped_to_line(session.find.matches().iter().copied(), line)
    } else {
        Vec::new()
    };

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

    // The server's classification of this line, when semantic highlighting is on.
    // Kept separate from the lexer's spans rather than merged into them: the two
    // disagree about where a run begins as often as they agree, and the finer
    // answer wins per cell rather than per run.
    let semantic: Vec<&deco_lsp::requests::SemanticSpan> = if semantic_highlighting(session) {
        pane.semantic
            .iter()
            .filter(|span| span.range.start.line == line as u32)
            .collect()
    } else {
        Vec::new()
    };

    // Foreground colour per UTF-16 offset, from the highlighting.
    let colour_at = |utf16: u32| -> Rgba {
        // The server first. It knows what a lexer cannot — that this `Foo` is a
        // type and that one is a variable — and a theme that styles the token type
        // has said which it prefers by styling it at all.
        if let Some(span) = semantic
            .iter()
            .find(|span| utf16 >= span.range.start.character && utf16 < span.range.end.character)
        {
            let modifiers: Vec<&str> = span.modifiers.iter().map(String::as_str).collect();
            let token = deco_theme::semantic::SemanticToken::new(
                &span.token_type,
                &modifiers,
                pane.document.language(),
            );
            if let Some(colour) = session
                .theme
                .style_for_semantic(&token)
                .and_then(|style| style.foreground)
            {
                return colour;
            }
            // The theme has no rule for this token type, so fall through to the
            // lexer rather than to the plain foreground: losing the keyword colour
            // because a server also called it a keyword would be a regression.
        }

        let Some(span) = highlights
            .iter()
            .find(|span| utf16 >= span.start && utf16 < span.end)
        else {
            return palette.fg;
        };
        let stack: Vec<&str> = match source {
            Some(source) => vec![source, span.scope],
            None => vec![span.scope],
        };
        session
            .theme
            .style_for_scopes(&stack)
            .foreground
            .unwrap_or(palette.fg)
    };

    // Where this line's trailing whitespace begins, for
    // `editor.renderWhitespace: "trailing"`. A line of nothing but whitespace is
    // trailing from its first character, which `trim_end` gives for free.
    let trailing_from: u32 = utf16_len(text.trim_end());
    let whitespace_mode = pane.document.settings.render_whitespace;
    // Indexed, because `boundary` mode has to see the characters either side.
    let chars: Vec<char> = text.chars().collect();

    for (index, &c) in chars.iter().enumerate() {
        // Everything before this row belongs to the row above, but its columns
        // still have to be counted: the lookups below are by column in the line.
        if utf16 < visual.start {
            utf16 += c.len_utf16() as u32;
            continue;
        }
        // And everything from the next row's start belongs to it.
        if visual.end.is_some_and(|end| utf16 >= end) {
            break;
        }
        let cell = cell_at(utf16);
        let fg = colour_at(utf16);
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
        let marked = marks_whitespace(
            whitespace_mode,
            &chars,
            index,
            utf16,
            trailing_from,
            cell != Cell::Plain,
        );
        if c == '\t' {
            // The arrow at the start of the tab's span and spaces after it, so the
            // glyph sits where the tab does rather than where it lands.
            for offset in 0..advance {
                let glyph = if marked && offset == 0 { '→' } else { ' ' };
                cells.push((glyph, cell, if marked { palette.whitespace_fg } else { fg }));
            }
        } else if marked && c == ' ' {
            cells.push(('·', cell, palette.whitespace_fg));
        } else if let Some(picture) = control_picture(c) {
            // `editor.renderControlCharacters` picks the glyph or a blank. Either way
            // the byte itself does not reach the terminal — see `control_picture`.
            let shown = if pane.document.settings.render_control_characters {
                (picture, palette.whitespace_fg)
            } else {
                (' ', fg)
            };
            cells.push((shown.0, cell, shown.1));
        } else {
            cells.push((c, cell, fg));
        }
        column += advance;
        utf16 += c.len_utf16() as u32;
    }

    // A selection that runs past the end of the line is drawn one cell wide, so
    // that selecting a line break is visible rather than invisible. Only on the
    // line's last row: there is one line break, and it is at the end.
    let trailing = cell_at(utf16);
    if visual.end.is_none() && trailing != Cell::Plain && column < width {
        cells.push((' ', trailing, palette.fg));
        column += 1;
    }
    while column < width {
        cells.push((' ', Cell::Plain, palette.fg));
        column += 1;
    }

    // Coalesce runs sharing a style; one span per character would be correct
    // but would make the terminal writer do far more work than it needs to.
    let rulers = &pane.document.settings.rulers;
    let mut spans: Vec<Span> = Vec::new();
    for (at, (c, cell, fg)) in cells.into_iter().enumerate() {
        let bg = match cell {
            // A ruler only shows through a cell nothing else has claimed: a
            // selection or a find match is what the user is doing, and it wins.
            Cell::Plain if rulers.contains(&at) => palette.ruler_bg,
            Cell::Plain => palette.bg,
            Cell::Selected => palette.selection_bg,
            Cell::CurrentMatch => palette.find_match_bg,
            Cell::OtherMatch => palette.find_highlight_bg,
        };
        match spans.last_mut() {
            // Coalesced on both colours now: a run of one style is one span, and
            // highlighting breaks runs far more often than a selection does.
            Some(last) if last.bg == bg && last.fg == fg => last.text.push(c),
            _ => spans.push(Span {
                text: c.to_string(),
                fg,
                bg,
            }),
        }
    }
    spans
}

/// A printable stand-in for a control character.
///
/// # Why this is not a cosmetic setting
///
/// deco draws into a terminal, and a terminal *interprets* these bytes. A document
/// containing `\x1b[31m` would recolour everything after it; `\x07` rings the bell; and
/// `\x1b]52;c;…\x07` is OSC 52, which **writes the clipboard** on every terminal that
/// supports it. Passing a document's bytes through to the terminal would make "open
/// this file" mean "let this file talk to your terminal", so nothing here is ever
/// emitted as-is.
///
/// The Unicode Control Pictures block is the stand-in: `␛` for escape, `␇` for bell,
/// one column each, so the substitution changes no column anybody counted.
///
/// `editor.renderControlCharacters` chooses between showing that glyph and showing a
/// blank. It cannot choose to send the byte: that is not a rendering option, it is a
/// way of handing the terminal to whoever wrote the file.
fn control_picture(c: char) -> Option<char> {
    match c {
        // Tab is expanded to spaces before this, and a line's content never contains
        // its own line break.
        '\t' | '\n' | '\r' => None,
        // `␀` through `␟`, in order, at U+2400.
        '\0'..='\u{1f}' => char::from_u32(0x2400 + c as u32),
        // Delete, which sits on its own at U+2421.
        '\u{7f}' => Some('␡'),
        _ => None,
    }
}

/// Every control character in `text` replaced by its picture.
///
/// The last line of defence, applied by the painter to everything it writes rather
/// than to the document alone: a file *name* with an escape byte in it reaches the tab
/// bar, and a search result carries a line of somebody else's file into a prompt row.
/// One column in, one column out, so no layout depends on which path the text took.
pub fn sanitise(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(|c| control_picture(c).is_some()) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(
        text.chars()
            .map(|c| control_picture(c).unwrap_or(c))
            .collect(),
    )
}

/// A string's length in UTF-16 code units, which is what a column is measured in.
fn utf16_len(text: &str) -> u32 {
    text.chars().map(|c| c.len_utf16() as u32).sum()
}

/// Whether `editor.renderWhitespace` marks the whitespace at `index`.
///
/// VS Code's five modes. `selection` is its default and the least intrusive useful
/// one: whitespace appears exactly where you are looking at it.
fn marks_whitespace(
    mode: RenderWhitespace,
    chars: &[char],
    index: usize,
    column: u32,
    trailing_from: u32,
    selected: bool,
) -> bool {
    if !matches!(chars.get(index), Some(' ') | Some('\t')) {
        return false;
    }
    match mode {
        RenderWhitespace::None => false,
        RenderWhitespace::All => true,
        RenderWhitespace::Selection => selected,
        RenderWhitespace::Trailing => column >= trailing_from,
        // Everything except a single space with a word on each side. Marking those
        // would put a dot between every word of a sentence, which is the reason this
        // mode exists rather than being the same as `all`.
        RenderWhitespace::Boundary => {
            chars[index] == '\t' || !single_space_between_words(chars, index)
        }
    }
}

/// Whether the space at `index` is a lone one between two non-space characters.
fn single_space_between_words(chars: &[char], index: usize) -> bool {
    let solid = |at: Option<&char>| at.is_some_and(|c| !c.is_whitespace());
    solid(index.checked_sub(1).and_then(|before| chars.get(before))) && solid(chars.get(index + 1))
}

/// Whether the language server's classification is used.
///
/// `editor.semanticHighlighting.enabled` is VS Code's setting and takes three
/// values: `true`, `false`, and `"configuredByTheme"` — the default — which defers
/// to the theme's own `semanticHighlighting` flag. Deferring is the right default
/// because a theme written without semantic rules looks *worse* with them applied:
/// every token the theme has no rule for falls back inconsistently.
fn semantic_highlighting(session: &Session) -> bool {
    if let Some(enabled) = session
        .settings
        .get_bool("editor.semanticHighlighting.enabled", None)
    {
        return enabled;
    }
    // Absent, or any string — including `configuredByTheme`. A misspelled value
    // therefore behaves as the default rather than as `false`, which is the kinder
    // failure.
    session.theme.semantic_highlighting()
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

/// What the status bar says about indentation.
///
/// VS Code shows `Spaces: 4` here, and it earns its place for the same reason: what
/// one press of `tab` inserts is not visible from the text, and it is what decides
/// whether a diff is one line or forty.
///
/// `(detected)` is added only when the file's own indentation **differed** from the
/// settings and won. A two-space file read as two-space where `editor.tabSize`
/// already said two overrode nothing, and marking that would be a permanent note
/// about a case with nothing to disclose.
fn indentation(session: &Session) -> String {
    let settings = &session.document.settings;
    let unit = if settings.insert_spaces {
        format!("Spaces: {}", settings.tab_size)
    } else {
        format!("Tab: {}", settings.tab_size)
    };
    if session.document.indentation_overridden {
        format!("{unit} (detected)")
    } else {
        unit
    }
}

/// What to draw in the gutter's mark column for `line`, if anything.
///
/// Shape as well as colour. VS Code separates *added* from *modified* by
/// colour alone, which is a distinction anyone who cannot tell its green from
/// its blue does not get; a heavy bar against a light one costs nothing and
/// carries the same information without it.
///
/// - `┃` — lines that are not in the committed file at all.
/// - `│` — lines that are there and say something else.
/// - `▔` — lines were removed just above this one. It sits on the cell's top
///   edge because that is where they were; there is nothing left to draw
///   beside.
fn git_mark(
    session: &Session,
    pane: &deco_editor::Pane<'_>,
    line: usize,
    palette: &Palette,
) -> Option<(char, Rgba)> {
    let path = pane.document.path.as_deref()?;
    let mark = session.diff_marks(path)?.mark_at(line)?;
    Some(match mark {
        deco_scm::Mark::Added => ('┃', palette.added_fg),
        deco_scm::Mark::Modified => ('│', palette.modified_fg),
        deco_scm::Mark::Deleted => ('▔', palette.deleted_fg),
    })
}

/// The branch and how far the working tree has drifted from it.
///
/// Empty when nobody has run `git status` yet, when there is no git, or when
/// the folder is not a repository — the three are indistinguishable from here
/// and all three mean the same thing on screen, which is nothing. VS Code does
/// the same: no repository, no segment.
///
/// The text itself is `deco_scm::Status::summary` rather than assembled here:
/// what to say is a decision about git, and this function's business is only
/// where it goes.
fn branch_summary(session: &Session) -> String {
    match session.scm_status() {
        Some(status) => format!("{}  ", status.summary()),
        None => String::new(),
    }
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
    // The branch first in the right-hand group: it is about the workspace,
    // where everything after it is about this file, and the two read better
    // apart than interleaved. VS Code puts it at the far left instead, which
    // deco cannot do — that end is the document title and the message line.
    let right = format!(
        " {}{}{}  {}  Ln {}, Col {} ",
        branch_summary(session),
        problem_summary(session),
        language,
        indentation(session),
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
    let text_height = height.saturating_sub(chrome_height(session, height));
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
        cursor_row
            .saturating_sub(box_height)
            .max(tab_bar_height(session))
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
    let text_height = height.saturating_sub(chrome_height(session, height));
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
        cursor_row
            .saturating_sub(box_height)
            .max(tab_bar_height(session))
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

    /// A session sized the way the frontend sizes one, with regions showing.
    ///
    /// The two-step is what `app::resize` does: the session is given what is
    /// left after the bars, and the renderer is given the whole terminal.
    fn with_chrome(side_bar: bool, panel: bool, width: usize, height: usize) -> Session {
        let mut session = session("fn main() {\n    println!(\"hi\");\n}\n");
        session.resize(width, height);
        if side_bar {
            session.run("workbench.action.toggleSidebarVisibility", None, 0);
        }
        if panel {
            session.run("workbench.action.togglePanel", None, 0);
        }
        let chrome = chrome_height(&session, height);
        session.resize(width, height - chrome);
        session
    }

    /// A session whose side bar holds a tree with `src/` open.
    fn with_tree(width: usize, height: usize) -> Session {
        use deco_editor::explorer::Entry;
        let mut session = with_chrome(true, false, width, height);
        session.set_workspace_root("/w");
        session.fill_directory(
            std::path::Path::new("/w"),
            vec![Entry::dir("src"), Entry::file("Cargo.toml")],
        );
        session.fill_directory(
            std::path::Path::new("/w/src"),
            vec![Entry::file("main.rs"), Entry::file("lib.rs")],
        );
        session
    }

    #[test]
    fn the_side_bar_is_named_for_its_tenant_and_lists_the_workspace() {
        let session = with_tree(72, 12);
        let frame = render(&session, 72, 12);
        let rows: Vec<String> = frame.rows.iter().map(|r| r.plain()).collect();
        assert!(rows[0].starts_with(" EXPLORER"), "{:?}", rows[0]);
        // A collapsed directory, then the file, chevron for the one that opens.
        assert!(rows[2].starts_with(" ▸ src"), "{:?}", rows[2]);
        assert!(rows[3].starts_with("   Cargo.toml"), "{:?}", rows[3]);
    }

    #[test]
    fn expanding_a_directory_indents_what_is_inside_it() {
        let mut session = with_tree(72, 12);
        session.run("workbench.files.action.focusFilesExplorer", None, 0);
        session.run("list.expand", None, 0);

        let frame = render(&session, 72, 12);
        let rows: Vec<String> = frame.rows.iter().map(|r| r.plain()).collect();
        assert!(rows[2].starts_with(" ▾ src"), "{:?}", rows[2]);
        assert!(rows[3].starts_with("     lib.rs"), "{:?}", rows[3]);
        assert!(rows[4].starts_with("     main.rs"), "{:?}", rows[4]);
        assert!(rows[5].starts_with("   Cargo.toml"), "{:?}", rows[5]);
    }

    #[test]
    fn an_unread_workspace_says_so_rather_than_drawing_blank() {
        let mut session = with_chrome(true, false, 72, 12);
        session.set_workspace_root("/w");
        let frame = render(&session, 72, 12);
        assert!(
            frame.rows[2].plain().contains("reading the workspace"),
            "{:?}",
            frame.rows[2].plain()
        );
    }

    #[test]
    fn an_empty_workspace_is_not_the_same_as_an_unread_one() {
        let mut session = with_chrome(true, false, 72, 12);
        session.set_workspace_root("/w");
        session.fill_directory(std::path::Path::new("/w"), Vec::new());
        assert!(render(&session, 72, 12).rows[2].plain().contains("empty"));
    }

    #[test]
    fn a_name_too_long_for_the_side_bar_is_cut_rather_than_wrapped() {
        use deco_editor::explorer::Entry;
        let mut session = with_chrome(true, false, 72, 12);
        session.set_workspace_root("/w");
        session.fill_directory(
            std::path::Path::new("/w"),
            vec![Entry::file(&"a".repeat(200))],
        );
        let frame = render(&session, 72, 12);
        let row = frame.rows[2].plain();
        assert!(row.contains('…'), "{row:?}");
        // One row per file, whatever the name's length.
        assert!(
            !frame.rows[3].plain().contains('a'),
            "{:?}",
            frame.rows[3].plain()
        );
    }

    #[test]
    fn a_side_bar_takes_columns_from_the_text_and_leaves_a_rule() {
        let session = with_chrome(true, false, 72, 10);
        let frame = render(&session, 72, 10);
        let first = frame.rows[0].plain();

        assert!(first.starts_with(" SIDE BAR"), "{first:?}");
        // The rule sits where the layout put it, and the text starts past it.
        let rule = session.regions().side_bar_rule.expect("showing");
        assert_eq!(first.chars().nth(rule), Some('│'), "{first:?}");
        assert!(first[rule..].contains("fn main"), "{first:?}");
        assert_eq!(first.chars().count(), 72);
    }

    #[test]
    fn a_right_side_bar_puts_the_text_on_the_left() {
        let mut session = session("fn main() {}\n");
        session.set_workspace_settings(r#"{"workbench.sideBar.location": "right"}"#);
        session.resize(72, 10);
        session.run("workbench.action.toggleSidebarVisibility", None, 0);
        session.resize(72, 10 - chrome_height(&session, 10));

        let frame = render(&session, 72, 10);
        let first = frame.rows[0].plain();
        assert!(first.starts_with("  1 fn main"), "{first:?}");
        assert!(first.trim_end().ends_with("SIDE BAR"), "{first:?}");
    }

    #[test]
    fn the_panel_sits_under_the_text_behind_a_rule() {
        let session = with_chrome(false, true, 72, 12);
        let frame = render(&session, 72, 12);
        let rule = session.regions().panel_rule.expect("showing");

        assert!(
            frame.rows[rule].plain().starts_with("────"),
            "{:?}",
            frame.rows[rule].plain()
        );
        assert!(frame.rows[rule + 1].plain().starts_with(" PANEL"));
    }

    #[test]
    fn the_two_rules_join_where_they_meet() {
        // `│` butted against a run of `─` reads as two borders that happen to
        // touch rather than as one drawn frame.
        let session = with_chrome(true, true, 72, 12);
        let frame = render(&session, 72, 12);
        let rule_row = session.regions().panel_rule.expect("showing");
        let rule_column = session.regions().side_bar_rule.expect("showing");

        let row = frame.rows[rule_row].plain();
        assert_eq!(row.chars().nth(rule_column), Some('├'), "{row:?}");
    }

    #[test]
    fn every_row_is_still_exactly_the_window_wide() {
        // The thing region stitching is most likely to get wrong.
        for (side_bar, panel) in [(true, false), (false, true), (true, true)] {
            let session = with_chrome(side_bar, panel, 72, 12);
            let frame = render(&session, 72, 12);
            assert_eq!(frame.rows.len(), 12);
            for (index, row) in frame.rows.iter().enumerate() {
                assert_eq!(
                    row.plain().chars().count(),
                    72,
                    "row {index} with side_bar={side_bar} panel={panel}: {:?}",
                    row.plain()
                );
            }
        }
    }

    #[test]
    fn the_caret_moves_over_by_a_left_side_bar() {
        let plain = session("fn main() {}\n");
        let before = render(&plain, 72, 10).cursor.expect("a caret");

        let session = with_chrome(true, false, 72, 10);
        let after = render(&session, 72, 10).cursor.expect("a caret");
        let shift = session.regions().editor.x as u16;

        assert_eq!(
            after.0,
            before.0 + shift,
            "shifted past the bar and its rule"
        );
        assert_eq!(after.1, before.1);
    }

    #[test]
    fn a_region_with_the_keyboard_takes_the_caret_off_the_text() {
        // Two carets, or one where typing does not go, is a lie about where the
        // next keystroke lands.
        let mut session = with_chrome(true, false, 72, 10);
        assert!(render(&session, 72, 10).cursor.is_some());

        session.run("workbench.action.focusSideBar", None, 0);
        assert_eq!(render(&session, 72, 10).cursor, None);

        session.run("workbench.action.focusActiveEditorGroup", None, 0);
        assert!(
            render(&session, 72, 10).cursor.is_some(),
            "and it comes back"
        );
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
        //
        // It used to say that and then draw both anyway: two rows into a one-row
        // window, which a terminal answers by scrolling. The frame is now as tall
        // as the window it is painted into, whatever is open.
        let frame = render(&searching("foo\n", "foo"), 40, 1);
        assert_eq!(frame.rows.len(), 1);
        assert!(
            frame.rows[0].plain().contains("Ln 1"),
            "the row that survived should be the status bar: {:?}",
            frame.rows[0].plain()
        );
    }

    // ---- The replace row ------------------------------------------------

    /// A session replacing `query` with `replacement` in `text`.
    fn replacing(text: &str, query: &str, replacement: &str) -> Session {
        let mut session = searching(text, query);
        session.run("deco.find.toggleField", None, 0);
        for c in replacement.chars() {
            session.handle_chord(deco_keymap::keys::Chord::parse(&c.to_string()).unwrap(), 0);
        }
        session
    }

    /// The query row while the replace row is also open: third from the bottom.
    fn query_row_of_two(frame: &Frame) -> String {
        frame.rows[frame.rows.len() - 3].plain()
    }

    #[test]
    fn the_replace_row_sits_under_the_query_row() {
        let session = replacing("foo\n", "foo", "bar");
        let frame = render(&session, 40, 8);
        assert!(query_row_of_two(&frame).contains("Find: foo"));
        assert!(
            find_row(&frame).contains("With: bar"),
            "{:?}",
            find_row(&frame)
        );
        assert!(frame.rows.last().unwrap().plain().contains("Ln 1"));
    }

    #[test]
    fn the_two_prompts_line_up() {
        let frame = render(&replacing("foo\n", "foo", "bar"), 40, 8);
        let query = query_row_of_two(&frame);
        let replace = find_row(&frame);
        assert_eq!(
            query.find("foo").unwrap(),
            replace.find("bar").unwrap(),
            "the two fields should start in the same column"
        );
    }

    #[test]
    fn the_replace_row_costs_a_second_line_of_text() {
        let text = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let one = render(&searching(text, "zzz"), 40, 8);
        let two = render(&replacing(text, "zzz", "x"), 40, 8);
        assert_eq!(one.rows.len(), two.rows.len());
        assert!(one.rows[5].plain().contains('f'));
        assert!(
            !two.rows[5].plain().contains('f'),
            "{:?}",
            two.rows[5].plain()
        );
    }

    #[test]
    fn every_row_is_the_terminal_width_with_both_rows_open() {
        let frame = render(&replacing("foo\n", "foo", "bar"), 40, 8);
        for row in &frame.rows {
            assert_eq!(row.plain().chars().count(), 40, "row was {:?}", row.plain());
        }
    }

    #[test]
    fn the_caret_follows_the_focused_input() {
        let session = replacing("foo\n", "foo", "bar");
        let frame = render(&session, 40, 8);
        let (x, y) = frame.cursor.unwrap();
        assert_eq!(y as usize, frame.rows.len() - 2, "on the replace row");
        // " With: " is seven columns, then three typed characters.
        assert_eq!(x, 10);

        // Back to the query, and the caret goes with it.
        let mut session = session;
        session.run("deco.find.toggleField", None, 0);
        let frame = render(&session, 40, 8);
        let (x, y) = frame.cursor.unwrap();
        assert_eq!(y as usize, frame.rows.len() - 3, "on the query row");
        assert_eq!(x, 10);
    }

    #[test]
    fn the_unfocused_input_is_still_drawn() {
        let session = replacing("foo\n", "foo", "bar");
        let frame = render(&session, 40, 8);
        // The query has the text even though the replacement has the keyboard.
        assert!(query_row_of_two(&frame).contains("foo"));
    }

    #[test]
    fn the_count_stays_on_the_query_row() {
        let frame = render(&replacing("foo foo\n", "foo", "bar"), 40, 8);
        assert!(query_row_of_two(&frame).contains("1 of 2"));
        assert!(!find_row(&frame).contains(" of "), "{:?}", find_row(&frame));
    }

    #[test]
    fn an_empty_replacement_draws_an_empty_field() {
        let session = replacing("foo\n", "foo", "");
        let frame = render(&session, 40, 8);
        assert_eq!(find_row(&frame).trim(), "With:");
    }

    // ---- Semantic highlighting ---------------------------------------------

    fn semantic(
        token_type: &str,
        line: u32,
        from: u32,
        to: u32,
    ) -> deco_lsp::requests::SemanticSpan {
        deco_lsp::requests::SemanticSpan {
            range: deco_core::position::Range::new(
                Position::new(line, from),
                Position::new(line, to),
            ),
            token_type: token_type.to_owned(),
            modifiers: Vec::new(),
        }
    }

    /// The colour the default theme gives a semantic token type.
    fn semantic_colour(session: &Session, token_type: &str) -> Option<Rgba> {
        session
            .theme
            .style_for_semantic(&deco_theme::semantic::SemanticToken::new(
                token_type,
                &[],
                session.document.language(),
            ))
            .and_then(|style| style.foreground)
    }

    #[test]
    fn a_servers_classification_wins_over_the_lexers_guess() {
        // The case the whole feature exists for: `Widget` is capitalised, so the
        // lexer calls it a type; a server that says it is a variable is right.
        let mut session = session("let Widget = 1;\n");
        let Some(expected) = semantic_colour(&session, "variable") else {
            // The bundled theme has no rule for this token type, so there is
            // nothing to assert about precedence.
            return;
        };
        session.semantic_tokens = vec![semantic("variable", 0, 4, 10)];
        let gutter = gutter_width(&session);
        let colours = foregrounds(&render(&session, 40, 5));
        assert_eq!(colours[gutter + 4], expected);
        assert_ne!(
            colours[gutter + 4],
            styled(&session, deco_syntax::scopes::TYPE),
            "the lexer's answer should have been overruled"
        );
    }

    #[test]
    fn text_outside_a_token_keeps_the_lexers_colour() {
        let mut session = session("let Widget = 1;\n");
        session.semantic_tokens = vec![semantic("variable", 0, 4, 10)];
        let gutter = gutter_width(&session);
        let colours = foregrounds(&render(&session, 40, 5));
        // `let` is still a keyword: the server classified only the name.
        assert_eq!(
            colours[gutter],
            styled(&session, deco_syntax::scopes::KEYWORD)
        );
    }

    #[test]
    fn a_token_type_the_theme_does_not_style_falls_back_to_the_lexer() {
        // Not to the plain foreground: losing the keyword colour because a server
        // also had an opinion about it would be a regression.
        let mut session = session("let x = 1;\n");
        session.semantic_tokens = vec![semantic("nonsenseTokenType", 0, 0, 3)];
        let gutter = gutter_width(&session);
        let colours = foregrounds(&render(&session, 40, 5));
        assert_eq!(
            colours[gutter],
            styled(&session, deco_syntax::scopes::KEYWORD)
        );
    }

    #[test]
    fn tokens_on_other_lines_do_not_colour_this_one() {
        let mut session = session("let a = 1;\nlet b = 2;\n");
        session.semantic_tokens = vec![semantic("variable", 1, 4, 5)];
        let gutter = gutter_width(&session);
        let frame = render(&session, 40, 6);
        let first: Vec<Rgba> = frame.rows[0]
            .spans
            .iter()
            .flat_map(|span| span.text.chars().map(move |_| span.fg))
            .collect();
        let palette = Palette::from(&session);
        assert_eq!(first[gutter + 4], palette.fg, "line 0 has no token");
    }

    #[test]
    fn the_setting_can_turn_semantic_highlighting_off() {
        let mut session = session("let Widget = 1;\n");
        if semantic_colour(&session, "variable").is_none() {
            return;
        }
        session.semantic_tokens = vec![semantic("variable", 0, 4, 10)];
        session
            .settings
            .load_layer(
                deco_config::Scope::User,
                r#"{ "editor.semanticHighlighting.enabled": false }"#,
            )
            .unwrap();
        let gutter = gutter_width(&session);
        let colours = foregrounds(&render(&session, 40, 5));
        assert_eq!(
            colours[gutter + 4],
            styled(&session, deco_syntax::scopes::TYPE),
            "the lexer's answer should stand when the setting is off"
        );
    }

    #[test]
    fn an_unrecognised_setting_value_behaves_as_the_default() {
        // A misspelling should not silently disable a feature.
        let mut session = session("let x = 1;\n");
        session
            .settings
            .load_layer(
                deco_config::Scope::User,
                r#"{ "editor.semanticHighlighting.enabled": "configuredByThyme" }"#,
            )
            .unwrap();
        assert_eq!(
            semantic_highlighting(&session),
            session.theme.semantic_highlighting()
        );
    }

    #[test]
    fn the_setting_defers_to_the_theme_when_absent() {
        let session = session("let x = 1;\n");
        assert_eq!(
            semantic_highlighting(&session),
            session.theme.semantic_highlighting()
        );
    }

    // ---- The tab bar ------------------------------------------------------

    /// A session with exactly `names` open as tabs, the first one active.
    ///
    /// Built from scratch rather than through `session()`, which already opens a
    /// file — the first open below replaces the pristine untitled tab, so the
    /// count comes out exact.
    fn tabbed(names: &[&str]) -> Session {
        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );
        for (index, name) in names.iter().enumerate() {
            session.open(
                PathBuf::from(format!("/w/{name}")),
                &format!("file {index}\n"),
            );
        }
        for _ in 1..names.len() {
            session.run("workbench.action.previousEditor", None, 0);
        }
        session
    }

    #[test]
    fn one_tab_shows_no_bar() {
        let frame = render(&tabbed(&["a.rs"]), 40, 6);
        assert!(
            !frame.rows[0].plain().contains("a.rs"),
            "{:?}",
            frame.rows[0].plain()
        );
        assert!(
            frame.rows[0].plain().contains("file 0"),
            "row 0 is still the text"
        );
    }

    #[test]
    fn two_tabs_put_a_bar_on_the_top_row() {
        let session = tabbed(&["a.rs", "b.rs"]);
        let frame = render(&session, 40, 6);
        let bar = frame.rows[0].plain();
        assert!(bar.contains("a.rs") && bar.contains("b.rs"), "{bar:?}");
        // The text moved down a row, and so did the cursor.
        assert!(
            frame.rows[1].plain().contains("file 0"),
            "{:?}",
            frame.rows[1].plain()
        );
        assert_eq!(frame.cursor.unwrap().1, 1);
    }

    #[test]
    fn the_bar_costs_the_text_area_a_row_not_the_status_bar() {
        let one = render(&tabbed(&["a.rs"]), 40, 6);
        let two = render(&tabbed(&["a.rs", "b.rs"]), 40, 6);
        assert_eq!(one.rows.len(), two.rows.len());
        assert!(two.rows.last().unwrap().plain().contains("Ln 1"));
    }

    #[test]
    fn the_active_tab_is_set_apart_by_colour() {
        let session = tabbed(&["a.rs", "b.rs"]);
        let frame = render(&session, 40, 6);
        let spans = &frame.rows[0].spans;
        let active = spans
            .iter()
            .find(|span| span.text.contains("a.rs"))
            .expect("the active tab is drawn");
        let inactive = spans
            .iter()
            .find(|span| span.text.contains("b.rs"))
            .expect("the inactive tab is drawn");
        assert_ne!(
            (active.fg, active.bg),
            (inactive.fg, inactive.bg),
            "the two tabs must not look the same"
        );
    }

    #[test]
    fn a_dirty_tab_is_marked_in_the_bar() {
        let mut session = tabbed(&["a.rs", "b.rs"]);
        session.run("type", Some(&serde_json::json!({ "text": "x" })), 0);
        let bar = render(&session, 40, 6).rows[0].plain();
        assert!(bar.contains("a.rs*"), "{bar:?}");
        assert!(!bar.contains("b.rs*"), "{bar:?}");
    }

    #[test]
    fn every_row_is_the_terminal_width_with_the_bar_open() {
        let frame = render(&tabbed(&["a.rs", "b.rs"]), 40, 6);
        for row in &frame.rows {
            assert_eq!(row.plain().chars().count(), 40, "row was {:?}", row.plain());
        }
    }

    #[test]
    fn a_bar_wider_than_the_terminal_truncates_whole_tabs() {
        let names: Vec<String> = (0..12).map(|n| format!("file-{n:02}.rs")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let frame = render(&tabbed(&refs), 30, 6);
        let bar = frame.rows[0].plain();
        assert_eq!(bar.chars().count(), 30);
        assert!(!bar.contains("file-11"), "the tail is dropped: {bar:?}");
    }

    #[test]
    fn a_hover_box_never_covers_the_tab_bar() {
        let mut session = tabbed(&["a.rs", "b.rs"]);
        // The cursor is on the first text row, so a box drawn above it would
        // land on the bar if nothing stopped it.
        session.view.selections = SelectionSet::caret(Position::ZERO);
        let hover = deco_lsp::Hover {
            contents: "one\ntwo\nthree\nfour".to_owned(),
            range: None,
        };
        let frame = render_with_hover(&session, 40, 8, Some(&hover));
        let bar = frame.rows[0].plain();
        assert!(bar.contains("a.rs"), "the bar survived: {bar:?}");
    }

    // ---- Syntax highlighting --------------------------------------------

    /// The foreground colour of every cell of row 0, one entry per column.
    /// The same group, described as though it did not have the keyboard.
    ///
    /// Stands in for a second group until there is one, so the rules a split
    /// depends on — one caret, and match highlighting only where the query ran —
    /// are asserted before anything relies on them.
    fn unfocused_copy<'a>(pane: &deco_editor::Pane<'a>) -> deco_editor::Pane<'a> {
        deco_editor::Pane {
            document: pane.document,
            view: pane.view,
            semantic: pane.semantic,
            diagnostics: pane.diagnostics,
            tabs: Vec::new(),
            focused: false,
        }
    }

    /// Every cell's background on the first row.
    fn backgrounds(frame: &Frame) -> Vec<Rgba> {
        frame.rows[0]
            .spans
            .iter()
            .flat_map(|span| span.text.chars().map(move |_| span.bg))
            .collect()
    }

    fn foregrounds(frame: &Frame) -> Vec<Rgba> {
        frame.rows[0]
            .spans
            .iter()
            .flat_map(|span| span.text.chars().map(move |_| span.fg))
            .collect()
    }

    /// The colour the theme gives `scope`.
    fn styled(session: &Session, scope: &str) -> Rgba {
        let source = session.document.syntax.source_scope().unwrap();
        session
            .theme
            .style_for_scopes(&[source, scope])
            .foreground
            .expect("the default theme styles this scope")
    }

    #[test]
    fn a_keyword_gets_the_themes_keyword_colour() {
        let session = session("let x = 1;\n");
        let gutter = gutter_width(&session);
        let colours = foregrounds(&render(&session, 40, 5));
        let keyword = styled(&session, deco_syntax::scopes::KEYWORD);
        assert_eq!(colours[gutter], keyword, "`l` of `let`");
        assert_eq!(colours[gutter + 2], keyword, "`t` of `let`");
    }

    #[test]
    fn strings_and_comments_each_get_their_own_colour() {
        let with_string = session("let s = \"hi\"; // note\n");
        let gutter = gutter_width(&with_string);
        let colours = foregrounds(&render(&with_string, 40, 5));
        assert_eq!(
            colours[gutter + 8],
            styled(&with_string, deco_syntax::scopes::DOUBLE_STRING)
        );
        assert_eq!(
            colours[gutter + 15],
            styled(&with_string, deco_syntax::scopes::LINE_COMMENT)
        );
    }

    #[test]
    fn a_number_gets_the_themes_numeric_colour() {
        let numbers = session("let n = 42;\n");
        let gutter = gutter_width(&numbers);
        let colours = foregrounds(&render(&numbers, 40, 5));
        assert_eq!(
            colours[gutter + 8],
            styled(&numbers, deco_syntax::scopes::NUMBER)
        );
    }

    #[test]
    fn unhighlighted_text_keeps_the_editors_foreground() {
        let session = session("let x = 1;\n");
        let palette = Palette::from(&session);
        let gutter = gutter_width(&session);
        // `x` is an identifier with no classification.
        assert_eq!(
            foregrounds(&render(&session, 40, 5))[gutter + 4],
            palette.fg
        );
    }

    #[test]
    fn a_language_with_no_rules_renders_in_one_colour() {
        let mut markdown = session("let x = 1;\n");
        markdown.open(PathBuf::from("/w/notes.md"), "let x = 1;\n");
        let palette = Palette::from(&markdown);
        assert!(
            foregrounds(&render(&markdown, 40, 5))
                .iter()
                .skip(gutter_width(&markdown))
                .all(|colour| *colour == palette.fg),
            "markdown has no lexer, so nothing should be coloured"
        );
    }

    #[test]
    fn highlighting_survives_a_selection_over_it() {
        // The background says selected, the foreground still says keyword: losing
        // the highlighting under a selection would make selected code unreadable
        // in a different way from unselected code.
        let mut session = session("let x = 1;\n");
        session.view.selections =
            SelectionSet::single(Selection::new(Position::new(0, 0), Position::new(0, 3)));
        let gutter = gutter_width(&session);
        let frame = render(&session, 40, 5);
        let palette = Palette::from(&session);
        let cells: Vec<(Rgba, Rgba)> = frame.rows[0]
            .spans
            .iter()
            .flat_map(|span| span.text.chars().map(move |_| (span.fg, span.bg)))
            .collect();
        assert_eq!(cells[gutter].1, palette.selection_bg);
        assert_eq!(
            cells[gutter].0,
            styled(&session, deco_syntax::scopes::KEYWORD)
        );
    }

    #[test]
    fn highlighting_follows_an_edit() {
        // The cache is invalidated from the edited line, so a `//` typed at the
        // start of a line must colour the rest of it as a comment.
        let mut session = session("let x = 1;\n");
        session.view.selections = SelectionSet::caret(Position::ZERO);
        session.run("type", Some(&serde_json::json!({ "text": "//" })), 0);
        let gutter = gutter_width(&session);
        let colours = foregrounds(&render(&session, 40, 5));
        let comment = styled(&session, deco_syntax::scopes::LINE_COMMENT);
        assert_eq!(colours[gutter], comment);
        assert_eq!(
            colours[gutter + 5],
            comment,
            "`let` is inside the comment now"
        );
    }

    #[test]
    fn a_block_comment_opened_above_colours_the_line_below_it() {
        let session = session("/* one\ntwo\n");
        let gutter = gutter_width(&session);
        let frame = render(&session, 40, 5);
        let second: Vec<Rgba> = frame.rows[1]
            .spans
            .iter()
            .flat_map(|span| span.text.chars().map(move |_| span.fg))
            .collect();
        assert_eq!(
            second[gutter],
            styled(&session, deco_syntax::scopes::BLOCK_COMMENT)
        );
    }

    #[test]
    fn highlighting_lines_up_with_text_outside_the_bmp() {
        // Spans are in UTF-16 units and the renderer walks characters, so an emoji
        // before a keyword would shift the colouring if either side were wrong.
        let session = session("let e = \"😀\"; let x = 1;\n");
        let gutter = gutter_width(&session);
        let colours = foregrounds(&render(&session, 60, 5));
        let keyword = styled(&session, deco_syntax::scopes::KEYWORD);
        // `let e = "😀"; ` occupies 13 display columns before the second `let`.
        assert_eq!(colours[gutter], keyword);
        assert_eq!(colours[gutter + 14], keyword, "the second `let`");
    }

    // ---- The quick-open prompt ------------------------------------------

    /// A session with the command palette open, filtered by `query`.
    fn palette(query: &str) -> Session {
        let mut session = session("fn main() {}\n");
        session.resize(60, 14);
        session.run("workbench.action.showCommands", None, 0);
        for c in query.chars() {
            session.handle_chord(deco_keymap::keys::Chord::parse(&c.to_string()).unwrap(), 0);
        }
        session
    }

    /// The prompt's own row: the one above the status bar.
    fn prompt_line(frame: &Frame) -> String {
        frame.rows[frame.rows.len() - 2].plain()
    }

    #[test]
    fn the_prompt_sits_above_the_status_bar() {
        let mut session = session("a\n");
        session.run("workbench.action.gotoLine", None, 0);
        let frame = render(&session, 60, 14);
        assert!(
            prompt_line(&frame).contains("Go to line:"),
            "{:?}",
            prompt_line(&frame)
        );
        assert!(frame.rows.last().unwrap().plain().contains("Ln 1"));
    }

    #[test]
    fn a_go_to_line_prompt_has_no_list_and_costs_one_row() {
        let mut with = session("a\nb\nc\nd\ne\nf\ng\nh\n");
        let without = render(&with, 60, 14);
        with.run("workbench.action.gotoLine", None, 0);
        with.resize(60, 14 - chrome_height(&with, 14));
        let frame = render(&with, 60, 14);
        assert_eq!(frame.rows.len(), without.rows.len());
        assert!(
            !prompt_line(&frame).contains("commands"),
            "no count to show"
        );
    }

    #[test]
    fn the_palette_lists_its_choices_above_the_prompt() {
        let session = palette("comment");
        let frame = render(&session, 60, 14);
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(all.contains("Toggle Line Comment"), "{all:?}");
        // The identifier is shown too: it is what a keybindings.json refers to.
        assert!(all.contains("editor.action.commentLine"));
        assert!(prompt_line(&frame).contains("Command:"));
    }

    #[test]
    fn the_prompt_counts_the_matching_commands() {
        // Counted from the session rather than written in, so adding a command
        // whose title happens to contain "comment" does not fail a test that is
        // about the readout's wording.
        let session = palette("comment");
        let matches = session.prompt.as_ref().expect("open").matches();
        assert!(matches > 1, "several commands should match");
        assert!(prompt_line(&render(&session, 60, 14)).contains(&format!("{matches} commands")));
    }

    #[test]
    fn the_count_is_named_after_what_is_being_chosen() {
        let mut session = session("x\n");
        session.offer_files(vec![deco_editor::commands::PaletteEntry::new(
            "/w/a.rs", "a.rs",
        )]);
        let line = prompt_line(&render(&session, 60, 14));
        assert!(line.contains("1 file"), "{line:?}");
        assert!(!line.contains("commands"), "{line:?}");
        assert!(!line.contains("1 files"), "one is singular: {line:?}");
    }

    #[test]
    fn a_result_row_does_not_repeat_the_path_the_title_already_shows() {
        let mut session = session("x\n");
        session.offer_search_results(
            "total",
            vec![deco_editor::commands::PaletteEntry::at(
                "/w/src/main.rs",
                "src/main.rs:2: let total = 1;",
                Position::new(1, 8),
            )],
        );
        let frame = render(&session, 60, 14);
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(all.contains("src/main.rs:2:"), "{all:?}");
        assert!(!all.contains("/w/src/main.rs"), "{all:?}");
        assert!(
            prompt_line(&frame).contains("1 match"),
            "{:?}",
            prompt_line(&frame)
        );
    }

    #[test]
    fn the_prompt_says_when_nothing_matches() {
        let session = palette("zzzzz");
        let frame = render(&session, 60, 14);
        assert!(prompt_line(&frame).contains("No commands"));
        // And the list is gone, so it costs a single row.
        assert_eq!(chrome_height(&session, 14), 2);
    }

    #[test]
    fn the_list_never_takes_more_than_its_share_of_the_screen() {
        let session = palette("");
        assert!(
            deco_editor::prompt::MAX_ROWS >= session.prompt.as_ref().unwrap().visible().len(),
            "the window is capped"
        );
        assert_eq!(
            chrome_height(&session, 14),
            1 + 1 + deco_editor::prompt::MAX_ROWS
        );

        // …of a screen that has the rows to spare. On one that does not, the list
        // is what gives way, so that the frame is never taller than the terminal
        // it is painted into.
        assert_eq!(chrome_height(&session, 5), 5);
        assert_eq!(render(&session, 60, 5).rows.len(), 5);
    }

    #[test]
    fn every_row_is_the_terminal_width_with_the_palette_open() {
        let frame = render(&palette("line"), 60, 14);
        for row in &frame.rows {
            assert_eq!(row.plain().chars().count(), 60, "row was {:?}", row.plain());
        }
    }

    #[test]
    fn the_selected_choice_is_drawn_inverted() {
        let session = palette("comment");
        let frame = render(&session, 60, 14);
        let palette_colours = Palette::from(&session);
        // The choices sit directly above the prompt row.
        let count = session.prompt.as_ref().unwrap().matches();
        let first_choice = frame.rows.len() - 2 - count;
        let selected_row = first_choice + session.prompt.as_ref().unwrap().selected_row();
        assert_eq!(
            frame.rows[selected_row].spans[0].bg, palette_colours.status_fg,
            "the selection should be inverted"
        );
        let other =
            frame.rows[first_choice + (selected_row - first_choice + 1) % count].spans[0].bg;
        assert_eq!(other, palette_colours.status_bg);
    }

    #[test]
    fn the_caret_sits_in_the_prompt() {
        let session = palette("li");
        let frame = render(&session, 60, 14);
        let (x, y) = frame.cursor.expect("the caret has to be somewhere");
        assert_eq!(y as usize, frame.rows.len() - 2);
        // " Command: " is ten columns, then two typed characters.
        assert_eq!(x, 12);
    }

    #[test]
    fn closing_the_prompt_gives_the_rows_back() {
        let mut session = palette("comment");
        session.run("workbench.action.closeQuickOpen", None, 0);
        let frame = render(&session, 60, 14);
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(!all.contains("Command:"));
        assert!(!all.contains("Toggle Line Comment"));
        assert_eq!(chrome_height(&session, 14), 1);
    }

    #[test]
    fn the_quick_open_prompt_lists_files_and_labels_itself() {
        let mut session = session("x\n");
        session.offer_files(vec![
            deco_editor::commands::PaletteEntry::new("/w/src/main.rs", "src/main.rs"),
            deco_editor::commands::PaletteEntry::new("/w/README.md", "README.md"),
        ]);
        let frame = render(&session, 60, 14);
        assert!(
            prompt_line(&frame).contains("Open:"),
            "{:?}",
            prompt_line(&frame)
        );
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(all.contains("src/main.rs"), "{all:?}");
        // The path is both the title and the identifier for a file, so the row
        // shows it once rather than twice.
        assert_eq!(all.matches("README.md").count(), 1, "{all:?}");
    }

    #[test]
    fn a_narrow_terminal_drops_the_identifier_rather_than_the_title() {
        let frame = render(&palette("comment"), 26, 14);
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(all.contains("Toggle"), "the title survives: {all:?}");
        assert!(!all.contains("editor.action.commentLine"));
    }

    #[test]
    fn a_split_editor_draws_two_columns_with_a_rule_between() {
        let mut session = session(&"line\n".repeat(40));
        session.resize(80, 10);
        session.run("workbench.action.splitEditor", None, 0);
        let frame = render(&session, 80, 10);

        let first = Row::plain(&frame.rows[0]);
        assert_eq!(columns(&first), 80, "the row still fills the terminal");
        assert_eq!(first.matches('│').count(), 1, "one rule: {first:?}");
        // The same line drawn twice, once per group.
        assert_eq!(first.matches("line").count(), 2, "{first:?}");
    }

    #[test]
    fn the_caret_lands_in_the_group_that_has_the_keyboard() {
        let mut session = session("abcdef\n");
        session.resize(80, 10);
        session.run("workbench.action.splitEditor", None, 0);
        session.view.selections = SelectionSet::caret(Position::new(0, 3));

        // The second group, so past the first column and its rule.
        let right = render(&session, 80, 10).cursor.expect("a caret").0;
        session.run("workbench.action.focusFirstEditorGroup", None, 0);
        session.view.selections = SelectionSet::caret(Position::new(0, 3));
        let left = render(&session, 80, 10).cursor.expect("a caret").0;
        assert!(right > left, "left {left}, right {right}");
        assert!(usize::from(left) < 80 / 2);
    }

    #[test]
    fn column_widths_add_up_to_the_terminal() {
        for width in [1usize, 2, 40, 79, 80, 81] {
            for groups in 1..=3usize {
                let widths = column_widths(width, groups);
                assert_eq!(widths.len(), groups);
                let separators = groups - 1;
                assert_eq!(
                    widths.iter().sum::<usize>() + separators,
                    width.max(separators),
                    "width {width}, groups {groups}"
                );
                // No column is left short of another for no reason.
                let (min, max) = (
                    widths.iter().min().copied().unwrap(),
                    widths.iter().max().copied().unwrap(),
                );
                assert!(max - min <= 1, "{widths:?}");
            }
        }
    }

    #[test]
    fn each_column_gets_its_own_gutter() {
        // Two groups can be showing very different line counts once each has its
        // own file; today they share one, so this checks the gutter is drawn twice.
        let mut session = session(&"line\n".repeat(40));
        session.resize(80, 10);
        session.run("workbench.action.splitEditor", None, 0);
        let first = Row::plain(&render(&session, 80, 10).rows[0]);
        assert_eq!(first.matches(" 1 ").count(), 2, "{first:?}");
    }

    #[test]
    fn an_unfocused_group_draws_no_caret() {
        // Two carets would be a lie about where typing goes.
        let session = session("hello\n");
        let palette = Palette::from(&session);
        let focused = &session.panes()[0];
        assert!(
            pane_rows(&session, focused, 40, 3, &palette)
                .cursor
                .is_some(),
            "the focused group has one"
        );

        let unfocused = unfocused_copy(focused);
        assert!(pane_rows(&session, &unfocused, 40, 3, &palette)
            .cursor
            .is_none());
    }

    #[test]
    fn an_unfocused_group_does_not_highlight_the_find_matches() {
        // The query was never run against its document, so marking text there
        // would be marking what nothing had searched. Its *selection* is still
        // drawn — that belongs to the group's own view, not to the search — so
        // this counts the find colour specifically rather than anything coloured.
        let session = searching("hello hello\n", "hello");
        let palette = Palette::from(&session);
        let focused = &session.panes()[0];
        let marked = |pane: &deco_editor::Pane<'_>| {
            let frame = pane_rows(&session, pane, 40, 2, &palette);
            backgrounds(&frame)
                .into_iter()
                .filter(|bg| *bg == palette.find_highlight_bg || *bg == palette.find_match_bg)
                .count()
        };
        assert!(marked(focused) > 0, "the focused group marks them");
        assert_eq!(marked(&unfocused_copy(focused)), 0);
    }

    #[test]
    fn the_symbol_prompt_shows_each_kind_in_a_second_column() {
        let mut session = session("x\n");
        session.offer_symbols(vec![
            deco_editor::commands::PaletteEntry::at(
                "/w/a.rs",
                "Counter",
                deco_core::Position::new(0, 11),
            )
            .with_detail("struct"),
            deco_editor::commands::PaletteEntry::at(
                "/w/a.rs",
                "Counter.bump",
                deco_core::Position::new(3, 11),
            )
            .with_detail("method"),
        ]);
        let frame = render(&session, 60, 14);
        assert!(prompt_line(&frame).contains("Go to symbol:"));
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(all.contains("Counter.bump"), "{all:?}");
        // The kind, not the path: the path would be the same on every row.
        assert!(all.contains("method"), "{all:?}");
        assert!(!all.contains("/w/a.rs"), "{all:?}");
    }

    #[test]
    fn the_replace_row_closes_with_the_bar() {
        let mut session = replacing("foo\n", "foo", "bar");
        session.run("closeFindWidget", None, 0);
        let frame = render(&session, 40, 8);
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(!all.contains("With:"));
        assert!(!all.contains("Find:"));
    }

    // ---- The cost of a large file ------------------------------------------

    /// `lines` of plausible Rust, long enough to be worth wrapping and lexing.
    fn many_lines(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("    let value_{i} = compute({i}) + other({i});\n"))
            .collect()
    }

    /// A session showing `lines` lines, laid out.
    fn opened(text: &str) -> Session {
        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );
        session.open(PathBuf::from("/w/big.rs"), text);
        session.resize(120, 40);
        session
    }

    /// How long `body` takes, averaged over `runs`.
    fn timed(runs: u32, mut body: impl FnMut()) -> std::time::Duration {
        // Once first, so a lazily built cache is not charged to the measurement.
        body();
        let start = std::time::Instant::now();
        for _ in 0..runs {
            body();
        }
        start.elapsed() / runs
    }

    #[test]
    fn drawing_and_typing_do_not_get_slower_as_the_file_gets_longer() {
        // "Lightweight and fast" is the claim the whole project is for, and it rested
        // on assertion. What makes it true is that the hot paths are bounded by the
        // *window*: the lexer resumes from the earliest line an edit touched, the wrap
        // and the draw walk the visible rows, and the rope makes an edit in the middle
        // of ten megabytes cost what one at the start costs.
        //
        // Asserted as a **ratio** rather than a time, so a loaded CI runner slows both
        // halves together and cannot fail this on its own. The allowance is an order of
        // magnitude, because what it is here to catch is an accidental `O(file)` — a
        // walk from line zero, a re-lex of everything, a `to_string()` of the buffer —
        // and those cost two hundred times more, not ten.
        let small = opened(&many_lines(1_000));
        let large = opened(&many_lines(200_000));

        let draw_small = timed(20, || {
            let _ = render(&small, 120, 40);
        });
        let draw_large = timed(20, || {
            let _ = render(&large, 120, 40);
        });
        assert!(
            draw_large < draw_small * 10,
            "drawing grew with the file: {draw_small:?} for 1k lines, {draw_large:?} for 200k"
        );

        // Typing in the *middle*, which is the worst case for anything that rescans
        // from the top and the best case for nothing.
        let mut small = small;
        let mut large = large;
        let key = deco_keymap::keys::Chord::parse("x").expect("a bound key");
        small.view.selections = SelectionSet::caret(deco_core::Position::new(500, 4));
        large.view.selections = SelectionSet::caret(deco_core::Position::new(100_000, 4));
        let mut clock = 0u64;
        let type_small = timed(50, || {
            clock += 100;
            small.handle_chord(key, clock);
        });
        let type_large = timed(50, || {
            clock += 100;
            large.handle_chord(key, clock);
        });
        assert!(
            type_large < type_small * 10,
            "typing grew with the file: {type_small:?} at 1k lines, {type_large:?} at 200k"
        );
    }

    #[test]
    fn only_the_visible_rows_are_laid_out() {
        // The structural half of the same claim, and the one that cannot be flaky: the
        // work is bounded by the window whatever the timings say.
        let session = opened(&many_lines(200_000));
        let rows = session
            .view
            .visible_rows(&session.document.buffer, &session.document.settings);
        assert!(
            rows.len() <= session.view.height,
            "{} rows for a window of {}",
            rows.len(),
            session.view.height
        );
        let frame = render(&session, 120, 40);
        assert_eq!(frame.rows.len(), 40);
    }

    // ---- Control characters -----------------------------------------------

    /// A document containing terminal escape sequences.
    const HOSTILE: &str = "a\u{1b}[31mred\u{7}\n\u{1b}]52;c;aGVsbG8=\u{7}\n";

    #[test]
    fn a_control_character_never_reaches_the_terminal_as_itself() {
        // The one that matters: `\x1b]52;c;…` is OSC 52, which writes the clipboard on
        // every terminal that supports it. Passing a document's bytes through would
        // make "open this file" mean "let this file talk to your terminal".
        let session = session(HOSTILE);
        let frame = render(&session, 40, 6);
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(
            !all.chars().any(|c| c.is_control()),
            "a control character survived: {all:?}"
        );
    }

    #[test]
    fn a_control_character_is_drawn_as_its_picture() {
        let session = session(HOSTILE);
        let frame = render(&session, 40, 6);
        assert_eq!(
            frame.rows[0].plain(),
            "  1 a␛[31mred␇                          "
        );
        assert!(frame.rows[1].plain().starts_with("  2 ␛]52;c;aGVsbG8=␇"));
    }

    #[test]
    fn the_setting_chooses_the_glyph_or_a_blank_and_not_the_byte() {
        // `renderControlCharacters: false` hides the marker. It cannot mean "send the
        // byte", which is not a rendering option.
        let mut settings = deco_config::Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            "editor.renderControlCharacters",
            serde_json::json!(false),
        );
        let mut session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);
        session.open(PathBuf::from("/w/evil.txt"), HOSTILE);
        let frame = render(&session, 40, 6);
        assert_eq!(
            frame.rows[0].plain(),
            "  1 a [31mred                           "
        );
        let all: String = frame.rows.iter().map(Row::plain).collect();
        assert!(!all.chars().any(|c| c.is_control()), "{all:?}");
    }

    #[test]
    fn a_substitution_costs_no_columns() {
        // A Control Pictures glyph is one column, as a control character was counted
        // to be, so nothing that was laid out around it moves.
        let frame = render(&session(HOSTILE), 40, 6);
        for row in &frame.rows {
            assert_eq!(row.plain().chars().count(), 40, "{:?}", row.plain());
        }
    }

    #[test]
    fn the_painter_sanitises_what_the_renderer_did_not() {
        // A file name reaches the tab bar and a search result carries a line of
        // somebody else's file into a prompt row. Both come from outside the document,
        // so the last line of defence is at the write.
        assert_eq!(sanitise("plain"), "plain");
        assert_eq!(sanitise("a\u{1b}b\u{7}c"), "a␛b␇c");
        assert_eq!(sanitise("\u{7f}"), "␡");
        // A tab has already been expanded and a line holds no break of its own, so
        // neither is substituted.
        assert_eq!(sanitise("a\tb"), "a\tb");
    }

    #[test]
    fn a_settings_file_cannot_reach_the_terminal_through_a_problem_message() {
        // The sharper case, and the reason `sanitise` is exported from the crate root:
        // a problem message quotes what a settings file said, the binary prints it to
        // the real terminal *before* the alternate screen opens, and a cloned
        // repository's `.vscode/settings.json` is somebody else's text.
        let mut settings = deco_config::Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            "workbench.colorTheme",
            serde_json::json!("\u{1b}]52;c;aGVsbG8=\u{7}"),
        );
        let session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);
        let problem = session
            .problems
            .first()
            .expect("an unknown theme is reported");
        assert!(
            problem.chars().any(|c| c.is_control()),
            "the fixture should carry the bytes: {problem:?}"
        );
        assert!(
            !sanitise(problem).chars().any(|c| c.is_control()),
            "and they should not survive being made printable"
        );
    }

    #[test]
    fn sanitising_borrows_when_there_is_nothing_to_do() {
        // Every span of every row of every frame goes through this.
        assert!(matches!(
            sanitise("ordinary text"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    // ---- Rendered whitespace ----------------------------------------------

    /// A session whose `editor.renderWhitespace` is `mode`.
    fn whitespace(text: &str, mode: &str) -> Session {
        let mut settings = deco_config::Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            "editor.renderWhitespace",
            serde_json::json!(mode),
        );
        let mut session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);
        session.open(PathBuf::from("/w/file.txt"), text);
        session
    }

    /// The text of one row, gutter stripped.
    fn text_of(frame: &Frame, row: usize) -> String {
        frame.rows[row].plain()[4..].trim_end().to_owned()
    }

    #[test]
    fn all_marks_every_space_and_tab() {
        // A tab is one arrow at the column it starts on, and blank for the rest of
        // its span — the same way VS Code draws it. Filling the span with dots would
        // make one tab indistinguishable from the spaces it replaces.
        let frame = render(&whitespace("  a b\tc\n", "all"), 40, 6);
        assert_eq!(text_of(&frame, 0), "··a·b→  c");
    }

    #[test]
    fn none_marks_nothing() {
        let frame = render(&whitespace("  a b\tc\n", "none"), 40, 6);
        assert_eq!(text_of(&frame, 0), "  a b   c");
    }

    #[test]
    fn boundary_leaves_a_single_space_between_words_alone() {
        // Otherwise a dot appears between every word of a sentence, and the mode
        // would be no different from `all`.
        let frame = render(&whitespace("  a b  c\n", "boundary"), 40, 6);
        assert_eq!(text_of(&frame, 0), "··a b··c");
    }

    #[test]
    fn trailing_marks_only_what_is_past_the_last_word() {
        let frame = render(&whitespace("  a b   \n", "trailing"), 40, 6);
        assert_eq!(text_of(&frame, 0), "  a b···");
    }

    #[test]
    fn selection_is_the_default_and_marks_only_what_is_selected() {
        // VS Code's default, and the least intrusive useful mode: whitespace appears
        // exactly where you are looking at it.
        let mut session = session("a  b  c\n");
        assert_eq!(
            session.document.settings.render_whitespace,
            deco_config::RenderWhitespace::Selection,
            "the default this test is about"
        );
        session.view.selections = SelectionSet::single(Selection::new(
            deco_core::Position::new(0, 0),
            deco_core::Position::new(0, 4),
        ));
        let frame = render(&session, 40, 6);
        assert_eq!(text_of(&frame, 0), "a··b  c");
    }

    // ---- Rulers -----------------------------------------------------------

    /// A session with rulers at `columns`.
    fn ruled(text: &str, columns: &[usize]) -> Session {
        let mut settings = deco_config::Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            "editor.rulers",
            serde_json::json!(columns),
        );
        let mut session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);
        session.open(PathBuf::from("/w/file.txt"), text);
        session
    }

    /// The background of one cell of one row, counting from the gutter's end.
    fn cell_bg(frame: &Frame, row: usize, column: usize) -> Rgba {
        let mut at = 0usize;
        for span in &frame.rows[row].spans {
            let width = span.text.chars().count();
            if at + width > column + 4 {
                return span.bg;
            }
            at += width;
        }
        panic!("column {column} is past the row");
    }

    #[test]
    fn a_ruler_tints_its_column_under_the_text_and_past_the_end() {
        // Under the text is where it matters: the column a ruler warns about is one
        // only a long line reaches.
        let session = ruled(&format!("{}\n", "x".repeat(20)), &[8]);
        let frame = render(&session, 40, 6);
        let tint = Palette::from(&session).ruler_bg;
        assert_ne!(tint, Palette::from(&session).bg, "the tint is visible");
        assert_eq!(cell_bg(&frame, 0, 8), tint, "under the text");
        assert_eq!(cell_bg(&frame, 1, 8), tint, "and on the empty line below");
        assert_ne!(cell_bg(&frame, 0, 7), tint, "and only that column");
    }

    #[test]
    fn several_rulers_are_all_drawn() {
        let session = ruled(&format!("{}\n", "x".repeat(30)), &[4, 12]);
        let frame = render(&session, 40, 6);
        let tint = Palette::from(&session).ruler_bg;
        assert_eq!(cell_bg(&frame, 0, 4), tint);
        assert_eq!(cell_bg(&frame, 0, 12), tint);
    }

    #[test]
    fn a_selection_wins_over_a_ruler() {
        // The selection is what the user is doing; a ruler is furniture.
        let mut session = ruled("xxxxxxxxxxxx\n", &[4]);
        session.view.selections = SelectionSet::single(Selection::new(
            deco_core::Position::new(0, 0),
            deco_core::Position::new(0, 8),
        ));
        let frame = render(&session, 40, 6);
        assert_eq!(cell_bg(&frame, 0, 4), Palette::from(&session).selection_bg);
    }

    #[test]
    fn no_rulers_is_the_default_and_tints_nothing() {
        let session = session("xxxxxxxxxxxx\n");
        let frame = render(&session, 40, 6);
        let bg = Palette::from(&session).bg;
        for column in 0..12 {
            assert_eq!(cell_bg(&frame, 0, column), bg, "column {column}");
        }
    }

    // ---- Interval line numbers --------------------------------------------

    #[test]
    fn interval_numbers_every_tenth_line_and_the_caret_s() {
        // The caret's line is the one you are about to quote in a stack trace.
        let mut settings = deco_config::Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            "editor.lineNumbers",
            serde_json::json!("interval"),
        );
        let mut session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);
        session.open(PathBuf::from("/w/file.txt"), &"x\n".repeat(30));
        session.view.selections = SelectionSet::caret(deco_core::Position::new(2, 0));

        let frame = render(&session, 40, 12);
        let gutters: Vec<String> = frame
            .rows
            .iter()
            .take(11)
            .map(|row| row.plain()[..4].trim().to_owned())
            .collect();
        assert_eq!(
            gutters,
            ["", "", "3", "", "", "", "", "", "", "10", ""],
            "line 3 has the caret and line 10 is the interval"
        );
    }

    // ---- The indentation readout ------------------------------------------

    #[test]
    fn the_status_bar_says_what_one_tab_inserts() {
        // Not visible from the text, and it is what decides whether a diff is one
        // line or forty.
        let frame = render(&session("fn a() {\n    b();\n}\n"), 60, 8);
        let bar = frame.rows.last().unwrap().plain();
        assert!(bar.contains("Spaces: 4"), "{bar:?}");
    }

    #[test]
    fn tabs_are_named_as_tabs() {
        let frame = render(&session("fn a() {\n\tb();\n}\n"), 60, 8);
        let bar = frame.rows.last().unwrap().plain();
        assert!(bar.contains("Tab: 4"), "{bar:?}");
    }

    #[test]
    fn a_file_that_overrode_the_setting_says_so() {
        // Two-space text where `editor.tabSize` says four. Without the marker a guess
        // is indistinguishable from the setting being wrong.
        let frame = render(&session("const a = {\n  b: 1,\n};\n"), 60, 8);
        let bar = frame.rows.last().unwrap().plain();
        assert!(bar.contains("Spaces: 2 (detected)"), "{bar:?}");
    }

    /// What `git status --porcelain=v2 --branch -z` writes for a branch with
    /// `changed` files differing from `HEAD`.
    fn scm(branch: &str, changed: usize) -> deco_scm::Status {
        let mut records = format!("# branch.oid 1c9d4e5\0# branch.head {branch}\0");
        for n in 0..changed {
            records.push_str(&format!("? file{n}.rs\0"));
        }
        deco_scm::parse(&records).expect("git's own format")
    }

    /// The side bar's rows, trimmed, for a session showing the SCM view.
    fn side_bar_lines(session: &Session) -> Vec<String> {
        render(session, 80, 14)
            .rows
            .iter()
            .map(|row| row.plain().chars().take(28).collect::<String>())
            .map(|line| line.trim_end().to_owned())
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// A session with the source-control view showing `entries`.
    fn with_scm(entries: &[&str]) -> Session {
        let mut out = String::from("# branch.oid 1c9d4e5\0# branch.head main\0");
        for entry in entries {
            out.push_str(entry);
            out.push('\0');
        }
        let mut session = session("fn a() {}\n");
        session.set_workspace_root("/w");
        session.fill_scm(Some(deco_scm::parse(&out).expect("git's own format")));
        session.run("workbench.view.scm", None, 0);
        session.resize(80, 13);
        session
    }

    #[test]
    fn the_source_control_view_groups_what_git_reported() {
        let session = with_scm(&[
            "1 M. N... 100644 100644 100644 aaaaaaa bbbbbbb src/staged.rs",
            "1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb work.rs",
            "? new.rs",
        ]);
        let lines = side_bar_lines(&session);
        let joined = lines.join("\n");
        assert!(joined.contains("SOURCE CONTROL"), "{joined}");
        assert!(joined.contains("Staged Changes 1"), "{joined}");
        assert!(joined.contains("M staged.rs"), "{joined}");
        assert!(
            joined.contains("src"),
            "the directory is shown beside the name: {joined}"
        );
        assert!(joined.contains("Changes 1"), "{joined}");
        assert!(joined.contains("Untracked 1"), "{joined}");
        assert!(joined.contains("? new.rs"), "{joined}");
    }

    #[test]
    fn a_scrolled_group_keeps_its_heading_and_selection_visible() {
        let entries: Vec<String> = (0..30).map(|n| format!("? file{n:02}.rs")).collect();
        let borrowed: Vec<&str> = entries.iter().map(String::as_str).collect();
        let mut session = with_scm(&borrowed);
        session.resize(80, 14);
        for _ in 0..29 {
            session.run("list.focusDown", None, 0);
        }

        let joined = side_bar_lines(&session).join("\n");
        assert!(
            joined.contains("Untracked 30"),
            "a row without its heading does not say whether stage or unstage applies: {joined}"
        );
        assert!(
            joined.contains("? file29.rs"),
            "making room for the repeated heading must not hide the selection: {joined}"
        );
    }

    #[test]
    fn a_clean_tree_says_so_rather_than_showing_an_empty_box() {
        let session = with_scm(&[]);
        let joined = side_bar_lines(&session).join("\n");
        assert!(joined.contains("no changes"), "{joined}");
    }

    #[test]
    fn a_folder_with_no_repository_says_that_instead() {
        let mut session = session("fn a() {}\n");
        session.set_workspace_root("/w");
        session.run("workbench.view.scm", None, 0);
        session.resize(80, 13);
        // Told apart from a clean tree, which is a different thing to be
        // looking at and a different thing to do next.
        let joined = side_bar_lines(&session).join("\n");
        assert!(joined.contains("not a git repository"), "{joined}");
    }

    #[test]
    fn the_tree_is_still_there_when_the_side_bar_switches_back() {
        let mut session = with_scm(&["? new.rs"]);
        session.run("workbench.view.explorer", None, 0);
        let joined = side_bar_lines(&session).join("\n");
        assert!(joined.contains("EXPLORER"), "{joined}");
        assert!(!joined.contains("new.rs"), "{joined}");
    }

    #[test]
    fn the_branch_and_what_differs_from_it_reach_the_status_bar() {
        let mut session = session("fn a() {}\n");
        session.fill_scm(Some(scm("main", 3)));
        let frame = render(&session, 100, 8);
        let bar = frame.rows.last().unwrap().plain();
        assert!(bar.contains("main ±3"), "{bar:?}");
    }

    #[test]
    fn a_workspace_with_no_git_says_nothing_rather_than_something_empty() {
        // Three different situations reach the renderer as `None` — nobody has
        // asked yet, there is no git, this is not a repository — and all three
        // mean the same thing on screen. A stray separator would be the one
        // visible difference between them.
        let before = render(&session("fn a() {}\n"), 100, 8)
            .rows
            .last()
            .unwrap()
            .plain();
        let mut session = session("fn a() {}\n");
        session.fill_scm(Some(scm("main", 0)));
        let after = render(&session, 100, 8).rows.last().unwrap().plain();
        assert!(after.contains("main"), "{after:?}");
        assert!(
            !before.contains('±') && !before.contains("  main"),
            "nothing at all, not a gap where the branch would be: {before:?}"
        );
    }

    /// The gutter's mark column, top to bottom, as a string.
    fn marks(session: &Session) -> String {
        render(session, 60, 8)
            .rows
            .iter()
            .take(4)
            .map(|row| {
                let gutter = gutter_width(session);
                row.plain().chars().nth(gutter - 1).unwrap_or(' ')
            })
            .collect()
    }

    #[test]
    fn changed_lines_are_marked_in_the_gutter() {
        let mut session = session("one\nTWO\nthree\nnew\n");
        session.fill_committed(
            session.document.path.clone().expect("a path"),
            Some("one\ntwo\nthree\n".to_owned()),
        );
        session.refresh_diffs();
        assert_eq!(
            marks(&session),
            " │ ┃",
            "line 2 says something else, line 4 was not there at all"
        );
    }

    #[test]
    fn a_removed_line_is_marked_where_it_was() {
        let mut session = session("one\nthree\n");
        session.fill_committed(
            session.document.path.clone().expect("a path"),
            Some("one\ntwo\nthree\n".to_owned()),
        );
        session.refresh_diffs();
        // Nothing is left to draw beside, so the mark belongs to the line that
        // took its place — on the cell's top edge, where the removed line was.
        assert_eq!(marks(&session), " ▔  ");
    }

    #[test]
    fn git_decorations_false_leaves_the_gutter_alone() {
        let mut session = session("one\nTWO\n");
        session.settings.set(
            deco_config::Scope::User,
            "git.decorations.enabled",
            serde_json::Value::Bool(false),
        );
        session.fill_committed(
            session.document.path.clone().expect("a path"),
            Some("one\ntwo\n".to_owned()),
        );
        session.refresh_diffs();
        assert_eq!(marks(&session), "    ", "the setting turns the marks off");
    }

    #[test]
    fn git_enabled_false_takes_the_branch_off_the_bar() {
        let mut session = session("fn a() {}\n");
        session.settings.set(
            deco_config::Scope::User,
            "git.enabled",
            serde_json::Value::Bool(false),
        );
        // Filled anyway: a setting changed after a run must take what is on
        // screen with it, not wait for the next one.
        session.fill_scm(Some(scm("main", 3)));
        let bar = render(&session, 100, 8).rows.last().unwrap().plain();
        assert!(!bar.contains("main"), "{bar:?}");
    }

    #[test]
    fn a_file_that_agrees_with_the_setting_says_nothing_extra() {
        // Which is most files, and a permanent note about nothing is noise.
        let frame = render(&session("fn a() {\n    b();\n}\n"), 60, 8);
        let bar = frame.rows.last().unwrap().plain();
        assert!(!bar.contains("detected"), "{bar:?}");
    }

    // ---- Word wrap --------------------------------------------------------

    /// A session showing `text` in a `width`-column window, wrapping.
    fn wrapping(text: &str, width: usize) -> Session {
        let mut session = session(text);
        session.resize(width, 8);
        session.run("editor.action.toggleWordWrap", None, 0);
        session.resize(width, 8);
        session
    }

    #[test]
    fn a_wrapped_line_is_drawn_across_rows() {
        let session = wrapping("the quick brown fox jumps over it\n", 24);
        let frame = render(&session, 24, 8);
        assert_eq!(frame.rows[0].plain(), "  1 the quick brown fox ");
        assert_eq!(frame.rows[1].plain(), "    jumps over it       ");
    }

    #[test]
    fn only_a_lines_first_row_carries_its_number() {
        // A number on every row would read as lines the file does not have, and
        // `ctrl+g` would send you somewhere else than the row you counted to.
        let session = wrapping("aaaa bbbb cccc dddd eeee ffff\nsecond\n", 24);
        let frame = render(&session, 24, 8);
        let gutters: Vec<String> = frame
            .rows
            .iter()
            .take(3)
            .map(|row| row.plain()[..4].to_owned())
            .collect();
        assert_eq!(gutters, ["  1 ", "    ", "  2 "]);
    }

    #[test]
    fn the_same_line_unwrapped_is_truncated_at_the_edge() {
        // The behaviour wrapping replaces, and the reason the setting is worth
        // having: without it the rest of the line is simply not on screen.
        let session = session("the quick brown fox jumps over it\n");
        let frame = render(&session, 24, 8);
        assert_eq!(frame.rows[0].plain(), "  1 the quick brown fox ");
        assert_eq!(frame.rows[1].plain(), "  2                     ");
    }

    #[test]
    fn the_caret_is_drawn_on_the_row_its_column_is_on() {
        let mut session = wrapping("the quick brown fox jumps over it\n", 24);
        session.view.selections = deco_core::SelectionSet::caret(deco_core::Position::new(0, 22));
        let frame = render(&session, 24, 8);
        // Column 22 is two into the second row, which starts at 20.
        assert_eq!(frame.cursor, Some((6, 1)));
    }

    #[test]
    fn a_selection_across_a_break_is_drawn_on_both_rows() {
        let mut session = wrapping("aaaa bbbb cccc dddd eeee\n", 24);
        session.view.selections = deco_core::SelectionSet::single(Selection::new(
            deco_core::Position::new(0, 2),
            deco_core::Position::new(0, 23),
        ));
        let frame = render(&session, 24, 8);
        let selected = |row: &Row| {
            row.spans
                .iter()
                .any(|span| span.bg != palette_bg(&session) && !span.text.trim().is_empty())
        };
        assert!(selected(&frame.rows[0]), "the first row");
        assert!(selected(&frame.rows[1]), "and the second");
    }

    /// A session wrapping at `width` less the gutter, with `editor.wrappingIndent`.
    fn indented(text: &str, width: usize, mode: &str) -> Session {
        let mut settings = deco_config::Settings::with_defaults();
        settings
            .load_layer(
                deco_config::Scope::User,
                &format!(r#"{{"editor.wordWrap": "on", "editor.wrappingIndent": "{mode}"}}"#),
            )
            .expect("valid settings");
        let mut session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);
        session.open(PathBuf::from("/w/file.txt"), text);
        session.resize(width, 8);
        session
    }

    /// An indented line long enough to wrap at thirty columns.
    const NESTED: &str = "fn a() {\n    let x = one two three four five six;\n}\n";

    #[test]
    fn a_continuation_row_matches_the_lines_own_indent() {
        // VS Code's default, and the reason a wrapped block still reads as one block:
        // at column zero the second row sits beside the unindented lines around it.
        let frame = render(&indented(NESTED, 34, "same"), 34, 8);
        assert_eq!(frame.rows[1].plain(), "  2     let x = one two three four");
        assert_eq!(frame.rows[2].plain(), "        five six;                 ");
    }

    #[test]
    fn none_starts_a_continuation_row_at_column_zero() {
        let frame = render(&indented(NESTED, 34, "none"), 34, 8);
        assert_eq!(frame.rows[2].plain(), "    five six;                     ");
    }

    #[test]
    fn indent_goes_one_level_deeper_than_the_line() {
        // Four more columns than `same`, at the default tab size.
        let frame = render(&indented(NESTED, 34, "indent"), 34, 8);
        assert_eq!(frame.rows[2].plain(), "            five six;             ");
    }

    #[test]
    fn deep_indent_goes_two_levels_deeper() {
        let frame = render(&indented(NESTED, 34, "deepIndent"), 34, 8);
        assert_eq!(frame.rows[2].plain(), "                five six;         ");
    }

    #[test]
    fn an_indent_that_would_leave_no_room_is_dropped() {
        // Past half the width a wrapped line is more indent than text, and a deeply
        // nested one would be wrapped into a column a few characters wide. Dropped
        // rather than trimmed: a partial indent lines the continuation up with
        // nothing.
        let deep = format!("{}one two three four five\n", " ".repeat(18));
        let frame = render(&indented(&deep, 34, "same"), 34, 8);
        assert_eq!(
            frame.rows[1].plain(),
            "    three four five               ",
            "column zero, not eighteen"
        );
    }

    #[test]
    fn the_caret_sits_after_the_indent_on_a_continuation_row() {
        // The row's text starts at the indent, so the caret has to as well — or it is
        // drawn beside the character it is on.
        let mut session = indented(NESTED, 34, "same");
        // Character 31 is the `f` of `five`, the first on the second row.
        session.view.selections = SelectionSet::caret(deco_core::Position::new(1, 31));
        let frame = render(&session, 34, 8);
        assert_eq!(
            frame.cursor,
            Some((8, 2)),
            "four columns of gutter, then four of indent"
        );
    }

    #[test]
    fn down_moves_by_screen_column_across_the_indent() {
        // The goal column is relative to the row's own text, so `down` from three
        // columns into row 0's text lands three columns into row 1's — which is a
        // different document column, and the same place on screen.
        let mut session = indented(NESTED, 34, "same");
        session.view.selections = SelectionSet::caret(deco_core::Position::new(1, 11));
        let before = render(&session, 34, 8).cursor.expect("a caret");
        session.run("cursorDown", None, 0);
        let after = render(&session, 34, 8).cursor.expect("a caret");
        assert_eq!(after.0, before.0, "the same screen column");
        assert_eq!(after.1, before.1 + 1, "one row down");
    }

    #[test]
    fn a_tab_on_a_continuation_row_is_measured_from_that_rows_start() {
        // A row is what has tab stops on screen. The break here is at column 22,
        // which is not a multiple of the tab size, so measuring from the line's
        // start instead would put the `c` one column along rather than three.
        let session = wrapping(&format!("{}b\tc\n", "a".repeat(22)), 26);
        let frame = render(&session, 26, 8);
        assert_eq!(frame.rows[1].plain(), "    b   c                 ");
    }

    /// The editor background, for tests that ask whether a cell is decorated.
    fn palette_bg(session: &Session) -> Rgba {
        Palette::from(session).bg
    }
}
