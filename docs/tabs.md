# Tabs

deco holds one document per tab and shows a tab bar as soon as there are two.
With a single document open, nothing changes — the bar earns its row only once
there is a choice to show.

![Opening a second file, switching, a refused close, and a successful one](img/tabs.svg)

| Key | Command |
| --- | --- |
| `ctrl+tab` | `workbench.action.nextEditor` |
| `ctrl+shift+tab` | `workbench.action.previousEditor` |
| `ctrl+w` | `workbench.action.closeActiveEditor` |
| `ctrl+n` | `workbench.action.files.newUntitledFile` |

All four are also in the command palette. `deco a.rs b.rs c.rs` opens each file
in its own tab, with the first one focused.

## What a tab keeps

Everything that should survive a round trip through the background: the text and
its **undo history**, the cursor and scroll position, the syntax highlighting
state, and the **diagnostics** a language server has published for it. Switch
away, switch back, and press `ctrl+z` — the edit you made there is undone, not
one from another file.

The find bar deliberately does not survive a switch. Its match list describes
text that is no longer on screen, and a stale highlight is worse than none — the
same rule as when a file replaces the document. The query is kept, so `F3` still
knows what to search for.

## The rules, and why

- **A file already open is switched to, not opened twice.** Two tabs onto one
  file would be two divergent copies of it, and whichever was saved last would
  silently win.
- **A dirty tab refuses to close, by name**: `main.rs has unsaved changes — save
  it first`. deco has no dialog to ask with, and losing edits to a keystroke is
  the worst thing an editor can do.
- **Closing the last tab leaves an untitled document.** The session always shows
  something.
- **Opening a file replaces a pristine untitled tab** — untouched, unnamed,
  empty — rather than sitting beside it. That is VS Code's rule, and it is what
  keeps `deco file.rs` from starting with an empty tab next to the file.
- The dirty marker in the bar is the same `*` the status bar uses, so the two
  read as one vocabulary.

## Language servers follow the active tab

Switching tabs tells the server which file is on screen (`didClose`/`didOpen`),
and switching to a file of a different language switches to that language's
server. A server publishes diagnostics for every file it knows about all along;
only the visible document's reach the screen, so a tab returning from the
background collects what it missed from the stored set.

**Go-to-definition across files now opens a new tab** (or switches to the tab
already holding the file). It previously refused to jump while the current
document had unsaved changes, because jumping replaced the document — with tabs,
nothing is at risk and the refusal is gone.

## Colours

The bar uses the theme's own tab keys — `tab.activeBackground`,
`tab.activeForeground`, `tab.inactiveBackground`, `tab.inactiveForeground`,
`editorGroupHeader.tabsBackground` — with sensible fallbacks for themes that do
not set them.

## Saving several at once

`ctrl+k s` writes every tab with unsaved changes, and reports how many.

![Editing two tabs and saving both with ctrl+k s](img/save-all.svg)
 Each write
is reported back individually, so one that fails leaves *that* tab dirty rather
than marking the batch saved — a tab that looks saved and is not is how work gets
lost. The reason goes where a reader can find it, since a status bar has one line
and several failures would each shorten the last.

A dirty **untitled** document is counted and skipped: there is no filename to
write to, and inventing one would put your work somewhere you did not ask for.

Each tab is written through its **own** settings, not the active tab's:
`files.insertFinalNewline` can be set per language, so a batch that saves a
`.md` and a `.txt` gives each the ending its own configuration asks for.

| Key | Command |
| --- | --- |
| `ctrl+s` | `workbench.action.files.save` |
| `ctrl+k s` | `workbench.action.files.saveAll` |

The loop and its reporting live in the core, and only the write itself belongs to
the frontend — so both frontends say the same thing about the same batch, and the
behaviour is tested with no filesystem involved.

`ctrl+shift+s` (save as) is **not** built: it needs a filename prompt and a tab
that can be renamed under it. It says so when pressed.

## Not built yet

No splits and no mouse: tabs are switched from the keyboard. A bar wider than
the terminal truncates rather than scrolling — every tab is still reachable with
`ctrl+tab`. The GPU frontend switches tabs but does not draw the bar, because it
has no chrome to draw it in yet.
