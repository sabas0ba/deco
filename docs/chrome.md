# The side bar and the panel

![ctrl+b and ctrl+j opening the side bar and the panel, and typing still going into the text](img/chrome.svg)

deco divides its window into three regions: the **editor**, a **side bar** along one edge, and a **panel** along the bottom. `ctrl+b` and `ctrl+j` show and hide the side bar and panel. The commands use VS Code's identifiers.

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

The panel's rows are taken from the bottom before the side bar's columns are taken from the side. The side bar spans the full height beside both the editor and the panel, so the panel has the width of the editor rather than the window. This matches VS Code's layout.

The side bar requests 30 columns and the panel requests 10 rows. In smaller windows, each region shrinks or is omitted if its minimum size cannot fit. The layout preserves a minimum editor size and limits each region to at most half the available dimension.

A region that does not fit is **not the same as one that is hidden**. The requested visibility is kept, and deco reports that the window is too small:

```text
no room for the side bar in this window
```

When the window becomes large enough, the region appears without another keypress.

## Where the keyboard is

Focus is stored in the session, so the keymap can route keys as VS Code does. The context keys use VS Code's names:

| Key | Means |
| --- | --- |
| `sideBarVisible`, `panelVisible` | the region is on screen |
| `sideBarFocus`, `panelFocus` | it has the keyboard |
| `editorTextFocus`, `editorFocus`, `textInputFocus` | the **text** has it — false while a region does |

Visibility and keyboard focus are separate. `ctrl+b` shows the side bar while retaining editor focus. The animation above demonstrates typing into the document with both regions open.

While a region has keyboard focus, text commands such as typing, motion, undo and clipboard operations do not change the document. The check is in the commands rather than in a `when` clause on each binding, because unbound printable keys are inserted by a fallback that does not use the keymap. The text caret is hidden while another region has keyboard focus.

`workbench.action.focusActiveEditorGroup` returns focus to the editor. Hiding a region that has focus also returns focus to the editor.

## Settings

`workbench.sideBar.location` has the same meaning as in VS Code: `"left"` (the default) or `"right"`. It applies to the window and ignores per-language overrides, so switching between documents does not move the side bar.

There is no setting for the width. VS Code has no such setting either; it stores a width set by dragging. deco [writes no files](configuration.md) in which to store one.

## Both frontends

Both frontends use the same layout; only the units differ. The terminal renderer draws the regions in cells, and the GPU frontend multiplies the same rectangles by its font metrics. The GPU frontend draws the dividing rules with box-drawing characters rather than filled rectangles because it has no quad pipeline yet. For the same reason, it calculates selection positions but does not draw selections yet.

## Not built yet

The panel has no implemented views. `` ctrl+` `` (`workbench.action.terminal.toggleTerminal`) reports that the terminal is not implemented. Terminal support requires a PTY and terminal rendering. See the [roadmap](roadmap.md) for planned panel features.

A region cannot be resized or dragged to the other side. The setting is the only way to move the side bar. Persisting resized dimensions would also require storage for view state.
