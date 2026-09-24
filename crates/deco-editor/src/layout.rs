//! How the screen is divided: between the chrome regions, and within the
//! editor between the gutter and the groups.
//!
//! This is in the core rather than in a frontend because both the renderer and
//! the core use it. The renderer uses it to draw, and the core uses it to wrap
//! lines, since the break position depends on the columns available for text.
//! If the frontend computed the layout, the two could disagree about the width
//! and draw the caret one column away from its text.
//!
//! The functions are pure arithmetic over session state. Nothing here touches a
//! terminal.

use deco_config::{LineNumbers, SideBarLocation};

use crate::Document;

/// A rectangle of cells, relative to the top-left of the area provided by the
/// frontend.
///
/// The unit is cells, not pixels. Cells are the terminal's unit, and the GPU
/// frontend multiplies by its own font metrics, so the core does not depend on
/// fonts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    /// Columns from the left edge.
    pub x: usize,
    /// Rows from the top edge.
    pub y: usize,
    /// How many columns wide.
    pub width: usize,
    /// How many rows tall.
    pub height: usize,
}

impl Rect {
    /// Whether there is anything to draw in it.
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// How wide the side bar is when it fits.
///
/// A fixed column count rather than a fraction of the window. The side bar
/// shows a file tree, and a width that followed the window would reflow every
/// path on each resize. VS Code's side bar also has a fixed pixel width. This is
/// not a setting: VS Code has no such setting either, and it stores a dragged
/// width, while deco [writes no files](../../../docs/configuration.md) in which
/// to store one.
pub const SIDE_BAR_WIDTH: usize = 30;

/// The narrowest width a side bar can shrink to before it is hidden.
///
/// A tree in ten columns shows mostly ellipses, which is not worth the space it
/// takes from the editor.
pub const MIN_SIDE_BAR_WIDTH: usize = 12;

/// How tall the panel is when it fits.
pub const PANEL_HEIGHT: usize = 10;

/// The shortest height a panel can shrink to before it is hidden.
///
/// This is two rows of content under a rule. A smaller panel would show only
/// its border.
pub const MIN_PANEL_HEIGHT: usize = 3;

/// The minimum size the editor keeps regardless of other regions.
///
/// A region never shrinks the editor below this. The editor has priority over
/// the side bar and panel.
pub const MIN_EDITOR_WIDTH: usize = 20;
/// The same, in rows.
pub const MIN_EDITOR_HEIGHT: usize = 3;

/// Where the screen's regions ended up.
///
/// A requested region that does not fit is `None` rather than an empty
/// rectangle. "No room in this window" and "hidden" are different states, and
/// only the first changes when the window grows. Visibility remains
/// [session state](crate::Session); this type holds only the computed layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Regions {
    /// Where the text and its groups go.
    pub editor: Rect,
    /// The side bar, if it is showing and fits.
    pub side_bar: Option<Rect>,
    /// The panel, if it is showing and fits.
    pub panel: Option<Rect>,
    /// The column the rule between side bar and editor sits in.
    ///
    /// Kept outside both rectangles because it is drawn in the chrome's colour.
    /// If a region's rectangle included the rule, drawing the region could
    /// overwrite it.
    pub side_bar_rule: Option<usize>,
    /// The row the rule between editor and panel sits in.
    pub panel_rule: Option<usize>,
}

/// Divides `width` x `height` between the editor and whichever regions are
/// showing.
///
/// The panel is taken from the bottom before the side bar is taken from the
/// side, so the side bar spans the full height beside both the editor and the
/// panel. This matches VS Code's layout, and is why a terminal in the panel is
/// as wide as the window.
pub fn regions(
    width: usize,
    height: usize,
    side_bar: Option<SideBarLocation>,
    panel: bool,
) -> Regions {
    let mut editor = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut out = Regions {
        editor,
        side_bar: None,
        panel: None,
        side_bar_rule: None,
        panel_rule: None,
    };

    if panel {
        // One row for the rule, and the editor keeps its minimum.
        if let Some(rows) = fits(height, PANEL_HEIGHT, MIN_PANEL_HEIGHT, MIN_EDITOR_HEIGHT) {
            editor.height = height - rows - 1;
            out.panel_rule = Some(editor.height);
            out.panel = Some(Rect {
                x: 0,
                y: editor.height + 1,
                width,
                height: rows,
            });
        }
    }

    if let Some(location) = side_bar {
        if let Some(columns) = fits(width, SIDE_BAR_WIDTH, MIN_SIDE_BAR_WIDTH, MIN_EDITOR_WIDTH) {
            editor.width = width - columns - 1;
            let (bar_x, rule) = match location {
                SideBarLocation::Left => {
                    editor.x = columns + 1;
                    (0, columns)
                }
                SideBarLocation::Right => (editor.width + 1, editor.width),
            };
            out.side_bar_rule = Some(rule);
            out.side_bar = Some(Rect {
                x: bar_x,
                y: 0,
                width: columns,
                // Beside the panel as well as the editor.
                height,
            });
            // The panel sits under the editor, not under the side bar.
            if let Some(panel) = out.panel.as_mut() {
                panel.x = editor.x;
                panel.width = editor.width;
            }
        }
    }

    out.editor = editor;
    out
}

/// How much of `total` a region gets, or `None` if it does not fit.
///
/// The region gets `wanted` if possible and less in a small window. It gets
/// nothing if it would be smaller than `min` or would leave the editor with
/// less than `keep`. The `+ 1` is the rule between the region and the editor.
///
/// The region is also capped at half of the space, so a small window is not
/// mostly chrome. Without the cap, a panel in a twelve-row terminal would take
/// every row above the editor's three-row minimum. With it, the region never
/// takes more than it leaves.
fn fits(total: usize, wanted: usize, min: usize, keep: usize) -> Option<usize> {
    let spare = total.checked_sub(keep + 1)?;
    // Half of the space *after* the rule, not half of the window. The rule is
    // taken from the editor's share, so halving the whole window would give the
    // region one more cell than the editor.
    let half = total.saturating_sub(1) / 2;
    let room = spare.min(wanted).min(half);
    (room >= min).then_some(room)
}

/// Columns the line-number gutter needs for `document`.
///
/// Computed per document rather than per session, because groups side by side
/// can show files of very different lengths.
pub fn gutter_width(document: &Document) -> usize {
    if document.settings.line_numbers == LineNumbers::Off {
        return 0;
    }
    let digits = document.buffer.line_count().to_string().len();
    // One space of padding on each side keeps the text off the numbers.
    digits.max(2) + 2
}

/// How wide each editor group's column is, left to right.
///
/// The remainder goes to the leftmost columns, one cell each, so the widths
/// differ by at most one. One separator column sits between each pair, so the
/// separators are subtracted before dividing.
pub fn column_widths(width: usize, groups: usize) -> Vec<usize> {
    if groups <= 1 {
        return vec![width];
    }
    let separators = groups - 1;
    let usable = width.saturating_sub(separators);
    let each = usable / groups;
    let extra = usable % groups;
    (0..groups)
        .map(|index| each + usize::from(index < extra))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_config::EditorSettings;

    fn document(lines: usize) -> Document {
        Document::from_file(
            std::path::PathBuf::from("/w/a.txt"),
            &"x\n".repeat(lines),
            EditorSettings::default(),
        )
    }

    #[test]
    fn the_gutter_fits_the_longest_line_number() {
        // `line_count` counts the empty line after a trailing newline, so ten
        // `x\n` lines are eleven lines and still two digits.
        assert_eq!(gutter_width(&document(9)), 4, "10 lines: two digits");
        assert_eq!(gutter_width(&document(99)), 5, "100 lines: three digits");
    }

    #[test]
    fn a_short_file_still_gets_two_digits_of_gutter() {
        // Otherwise the text would shift when the file grows past nine lines and
        // everything would be redrawn.
        assert_eq!(gutter_width(&document(1)), 4);
    }

    #[test]
    fn line_numbers_off_costs_no_columns() {
        let mut document = document(10);
        document.settings.line_numbers = LineNumbers::Off;
        assert_eq!(gutter_width(&document), 0);
    }

    /// The regions of an 80x24 window, which is big enough for everything.
    fn roomy(side_bar: Option<SideBarLocation>, panel: bool) -> Regions {
        regions(80, 24, side_bar, panel)
    }

    #[test]
    fn nothing_showing_leaves_the_editor_the_whole_window() {
        let out = roomy(None, false);
        assert_eq!(
            out.editor,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 24
            }
        );
        assert_eq!(out.side_bar, None);
        assert_eq!(out.panel, None);
        assert_eq!(out.side_bar_rule, None);
        assert_eq!(out.panel_rule, None);
    }

    #[test]
    fn a_left_side_bar_takes_its_columns_and_a_rule() {
        let out = roomy(Some(SideBarLocation::Left), false);
        assert_eq!(
            out.side_bar,
            Some(Rect {
                x: 0,
                y: 0,
                width: SIDE_BAR_WIDTH,
                height: 24
            })
        );
        // The rule sits between them, in neither rectangle.
        assert_eq!(out.side_bar_rule, Some(SIDE_BAR_WIDTH));
        assert_eq!(out.editor.x, SIDE_BAR_WIDTH + 1);
        assert_eq!(out.editor.width, 80 - SIDE_BAR_WIDTH - 1);
        assert_eq!(
            out.editor.x + out.editor.width,
            80,
            "and together they fill the window exactly"
        );
    }

    #[test]
    fn a_right_side_bar_is_the_same_arithmetic_mirrored() {
        let out = roomy(Some(SideBarLocation::Right), false);
        assert_eq!(out.editor.x, 0);
        assert_eq!(out.editor.width, 80 - SIDE_BAR_WIDTH - 1);
        assert_eq!(out.side_bar_rule, Some(out.editor.width));
        assert_eq!(
            out.side_bar.expect("showing").x,
            out.editor.width + 1,
            "past the rule"
        );
        assert_eq!(out.side_bar.expect("showing").width, SIDE_BAR_WIDTH);
    }

    #[test]
    fn the_panel_comes_off_the_bottom() {
        let out = roomy(None, true);
        assert_eq!(out.editor.height, 24 - PANEL_HEIGHT - 1);
        assert_eq!(out.panel_rule, Some(out.editor.height));
        assert_eq!(
            out.panel,
            Some(Rect {
                x: 0,
                y: 24 - PANEL_HEIGHT,
                width: 80,
                height: PANEL_HEIGHT
            })
        );
    }

    #[test]
    fn the_side_bar_runs_past_the_panel() {
        // This matches VS Code. A terminal in the panel is as wide as the editor,
        // not as wide as the window.
        let out = roomy(Some(SideBarLocation::Left), true);
        let bar = out.side_bar.expect("showing");
        let panel = out.panel.expect("showing");

        assert_eq!(bar.height, 24, "full height, beside both");
        assert_eq!(
            panel.x, out.editor.x,
            "the panel starts where the text does"
        );
        assert_eq!(panel.width, out.editor.width);
    }

    #[test]
    fn a_region_gives_way_before_it_starves_the_editor() {
        // Narrow enough that the side bar cannot have its full width, but wide
        // enough that it can have some.
        let out = regions(45, 24, Some(SideBarLocation::Left), false);
        let bar = out.side_bar.expect("it still fits, squeezed");
        assert!(bar.width < SIDE_BAR_WIDTH);
        assert!(
            bar.width <= out.editor.width,
            "a region never takes more than it leaves: {} vs {}",
            bar.width,
            out.editor.width
        );
        assert_eq!(
            bar.width + 1 + out.editor.width,
            45,
            "and nothing is wasted"
        );
    }

    #[test]
    fn a_small_window_does_not_become_mostly_chrome() {
        // The half cap exists for this case. Without it, the panel would take
        // every row above the editor's minimum.
        let out = regions(80, 12, None, true);
        let panel = out.panel.expect("it fits");
        assert!(
            panel.height <= out.editor.height,
            "panel {} vs editor {}",
            panel.height,
            out.editor.height
        );
        assert!(out.editor.height > MIN_EDITOR_HEIGHT);
    }

    #[test]
    fn a_window_with_no_room_shows_no_region_at_all() {
        // `None` rather than an empty rectangle. It means "no room in this
        // window" and changes when the window grows. Hidden is a separate state
        // held by the session.
        let out = regions(28, 24, Some(SideBarLocation::Left), false);
        assert_eq!(out.side_bar, None);
        assert_eq!(out.side_bar_rule, None);
        assert_eq!(out.editor.width, 28, "the editor keeps all of it");

        let short = regions(80, 5, None, true);
        assert_eq!(short.panel, None);
        assert_eq!(short.editor.height, 5);
    }

    #[test]
    fn a_window_too_small_for_anything_does_not_underflow() {
        for (width, height) in [(0, 0), (1, 1), (2, 40), (40, 2), (200, 1), (1, 200)] {
            let out = regions(width, height, Some(SideBarLocation::Left), true);

            // No region may be larger than the window or extend past its edges.
            // An underflowed subtraction would show up here.
            assert!(out.editor.width <= width, "{width}x{height}");
            assert!(out.editor.height <= height, "{width}x{height}");
            for region in [out.side_bar, out.panel].into_iter().flatten() {
                assert!(region.x + region.width <= width, "{width}x{height}");
                assert!(region.y + region.height <= height, "{width}x{height}");
            }
            // A region that did show left the editor its minimum.
            if out.side_bar.is_some() {
                assert!(out.editor.width >= MIN_EDITOR_WIDTH, "{width}x{height}");
            }
            if out.panel.is_some() {
                assert!(out.editor.height >= MIN_EDITOR_HEIGHT, "{width}x{height}");
            }
        }
    }

    #[test]
    fn a_window_short_enough_to_refuse_the_panel_still_shows_the_side_bar() {
        // Each region is checked separately against the axis it takes space
        // from. A height too small for the panel does not affect the side bar.
        let out = regions(40, 2, Some(SideBarLocation::Left), true);
        assert_eq!(out.panel, None, "two rows leaves the editor nothing");
        assert!(
            out.side_bar.is_some(),
            "but forty columns has room for a squeezed one"
        );
        assert_eq!(out.editor.height, 2);
    }

    #[test]
    fn both_regions_together_still_leave_the_editor_its_minimum() {
        let out = regions(
            MIN_EDITOR_WIDTH + MIN_SIDE_BAR_WIDTH + 1,
            MIN_EDITOR_HEIGHT + MIN_PANEL_HEIGHT + 1,
            Some(SideBarLocation::Left),
            true,
        );
        assert_eq!(out.side_bar.expect("just fits").width, MIN_SIDE_BAR_WIDTH);
        assert_eq!(out.panel.expect("just fits").height, MIN_PANEL_HEIGHT);
        assert_eq!(out.editor.width, MIN_EDITOR_WIDTH);
        assert_eq!(out.editor.height, MIN_EDITOR_HEIGHT);
    }

    #[test]
    fn one_group_gets_the_whole_width() {
        assert_eq!(column_widths(80, 1), [80]);
    }

    #[test]
    fn two_groups_split_the_width_minus_a_separator() {
        assert_eq!(column_widths(81, 2), [40, 40]);
    }

    #[test]
    fn the_remainder_goes_to_the_left() {
        // 80 less one separator is 79, so one column is a cell wider.
        assert_eq!(column_widths(80, 2), [40, 39]);
        assert_eq!(column_widths(80, 3), [26, 26, 26]);
        assert_eq!(column_widths(81, 3), [27, 26, 26]);
    }

    #[test]
    fn a_width_too_small_to_divide_does_not_underflow() {
        assert_eq!(column_widths(1, 2), [0, 0]);
        assert_eq!(column_widths(0, 3), [0, 0, 0]);
    }
}
