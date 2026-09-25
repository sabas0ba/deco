# Tabs

deco holds one document per tab. The tab bar is shown when two or more documents are open.

![Opening a second file, switching, a refused close, and a successful one](img/tabs.svg)

| Key | Command |
| --- | --- |
| `ctrl+tab` | `workbench.action.nextEditor` |
| `ctrl+shift+tab` | `workbench.action.previousEditor` |
| `ctrl+w` | `workbench.action.closeActiveEditor` |
| `ctrl+n` | `workbench.action.files.newUntitledFile` |

All four are also in the command palette. `deco a.rs b.rs c.rs` opens each file in its own tab and focuses the first one.

## What a tab keeps

Each tab retains its text, **undo history**, cursor and scroll position, syntax highlighting state and language-server **diagnostics** when another tab becomes active. `ctrl+z` applies to the active document's undo history.

Each tab retains its find bar and matches while inactive. Two tabs can retain different searches.

The **search string is shared** between tabs even though the bar is not, as in VS Code. Opening find in another file shows the same query, and `F3` in a tab that has not been searched uses the last query.

Opening a *different document* in the same tab clears the matches, because they no longer apply.

While the editor is split, both groups show the same tab and therefore share the find state. Moving between groups closes the bar, because its current match is at the *other* group's cursor. A find bar per group requires a tab list per group, which is not implemented yet.

## The rules, and why

- **An open file is switched to, not opened twice.** In deco a tab *is* a document, with no separate view layer, so two tabs for one file would be two diverging copies. To show one file twice, use a split with [`ctrl+\`](#splitting), which gives two views of one buffer, as VS Code does. Paths are compared after resolving `.` and `..` segments, so `deco src/main.rs` and the same file chosen from `ctrl+p` use one tab. **Symlinks are not resolved**: two names for one file through a link open two tabs, as in VS Code. Detecting this would require filesystem access, which the core does not have.
- **A dirty tab refuses to close and names the file**: `main.rs has unsaved changes — save it first`. [Reverting](#throwing-changes-away) discards the changes so the tab can be closed.
- **Closing the last tab leaves an untitled document.** The session always shows a document.
- **Opening a file replaces a pristine untitled tab** (unmodified, unnamed and empty) rather than opening beside it. This is VS Code's rule, and it prevents `deco file.rs` from starting with an empty tab next to the file.
- The dirty marker in the bar is the same `*` used in the status bar.

## Language servers follow the active tab

Switching tabs tells the server which file is on screen (`didClose`/`didOpen`). Switching to a file in a different language switches to that language's server. A server publishes diagnostics for every file it knows about, but only the visible document's diagnostics are displayed. When a tab becomes active again, its diagnostics are taken from the stored set.

**Go-to-definition across files now opens a new tab** (or switches to the tab already holding the file). It previously refused to jump while the current document had unsaved changes, because jumping replaced the document. With tabs, the current document is kept, so the restriction has been removed.

## Colours

The bar uses the theme's tab colour keys (`tab.activeBackground`, `tab.activeForeground`, `tab.inactiveBackground`, `tab.inactiveForeground`, `editorGroupHeader.tabsBackground`), with fallbacks for themes that do not set them.

## Saving several at once

`ctrl+k s` writes every tab with unsaved changes and reports how many were written.

![Editing two tabs and saving both with ctrl+k s](img/save-all.svg)

Each write result is reported individually, so a failed write leaves *that* tab dirty instead of the whole batch being marked saved. The failure reason is stored where it can be read later, because the one-line status bar cannot show several failures.

A dirty **untitled** document is counted and skipped, because it has no filename to write to and deco does not invent one.

Each tab is written with its **own** settings, not the active tab's. `files.insertFinalNewline` can be set per language, so a batch that saves a `.md` and a `.txt` applies each file's configuration.

| Key | Command |
| --- | --- |
| `ctrl+s` | `workbench.action.files.save` |
| `ctrl+k s` | `workbench.action.files.saveAll` |

The loop and its reporting are in the core, and only the write itself is in the frontend. Both frontends therefore report the same results for the same batch, and the behaviour is tested without a filesystem.

## Saving somewhere else

`ctrl+shift+s` opens a Save As prompt containing the current path. Edit that path or use `ctrl+x` to clear the field before entering another destination.

![Saving notes.txt as Cargo.toml, which makes it TOML](img/save-as.svg)

Saving under a new name reruns language detection. For example, saving `notes.txt` as `notes.toml` enables the TOML lexer and `[toml]` settings. A language selected manually with `ctrl+k m` remains selected.

A relative path is resolved against the workspace root and `~` is expanded, so `~/notes.md` and `docs/notes.md` both work. Relative paths do not depend on the directory deco was launched from.

| Key | Command |
| --- | --- |
| `ctrl+s` | `workbench.action.files.save` |
| `ctrl+k s` | `workbench.action.files.saveAll` |
| `ctrl+shift+s` | `workbench.action.files.saveAs` |

**`ctrl+s` on an untitled document opens the save-as prompt**, as in VS Code. deco does not generate a filename for it.

The prompt returns the path **exactly as typed**. The frontend resolves it, writes the file, and reports the resolved path. Resolving requires a home directory and a working directory, which the core does not have.

## Throwing changes away

`Revert File` in the palette re-reads the document from disk, and `Revert and Close Editor` also closes it. Neither has a default key, as in VS Code.

**Reverting re-reads the file instead of using a stored copy.** Storing a second copy of every open file would double the memory used by large files, and re-reading also picks up changes made to the file on disk.

Reverting creates an undo entry, so `ctrl+z` restores the previous buffer contents. If reading the file fails, the buffer remains unchanged.

An **untitled** document reverts to empty, since there is no file to re-read and it started empty. This is also how to close a scratch buffer without saving it.

## Quitting with work unsaved

`ctrl+q` refuses once and lists the unsaved documents, for example `2 tabs have unsaved changes: a.txt, b.rs`. A second `ctrl+q` quits anyway.

The second `ctrl+q` must be the **next keystroke**. Any intervening key cancels the pending quit confirmation.

The session performs the check over every tab, not only the visible one, so both frontends use it. This applies the same protection as `ctrl+w` to quitting.

## Splitting

`ctrl+\` opens a second view of the file beside the first, and `ctrl+1` / `ctrl+2` move keyboard focus between them.

![Splitting, scrolling one group, editing, and closing the split](img/split.svg)

**One buffer, two views.** Two documents would be two diverging copies of one file, and the last one saved would overwrite the other. An edit in either group therefore appears in both, and there is one undo history. Each group keeps its own **scroll position and cursor**. Scrolling the second group to the end of a function leaves the first at the top.

The new group receives keyboard focus.

`ctrl+w` closes the second group before it closes any tab, so after a split it first restores the single view. Moving between groups closes the find bar, because its matches were found in the other view and its current match is at that group's cursor.

| Key | Command |
| --- | --- |
| `ctrl+\` | `workbench.action.splitEditor` |
| `ctrl+1` / `ctrl+2` | `workbench.action.focusFirstEditorGroup` / `…Second…` |

Each column is drawn with its own gutter, and the widths differ by at most one cell. A rule marks the boundary, because a blank column would look like part of the file with shorter lines.

**Two groups, and both show the same file.** A third group, or two groups with *different* files, requires a tab list per group. Currently the session has one list, which both groups use. `ctrl+3` reports that there is no third group.

## Not built yet

There is no mouse support; tabs and groups are switched from the keyboard. A bar wider than the terminal is truncated rather than scrolled, and every tab is still reachable with `ctrl+tab`. The GPU frontend switches tabs but does not draw the bar, because it has no chrome yet, and it draws one group rather than two.
