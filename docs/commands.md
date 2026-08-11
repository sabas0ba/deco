# Running commands

Every action in deco is a command with VS Code's identifier —
`editor.action.commentLine`, not `deco.comment` — which is what lets a rebinding
in your own `keybindings.json` reach the same command deco runs by default. Two
keys let you reach one without a binding at all.

## The command palette

`ctrl+shift+p` (or `F1`) lists every command the editor can run, filtered as you
type. `up` / `down` move the selection, `enter` runs it, `escape` closes.

![Filtering the command palette and running Toggle Line Comment](img/command-palette.svg)

Each row shows the title on the left and the command identifier on the right,
because the identifier is what a `keybindings.json` refers to — so the palette
doubles as the answer to "what do I bind this to?". On a narrow terminal the
identifier is dropped and the title kept.

Filtering matches, best first:

| Rank | Example |
| --- | --- |
| The title starts with what you typed | `go` → **Go** to Line |
| A word of the title starts with it | `line` → Toggle **Line** Comment |
| The title contains it | `omment` → Toggle Line C**omment** |
| The identifier contains it | `commentLine` → Toggle Line Comment |
| The title's letters appear in order | `gtl` → **G**o **t**o **L**ine |

The selection follows the **same command** as the list narrows, rather than
staying on the same row. That matters more here than in a completion list: the
next key runs whatever is selected, and a keystroke that silently moved the
selection onto a different command would be a way to run the wrong one.

If nothing matches, `enter` says so instead of closing quietly, which would look
like the command had run.

### What the palette offers

Only commands that work. The list is assembled from two places: this crate's own
commands, and the ones the running frontend implements. The core cannot know
whether a command it hands onward will be handled — the terminal frontend can
format a document because it has a language-server client, and the GPU frontend
cannot because it has neither — so each frontend declares what it can do. Nothing
is listed on the assumption that somebody downstream will handle it.

A test asserts that every entry actually resolves to a command, because the
registry is a list of strings beside a `match` on strings and the two could
otherwise drift.

Motions are deliberately left out. `cursorDown` is a keypress, not something
anyone looks up by name, and listing forty of them would bury the commands people
do look for.

## Quick open

`ctrl+p` lists the files in the workspace and opens the one you pick, in a new
tab.

![Filtering the file list and opening one in a new tab](img/quick-open.svg)

Filtering runs against the path shown, so `conf` finds `src/config/mod.rs` by its
directory and `main` finds `src/main.rs` by its name. The same ranking as the
palette applies, so the file name is matched before the rest of the path — which
is the order people think in.

The workspace is the directory of the file deco was started with, or the working
directory when it was started with none. The walk:

- skips what `files.exclude` excludes, using VS Code's own glob dialect, and
  honours a pattern set to `false` as the disabled pattern it means;
- always skips `.git`, `node_modules`, `target`, `dist`, `build`, `.venv`,
  `__pycache__` and friends. These are conventions rather than configuration, and
  a build directory is exactly what makes a walk slow and its results useless;
- stops at 10,000 files or 24 directories deep, **and says so** — a quick open
  that silently omitted the file you wanted would be worse than one that admits
  it ran out of room. The depth limit also bounds a symlink loop, since
  `read_dir` follows links.

The list is walked fresh on every `ctrl+p`, so a file created a moment ago is
there. It is sorted by path, because `read_dir` guarantees no order and a list
that reshuffles between presses is one you cannot learn.

The core does not do this walk: it has no filesystem at all — a document is
handed its text, never a path to read — which is what lets the whole editable
surface be tested without one. `ctrl+p` therefore asks the frontend for the list,
and accepting a choice asks the frontend to read the file, exactly as saving asks
it to write one.

## Go to line

`ctrl+g` asks for a line number.

![Jumping to line 4 with ctrl+g](img/go-to-line.svg)

It takes `12` and `12:5` — VS Code's `line:column` — because the status bar
reports both and whoever read one has usually read the other. Lines and columns
are one-based, as the status bar shows them.

A column past the end of its line lands at the end of the line, which is what was
meant. A line outside the document is refused with the range it should have been
in: `line 99 is outside 1-42`. "Out of range" without the range leaves you
guessing.

## A prompt is a text input

Both of these are the same widget, and it behaves like the find bar because it is
built from the same one-line input:

- `ctrl+v` pastes into the prompt, not into the file. `ctrl+z`, `ctrl+a` and
  `ctrl+x` are swallowed rather than reaching a document you are not looking at.
- `editorTextFocus` goes false while a prompt is open, so `tab`, `ctrl+space` and
  the rest stop resolving at all. The context key is VS Code's `inQuickOpen`.
- The caret is drawn in the prompt, because that is where typing goes.

## Not built yet

Search-in-files (`ctrl+shift+f`), the keyboard-shortcuts editor and the settings
UI. Those keys report themselves as unimplemented rather than doing nothing.
Quick open has no symbol mode (`ctrl+p` then `@`) and no recently-opened ordering
— the list is alphabetical.

The GPU frontend has no chrome to draw a prompt in, so it refuses `ctrl+g`,
`ctrl+shift+p` and `ctrl+p` and says so — an invisible widget holding the keyboard
looks exactly like an editor that has stopped responding.
