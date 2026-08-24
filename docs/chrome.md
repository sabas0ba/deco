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

**The regions are empty, and say so.** Nothing lives in them yet: the file tree,
search and source control belong in the side bar, and the terminal, problems and
output in the panel. Each region draws its name and what it is waiting for,
which is the same rule deco applies to a key it has not implemented — the chrome
is real, what goes in it is named, and neither is pretended about. This page will
grow a section per tenant as they arrive.

## Where the space comes from

The frontend hands the session a rectangle and the session divides it. That is
the part worth doing carefully, because the answer feeds the renderer *and* the
wrap: where a line breaks depends on how many columns are left for text, so a
session that did not know its own layout could only wrap by asking a frontend —
and then the two would be free to disagree, which shows up as a caret one column
off the text it belongs to.

The panel comes off the bottom before the side bar comes off the side, so the
side bar runs the full height beside both. That is VS Code's arrangement, and
the reason a terminal in the panel is as wide as the editor rather than as wide
as the window.

**A region gives way before it starves the editor.** It asks for a fixed size —
30 columns, 10 rows — and takes less on a small window, down to a floor below
which it is not shown at all. Two rules bound it: the editor keeps a minimum,
and no region takes more than it leaves. Without the second, a panel on a
twelve-row terminal takes every row above the editor's minimum and leaves a slot
to read code through.

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

Visible and focused are deliberately separate. `ctrl+b` shows the side bar
**without** taking the keyboard into it, exactly as VS Code's does: showing the
tree should not cost you your place in the file. The animation above types into
the document with both regions open, which is what that looks like.

While a region has the keyboard, the editor's own commands do not reach the
document — typing, motion, undo, the clipboard. They act on the text, and the
text is not what has focus. That is enforced on the command rather than as a
`when` clause on each binding, because the fallback that types an unbound
printable key never goes through the keymap at all and a clause could not reach
it. The caret disappears from the text while it lasts: two carets, or one where
typing does not go, is a lie about where the next keystroke lands.

`workbench.action.focusActiveEditorGroup` is the way back, and hiding a region
that has the keyboard gives it back on its own.

## Settings

`workbench.sideBar.location` is read with VS Code's meaning — `"left"` (the
default) or `"right"`. It is not a per-language setting: which side the chrome is
on belongs to the window, and a side bar that jumped across the screen when you
switched tabs would be answering a question nobody asked.

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

The regions have no tenants. `` ctrl+` `` (`workbench.action.terminal.toggleTerminal`)
still reports that it is not implemented: the panel it would open into exists
now, and what is missing is a PTY to put in it. The
[roadmap](roadmap.md) has the plan for each of them, and the file tree is next.

A region cannot be resized or dragged to the other side; the setting is the only
way to move the side bar. Tenants that need to remember a width will need
somewhere to remember it, which is the same question as everything else deco
declines to write down.
