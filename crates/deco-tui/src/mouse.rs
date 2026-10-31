//! What is under the pointer, in the frame the renderer draws.
//!
//! The renderer lays the window out as rows of cells; [`hit`] answers, for a
//! cell, which part of that layout it belongs to and where inside it. It uses
//! the same measurements as the renderer (the tab bar's row, the regions, the
//! groups' columns, the rows below the text), so a click lands on what is
//! drawn there. [`crate::Driver::mouse`] then calls the session's mouse
//! methods with those positions.
//!
//! Both frontends use it: the terminal frontend with the cell crossterm
//! reports, and the GPU frontend with the cell under the pointer's pixels.

use deco_editor::layout::column_widths;
use deco_editor::{PanelView, Session, SideBarView};

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// The primary button.
    Left,
    /// The wheel's button.
    Middle,
    /// The secondary button.
    Right,
}

/// What the mouse did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A button went down.
    Press(Button),
    /// A button came up.
    Release(Button),
    /// The pointer moved with a button held.
    Drag(Button),
    /// The pointer moved with no button held.
    Move,
    /// The wheel turned away from the user.
    ScrollUp,
    /// The wheel turned towards the user.
    ScrollDown,
}

/// A mouse event at a cell of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mouse {
    /// What happened.
    pub action: Action,
    /// The column, from 0 at the left.
    pub col: u16,
    /// The row, from 0 at the top.
    pub row: u16,
    /// Shift was held.
    pub shift: bool,
    /// Alt was held.
    pub alt: bool,
    /// Ctrl was held.
    pub ctrl: bool,
}

/// The part of the frame a cell belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Tab `index` of the tab bar.
    Tab {
        /// Which tab, counting in the group.
        index: usize,
    },
    /// The text of group `group`, at a row of the group and a column from
    /// its left edge, gutter included.
    Text {
        /// Which group, counting from the left.
        group: usize,
        /// The row, from the group's top.
        row: usize,
        /// The column, from the group's left edge.
        col: usize,
    },
    /// Row `line` of the side bar's list, counted from its first row under
    /// the heading.
    SideBar {
        /// The row.
        line: usize,
    },
    /// The side bar's heading.
    SideBarHeading,
    /// Problem `index` of the problems view, counting every problem.
    Problem {
        /// Which problem.
        index: usize,
    },
    /// A cell of the shown terminal's screen.
    Terminal {
        /// The row of the screen.
        row: usize,
        /// The column.
        col: usize,
    },
    /// The panel, outside any list or screen: its heading or a blank row.
    Panel,
    /// Row `row` of an open prompt's list, counted from the first row drawn.
    Prompt {
        /// The row.
        row: usize,
    },
    /// The status bar, the find bar, a rule between regions, or nothing.
    Other,
}

/// What the cell at `col`, `row` of a `width` by `height` frame shows.
pub fn hit(session: &Session, width: usize, height: usize, col: usize, row: usize) -> Hit {
    if col >= width || row >= height {
        return Hit::Other;
    }
    let chrome = crate::render::chrome_height(session, height);
    // A frame too short for its chrome drops rows from the top, which moves
    // everything; nothing is clickable then rather than the wrong thing.
    if chrome >= height {
        return Hit::Other;
    }
    let top = crate::render::tab_bar_height(session);
    let text_height = height - chrome;
    if row < top {
        return tab_at(session, col);
    }
    let row = row - top;
    if row < text_height {
        return text_area_hit(session, width, text_height, col, row);
    }
    // Below the text: the find bar's rows, then the prompt's list and its
    // input row, then the status bar.
    let below = row - text_height;
    let find = if session.find.visible() {
        1 + usize::from(session.find.replacing())
    } else {
        0
    };
    if below < find {
        return Hit::Other;
    }
    let listed = crate::render::prompt_rows(session, height);
    if session.prompt.is_some() && below - find < listed {
        return Hit::Prompt { row: below - find };
    }
    Hit::Other
}

/// The tab under `col` in the tab bar, laid out as the renderer lays it out.
fn tab_at(session: &Session, col: usize) -> Hit {
    let mut used = 0usize;
    for pane in session.panes() {
        for (index, label) in pane.tabs.iter().enumerate() {
            let dirty = if label.dirty { "*" } else { "" };
            let cells = crate::render::columns(&format!(" {}{dirty} ", label.title));
            if col < used + cells {
                return Hit::Tab { index };
            }
            used += cells;
        }
    }
    Hit::Other
}

fn text_area_hit(session: &Session, width: usize, height: usize, col: usize, row: usize) -> Hit {
    let regions = session.regions_for(width, height);
    if regions.panel_rule == Some(row) || regions.side_bar_rule == Some(col) {
        return Hit::Other;
    }
    if let Some(rect) = regions.side_bar.filter(|rect| contains(rect, col, row)) {
        let line = row - rect.y;
        return match line.checked_sub(deco_editor::session::EXPLORER_CHROME_ROWS) {
            Some(line) => Hit::SideBar { line },
            None => Hit::SideBarHeading,
        };
    }
    if let Some(rect) = regions.panel.filter(|rect| contains(rect, col, row)) {
        let (inside_row, inside_col) = (row - rect.y, col - rect.x);
        let Some(content_row) = inside_row.checked_sub(1) else {
            return Hit::Panel;
        };
        if session.panel_view() == PanelView::Problems {
            // Scrolled as the renderer scrolls it: the selection on screen,
            // with as many rows above it as fit.
            let rows = rect.height.saturating_sub(1).max(1);
            let first = session.markers().selected().saturating_sub(rows - 1);
            let index = first + content_row;
            return if index < session.markers().len() {
                Hit::Problem { index }
            } else {
                Hit::Panel
            };
        }
        if session.terminal().is_some() {
            return Hit::Terminal {
                row: content_row,
                col: inside_col,
            };
        }
        return Hit::Panel;
    }
    let editor = regions.editor;
    if !contains(&editor, col, row) {
        return Hit::Other;
    }
    // Groups side by side, with a one-column rule between each pair.
    let groups = session.panes().len();
    let mut left = editor.x;
    for (group, width) in column_widths(editor.width, groups).into_iter().enumerate() {
        if col < left + width {
            return Hit::Text {
                group,
                row: row - editor.y,
                col: col - left,
            };
        }
        left += width + 1;
        if col < left {
            return Hit::Other;
        }
    }
    Hit::Other
}

fn contains(rect: &deco_editor::layout::Rect, col: usize, row: usize) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Whether the side bar shows the tree, which the wheel scrolls.
pub(crate) fn tree_showing(session: &Session) -> bool {
    session.side_bar_view() == SideBarView::Explorer && session.explorer().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_editor::Session;

    fn session() -> Session {
        let mut session = Session::with_defaults();
        session.open(std::path::PathBuf::from("/w/a.txt"), "one\ntwo\n");
        session
    }

    #[test]
    fn a_cell_in_the_text_is_the_text_at_that_row_and_column() {
        let mut s = session();
        s.resize(40, 9);
        assert_eq!(
            hit(&s, 40, 10, 5, 1),
            Hit::Text {
                group: 0,
                row: 1,
                col: 5
            }
        );
        // The last row is the status bar.
        assert_eq!(hit(&s, 40, 10, 5, 9), Hit::Other);
    }

    #[test]
    fn with_two_tabs_the_first_row_is_the_tab_bar() {
        let mut s = session();
        s.open(std::path::PathBuf::from("/w/b.txt"), "b\n");
        s.resize(40, 8);
        assert_eq!(hit(&s, 40, 10, 1, 0), Hit::Tab { index: 0 });
        // ` a.txt ` is seven columns.
        assert_eq!(hit(&s, 40, 10, 8, 0), Hit::Tab { index: 1 });
        assert_eq!(
            hit(&s, 40, 10, 0, 1),
            Hit::Text {
                group: 0,
                row: 0,
                col: 0
            },
            "the text starts below the bar"
        );
    }

    #[test]
    fn the_second_group_starts_after_the_rule() {
        let mut s = session();
        s.run("workbench.action.splitEditor", None, 0);
        s.resize(41, 9);
        // Two columns of twenty with a rule at column 20.
        assert_eq!(hit(&s, 41, 10, 20, 0), Hit::Other);
        assert_eq!(
            hit(&s, 41, 10, 21, 0),
            Hit::Text {
                group: 1,
                row: 0,
                col: 0
            }
        );
    }

    #[test]
    fn the_side_bar_rows_start_under_its_heading() {
        let mut s = session();
        s.set_workspace_root("/w");
        s.run("workbench.action.toggleSidebarVisibility", None, 0);
        s.resize(80, 19);
        let rect = s.regions_for(80, 19).side_bar.expect("a side bar");
        assert_eq!(hit(&s, 80, 20, rect.x, 0), Hit::SideBarHeading);
        assert_eq!(hit(&s, 80, 20, rect.x, 2), Hit::SideBar { line: 0 });
    }
}
