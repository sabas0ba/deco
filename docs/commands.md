# Running commands

Every action in deco is a command with VS Code's identifier, for example `editor.action.commentLine` rather than `deco.comment`. A binding in your `keybindings.json` therefore refers to the same command that deco runs by default. Two keys run commands without a binding.

## The command palette

`ctrl+shift+p` (or `F1`) lists every command the editor can run, filtered as you
type. `up` / `down` move the selection, `enter` runs it, `escape` closes.

![Filtering the command palette and running Toggle Line Comment](img/command-palette.svg)

Each row shows the title on the left and the command identifier on the right. The identifier is the value to use in `keybindings.json`. On a narrow terminal the identifier is omitted and the title is kept.

Filtering matches, best first:

| Rank | Example |
| --- | --- |
| The title starts with what you typed | `go` → **Go** to Line |
| A word of the title starts with it | `line` → Toggle **Line** Comment |
| The title contains it | `omment` → Toggle Line C**omment** |
| The identifier contains it | `commentLine` → Toggle Line Comment |
| The title's letters appear in order | `gtl` → **G**o **t**o **L**ine |

The selection moves to the **best match** after every keystroke, as in VS Code's quick pick. Type a few letters and press `enter`.

An earlier version kept the previously selected entry selected while it still matched. Because the initial selection is row 0, which is the first registry entry rather than a user choice, a poorly ranked entry could stay selected and `enter` could run it instead of the top match.

If nothing matches, `enter` reports that no command matched instead of closing the palette, which would look as if a command had run.

### What the palette offers

The palette lists only commands that can run. The list combines the core's own commands with the commands implemented by the running frontend. The core cannot tell whether a forwarded command will be handled, so each frontend declares its commands. For example, the terminal frontend can format a document because it has a language-server client, and the GPU frontend cannot because it has no such client.

A test checks that every entry resolves to a command. The registry is a list of strings next to a `match` on strings, and the two could otherwise diverge.

Cursor motions are not listed. Commands such as `cursorDown` are used through keys rather than by name, and listing forty of them would make other commands harder to find.

## Quick open

`ctrl+p` lists the files in the workspace and opens the one you pick, in a new
tab.

![Filtering the file list and opening one in a new tab](img/quick-open.svg)

Filtering matches the displayed path, so `conf` finds `src/config/mod.rs` by its directory and `main` finds `src/main.rs` by its name. The palette's ranking applies, so matches in the file name rank before matches in the rest of the path.

The workspace is the directory of the file deco was started with, or the working
directory when it was started with none. The walk:

- skips paths excluded by `files.exclude`, using VS Code's glob syntax, and treats a pattern set to `false` as disabled;
- always skips `.git`, `node_modules`, `target`, `dist`, `build`, `.venv`, `__pycache__` and similar directories. These are fixed conventions rather than configuration, because build and dependency directories make the walk slow and fill the results with irrelevant files;
- stops at 10,000 files or 24 directories deep, **and reports it**, so a missing file is not silently omitted. The depth limit also bounds symlink loops, since `read_dir` follows links.

The list is rebuilt on every `ctrl+p`, so newly created files are included. It is sorted by path, because `read_dir` does not guarantee an order and the list should be stable between invocations.

### Files you have had open come first

Rows are ordered by how recently each file was on screen, then by path. VS Code orders quick open the same way. The last frame above is the second `ctrl+p`, with the two files that have been on screen at the top.

Recency **only orders equal matches**. A row that matches the typed text better still ranks first. Closed files are still included in the recency order.

Paths are compared after resolving `.` and `..`. The walk and `ctrl+o` can spell the same file differently, such as `src/main.rs` and `./src/main.rs`, and without normalisation a recent file would lose its recency position.

**This session only.** VS Code keeps this history in workspace storage. deco [writes no files](configuration.md#colour-themes), so the list starts empty in each session. Sixty-four paths are remembered; older entries are ordered by path.

The core does not perform the walk because it has no filesystem access. A document receives its text, not a path to read, so the editing code can be tested without a filesystem. `ctrl+p` asks the frontend for the list, and accepting a choice asks the frontend to read the file, in the same way that saving asks it to write one.

## Commands an extension contributes

Commands from installed code extensions are listed in the palette with deco's commands. The right-hand column shows the extension's name instead of the identifier. Choosing one starts that extension in a sandboxed host and runs the command. See [Running one](extensions.md#running-one).

These entries are filtered like any other entry. If the extension has not started yet, choosing one of its commands starts it.

## Colour theme

`ctrl+k ctrl+t` switches theme. The frontend builds the list and loads the theme, because a contributed theme is a file in an extension directory. See [Colour themes](configuration.md#colour-themes).

## Change language mode

`ctrl+k m` sets the document's language. The language determines the lexer, the applicable `[language]` settings, and which language server runs. See [Choosing the language yourself](highlighting.md#choosing-the-language-yourself).

## Go to symbol

`ctrl+shift+o` lists the symbols the language server reports for the current file and moves the cursor to the selected one.

![Filtering a document's symbols and jumping to one](img/go-to-symbol.svg)

Rows show qualified names such as `Counter.bump` rather than `bump`, so filtering can find a method by its class and two `new` symbols in one file can be distinguished. The right-hand column shows the symbol's **kind**, such as `struct`, `field` or `method`, which distinguishes a field from a method with the same name. If deco does not recognise a kind, the column is empty and the symbol is still listed, so symbols from newer protocol versions remain visible.

The list is in **document order**, not alphabetical, as in VS Code's picker. Other prompts break ties by title, because a command list would otherwise follow the registry's source order.

The key is gated on `editorHasDocumentSymbolProvider`. Without a server that provides symbols, the key does not resolve and no error is reported. See [Language servers](language-servers.md#go-to-symbol).

## Opening a path

`ctrl+o` opens a file by typed path instead of from a list. Use it for files outside the workspace, which quick open does not list. The prompt is pre-filled with the current file's **directory**, not its name, because the command is used to open a different file. This is the only pre-filled prompt whose text is *not selected*, because the text is a prefix to extend rather than a value to replace.

`~` is expanded and a relative path is resolved against the workspace root, as in save-as.

| Key | Command |
| --- | --- |
| `ctrl+o` | `workbench.action.files.openFile` |

`workbench.action.files.openFolder` is still pending. Changing the workspace root affects the file walk, search and language servers, which all use that root.

## Search in files

`ctrl+shift+f` prompts for a query, searches every file under the workspace root and lists the matches. `enter` opens the selected file at the matching line.

![Searching the workspace and opening a result](img/search-in-files.svg)

| Key | Command |
| --- | --- |
| `ctrl+shift+f` | `workbench.action.findInFiles` |
| `ctrl+shift+h` | `workbench.action.replaceInFiles` — see [Find and replace](find-and-replace.md#replacing-across-the-workspace) |
| `alt+c` / `alt+w` | `toggleFindCaseSensitive` / `toggleFindWholeWord` |

The query field is pre-filled with the selection, the word under the cursor, or the find bar's last query, in that order of precedence. The pre-filled text is **selected**, so typing replaces it. An empty query is rejected with `nothing to search for` instead of matching every position in the workspace.

`alt+c` and `alt+w` toggle **the search's** options while the field is open, and the find bar's options while `ctrl+f` is open. The two sets of options are independent, as in VS Code, so a project-wide search setting does not change the next `ctrl+f`. The prompt has one line and cannot display the state, so each toggle reports it, for example `Search: case on, whole word off`.

The keys are bound twice, once on `findWidgetVisible` and once on `searchViewletVisible`. These are VS Code's context-key names for a visible find bar and search view, so a `when` clause copied from a VS Code `keybindings.json` has the same meaning.

Each row is `path:line: the line's text`, and the filter matches that text. For example, typing `report` narrows four results to the one in `src/report.rs`.

The search is **synchronous and bounded**. It stops at 500 matches, skips files over 1 MiB and files that are not text, which excludes binaries, and applies `files.exclude` and the fixed skips in the same way as quick open. When a limit stops the search, the status bar reports how many matches were found *and that there may be more*.

Search does not stream results or update a persistent results view. The limit message distinguishes a complete result set from a truncated search.

## Go to line

`ctrl+g` asks for a line number.

![Jumping to line 4 with ctrl+g](img/go-to-line.svg)

It accepts `12` and `12:5`, VS Code's `line:column` format, matching the position shown in the status bar. Lines and columns are one-based, as in the status bar.

A column past the end of its line moves the cursor to the end of the line. A line outside the document is rejected with the valid range: `line 99 is outside 1-42`.

## A prompt is a text input

All of these prompts use the same widget, which is built from the same one-line input as the find bar:

- `ctrl+v` pastes into the prompt, not into the file. `ctrl+z`, `ctrl+a` and `ctrl+x` are consumed by the prompt and do not reach the document. `ctrl+a` selects the prompt's line, so the next key replaces it.
- `editorTextFocus` is false while a prompt is open, so `tab`, `ctrl+space` and other text-editing keys do not resolve. The context key is VS Code's `inQuickOpen`.
- The caret is drawn in the prompt, where typed text is inserted.

## A bound key never does nothing

Every command bound in the default keymap either runs or **reports why it does not**:

- A recognised but unimplemented command reports its title, for example `Toggle Terminal is not implemented yet`. These commands are listed in `deco-editor::commands::PENDING`.
- An unknown identifier reports that the command does not exist: `there is no command \`editor.action.nonsens\``. This usually indicates a typo in `keybindings.json` rather than a missing feature.

A test checks that every default binding is handled or reports an error. Pending commands are excluded from the palette.

The pending list includes the terminal, zen mode, zoom, open folder, settings and keyboard-shortcut editors, and the remote menu.

## Not built yet

The keyboard-shortcuts editor and the settings UI are not built. Quick open has no `@` mode. File symbols are available with `ctrl+shift+o`, but typing `@` after `ctrl+p` does not switch the file list to a symbol list, because the prompt cannot change its item source while typing.

Search in files has no persistent results view. [Regular expressions](find-and-replace.md#regular-expressions) are toggled with `alt+r` while the query prompt is open. The matches are shown in a picker, so opening a second match requires pressing `ctrl+shift+f` again. Replacing across the workspace is built (`ctrl+shift+h`, documented in [Find and replace](find-and-replace.md#replacing-across-the-workspace)), but it asks for the replacement in a second prompt instead of showing the matches and replacement together as a search view would.

The GPU frontend cannot draw prompts, so it rejects `ctrl+g`, `ctrl+shift+p` and `ctrl+p` with a message instead of opening an invisible prompt that captures the keyboard. `ctrl+shift+o` is not available there because it is gated on a language-server capability, and the GPU frontend has no language-server client.
