# The side bar and the panel

![ctrl+b and ctrl+j opening the side bar and the panel, and typing still going into the text](img/chrome.svg)

deco divides its window into three regions: the **editor**, a **side bar** down
one edge, and a **panel** across the bottom. `ctrl+b` and `ctrl+j` show and hide
them, with VS Code's own command identifiers.

| Key | Command |
| --- | --- |
| `ctrl+b` | `workbench.action.toggleSidebarVisibility` |
| `ctrl+j` | `workbench.action.togglePanel` |
| | `workbench.action.focusSideBar` |
| | `workbench.action.focusPanel` |
| | `workbench.action.focusActiveEditorGroup` |
| | `workbench.action.closeSidebar`, `workbench.action.closePanel` |

The side bar displays the [file tree](files.md) or [source-control view](git.md). The panel has no implemented views yet. Empty regions display a label identifying the planned view. This page describes region layout and keyboard focus; feature pages describe the views within those regions.

## Where the space comes from

The frontend supplies the available rectangle, and the session calculates the editor, side-bar and panel rectangles. Rendering and word wrapping use this same layout so text and cursor positions agree.

The panel comes off the bottom before the side bar comes off the side, so the
side bar runs the full height beside both. That is VS Code's arrangement, and
the reason a terminal in the panel is as wide as the editor rather than as wide
as the window.

The side bar requests 30 columns and the panel requests 10 rows. In smaller windows, each region shrinks or is omitted if its minimum size cannot fit. The layout preserves a minimum editor size and limits each region to at most half the available dimension.

A region that does not fit is **not the same as one that is hidden**. The
visibility you asked for is remembered, the window is simply too small to honour
it, and it says so:

```text
no room for the side bar in this window
```

Widen the window and it appears, with no second keypress.

## Where the keyboard is

Focus is part of the session, so the keymap can route keys the way VS Code does.
The context keys are VS Code's:

| Key | Means |
| --- | --- |
| `sideBarVisible`, `panelVisible` | the region is on screen |
| `sideBarFocus`, `panelFocus` | it has the keyboard |
| `editorTextFocus`, `editorFocus`, `textInputFocus` | the **text** has it — false while a region does |

Visibility and keyboard focus are separate. `ctrl+b` shows the side bar while retaining editor focus. The animation above demonstrates typing into the document with both regions open.

While a region has the keyboard, the editor's own commands do not reach the
document — typing, motion, undo, the clipboard. They act on the text, and the
text is not what has focus. That is enforced on the command rather than as a
`when` clause on each binding, because the fallback that types an unbound
printable key never goes through the keymap at all and a clause could not reach
it. The text caret is hidden while another region has keyboard focus.

`workbench.action.focusActiveEditorGroup` is the way back, and hiding a region
that has the keyboard gives it back on its own.

## Settings

`workbench.sideBar.location` is read with VS Code's meaning — `"left"` (the
default) or `"right"`. It applies to the window and ignores per-language overrides, so switching between documents does not move the side bar.

There is no setting for the width. VS Code has none either — it remembers a
width you dragged, and deco [writes no files](configuration.md) to remember one
in.

## Both frontends

The split is shared; only the units differ. The terminal renderer draws the
regions in cells, and the GPU frontend multiplies the same rectangles by its font
metrics. The GPU frontend paints the rules with the same box-drawing characters
rather than as filled rectangles, because it has no way to fill one yet — there
is no quad pipeline in it, which is also why selections are laid out there but
not yet drawn.

## Not built yet

The panel has no implemented views. `` ctrl+` `` (`workbench.action.terminal.toggleTerminal`) reports that the terminal is not implemented. Terminal support requires a PTY and terminal rendering. See the [roadmap](roadmap.md) for planned panel features.

A region cannot be resized or dragged to the other side; the setting is the only
way to move the side bar. Persisting resized dimensions would also require storage for view state.
