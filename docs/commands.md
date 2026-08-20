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

The selection is the **best match** for what has been typed, re-chosen on every
keystroke, as VS Code's quick pick does. Type a few letters and press `enter`.

Following the previously selected entry instead was tried, to stop a keystroke
moving the selection onto a different command. It cut the other way: the selection
starts on row 0, which nobody chose — it is whatever the registry listed first — so
if that entry still matched at all it stayed selected however badly it ranked, and
`enter` ran something the user never looked at while the entry they were typing
towards sat at the top.

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

### Files you have had open come first

The file you want is usually one you just had open, and an alphabetical list buries
it. So the rows are ordered most-recently-on-screen first, then by path — which is
how VS Code orders quick open, and most of what makes the key fast. The last frame
above is the second `ctrl+p`, with the two files that have been on screen at the top.

Recency **orders equal matches and no more than that**: a row that matches what you
typed better still comes first, so typing does not fight the ordering. A file that
was closed is still remembered, since it is exactly the one you are most likely to
want back.

Paths are compared with their `.` and `..` resolved, because the walk and `ctrl+o`
spell the same file differently — `src/main.rs` against `./src/main.rs` — and a
recent file the list failed to recognise would sink back into the alphabet without
saying why.

**This session only.** VS Code keeps its history in workspace storage; deco
[writes no files](configuration.md#colour-themes), so the list starts empty each
time rather than being persisted somewhere you did not ask for. Sixty-four paths are
remembered; past that the tail falls back to the alphabet.

The core does not do this walk: it has no filesystem at all — a document is
handed its text, never a path to read — which is what lets the whole editable
surface be tested without one. `ctrl+p` therefore asks the frontend for the list,
and accepting a choice asks the frontend to read the file, exactly as saving asks
it to write one.

## Commands an extension contributes

An installed code extension's commands are in the palette beside deco's own, with the
extension's name in the right-hand column instead of the identifier. Choosing one
starts that extension in a sandboxed host and runs it — see
[Running one](extensions.md#running-one).

Nothing about them is special once they are listed: they match as you type like any
other entry, and an extension that has not started yet is started by being chosen.

## Colour theme

`ctrl+k ctrl+t` switches theme. The list and the loading belong to the frontend,
since a contributed theme is a file in an extension directory — see
[Colour themes](configuration.md#colour-themes).

## Change language mode

`ctrl+k m` says what language this document is, which decides the lexer, the
`[language]` settings that apply, and which language server runs. It is documented
with the highlighting it drives — see
[Choosing the language yourself](highlighting.md#choosing-the-language-yourself).

## Go to symbol

`ctrl+shift+o` lists the names the language server found in the file being edited
and takes you to the one you pick.

![Filtering a document's symbols and jumping to one](img/go-to-symbol.svg)

Rows read `Counter.bump` rather than `bump`, so filtering can find a method by its
class, and two `new`s in one file are told apart. The right-hand column is the
symbol's **kind** — `struct`, `field`, `method` — which is what distinguishes a
field from a method of the same name. A kind this client does not recognise leaves
the column empty rather than dropping the symbol: a newer protocol than deco is no
reason to hide a name.

The list is in **document order**, not alphabetical: it is the order the file reads
in, and it is what VS Code's own picker shows. Every other prompt breaks ties by
title instead — a command list would otherwise be ordered by however the registry
happened to be written.

The key is gated on `editorHasDocumentSymbolProvider`, so it does not resolve at
all without a server that can answer — a key that reports nothing rather than one
that reports a failure. See [Language servers](language-servers.md#go-to-symbol).

## Opening a path

`ctrl+o` types a path instead of choosing from a list, which is what you want for a
file outside the workspace — quick open only offers what it walked. The prompt is
seeded with the current file's **directory**, not its name: the point is to open
something else, and a seed whose last component you have to delete is a seed that
cost you. This is the one seeded prompt that opens *unselected*, because its seed
is a prefix to continue rather than an answer to replace.

`~` expands and a relative path is taken against the workspace root, the same
resolution save-as uses.

| Key | Command |
| --- | --- |
| `ctrl+o` | `workbench.action.files.openFile` |

`workbench.action.files.openFolder` is still pending: a new workspace root is what
the file walk, the search and the language servers are all anchored to.

## Search in files

`ctrl+shift+f` asks what to look for, searches every file under the workspace root
and lists what it found; `enter` opens that file at that line.

![Searching the workspace and opening a result](img/search-in-files.svg)

| Key | Command |
| --- | --- |
| `ctrl+shift+f` | `workbench.action.findInFiles` |
| `alt+c` / `alt+w` | `toggleFindCaseSensitive` / `toggleFindWholeWord` |

The query field is seeded with the selection, the word under the cursor, or
whatever the find bar last searched for — in that order, because that is the order
of how recently you said it. It is a seed and not the query: it opens **selected**,
so typing replaces it and searching for something the cursor is nowhere near is
just typing it. An empty query is refused with `nothing to search for` instead of
walking the workspace for a match everything has.

`alt+c` and `alt+w` toggle **this search's** options while the field is open, and
the find bar's while `ctrl+f` is open. The two are independent, as they are in VS
Code: case-sensitivity set for a search across the project has no business
changing what the next `ctrl+f` matches. Since the prompt has one line and no room
to draw the state, each toggle reports it — `Search: case on, whole word off` — on
the grounds that a toggle nobody can see is a toggle nobody trusts.

The keys are bound twice over, once on `findWidgetVisible` and once on
`searchViewletVisible`, which are VS Code's own context-key names for the find bar
and the search view being up. A `when` clause copied out of somebody's
`keybindings.json` gates on the same thing it gates on there.

Each row is `path:line: the line's text`, which is also what the filter matches —
so typing `report` narrows four results to the one in `src/report.rs`.

The search is **synchronous and bounded**: it stops at 500 matches, skips files
over 1 MiB and files that are not text (which is how a binary presents itself),
and honours `files.exclude` and the conventional skips exactly as quick open does.
When a limit stops it, the status bar says how many it found *and that there may
be more*.

That honesty is the point. A streaming search that fills a panel as it goes needs
a thread and a results view that updates, and this needs neither to be useful —
but a search that quietly stopped at 500 and let you believe that was all of them
would be worse than no search.

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
  `ctrl+x` are swallowed rather than reaching a document you are not looking at —
  `ctrl+a` selects the prompt's own line, so the next key replaces it.
- `editorTextFocus` goes false while a prompt is open, so `tab`, `ctrl+space` and
  the rest stop resolving at all. The context key is VS Code's `inQuickOpen`.
- The caret is drawn in the prompt, because that is where typing goes.

## A bound key never does nothing

Every command the default keymap binds either runs or **says why it does not**:

- A feature deco means to build names itself — `Split Editor is not implemented
  yet` — from a list of such commands in `deco-editor::commands::PENDING`.
- An identifier that does not exist here says *that* instead: `there is no command
  \`editor.action.nonsens\``. A different fact, and usually a typo in somebody's
  `keybindings.json` rather than a missing feature.

A test walks the whole default keymap and fails if any binding answers neither, so
a dead key cannot be added by accident. Nothing on the pending list is offered in
the palette: an entry there has to work when chosen, and one that only apologises
is worse than a shorter list.

What is on that list today: the side bar, panel, terminal and zen mode, zoom, open
folder, the settings and keyboard-shortcut editors, rename and quick fix, and the
remote menu.

## Not built yet

The keyboard-shortcuts editor and the settings UI. Quick open has no `@` mode — the
symbols of a file are reachable with `ctrl+shift+o`, but typing `@` after `ctrl+p`
does not switch a file list into a symbol one, which needs the prompt to re-source
its choices mid-typing.
Search in files has no replace-across-files and no regular expressions. Nor is
there a results view that stays open: the matches are a picker, so reading the
second one means pressing `ctrl+shift+f` again.

The GPU frontend has no chrome to draw a prompt in, so it refuses `ctrl+g`,
`ctrl+shift+p` and `ctrl+p` and says so — an invisible widget holding the keyboard
looks exactly like an editor that has stopped responding. `ctrl+shift+o` does not
arise there at all: it is gated on a language-server capability, and the GPU
frontend has no client to report one.
