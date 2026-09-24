# Editing

Commands use VS Code's identifiers, for example `editor.action.commentLine` rather than `deco.comment`. A binding in your `keybindings.json` therefore refers to the same command that deco runs by default.

## Lines and comments

`alt+up` / `alt+down` move the line or the selected block. `ctrl+/` toggles the line comment using the open language's token. `ctrl+shift+alt+down` copies the line downwards. Undo groups edits by time and kind, so one `ctrl+z` removes the whole comment rather than one character of it.

![Moving a line, commenting it, undoing, and copying a line](img/editing.svg)

| Key | Command |
| --- | --- |
| `alt+up` / `alt+down` | `editor.action.moveLinesUpAction` / `…DownAction` |
| `ctrl+shift+alt+up` / `…+down` | `editor.action.copyLinesUpAction` / `…DownAction` |
| `ctrl+shift+k` | `editor.action.deleteLines` |
| `ctrl+enter` / `ctrl+shift+enter` | `editor.action.insertLineAfter` / `…Before` |
| `ctrl+/` | `editor.action.commentLine` |
| `ctrl+k ctrl+c` / `ctrl+k ctrl+u` | `editor.action.addCommentLine` / `…remove…` |
| `ctrl+shift+a` | `editor.action.blockComment` |
| `ctrl+]` / `ctrl+[` | `editor.action.indentLines` / `…outdentLines` |

Commenting is idempotent in both directions. `ctrl+k ctrl+c` leaves an already-commented line unchanged rather than commenting it twice, and a partly commented selection becomes fully commented rather than being toggled line by line. Blank lines are skipped, and the token is inserted after the indentation rather than at column zero.

### Block comments

`ctrl+shift+a` encloses the selection in block-comment delimiters. Line-comment commands instead add a comment token to each selected line.

![Wrapping two lines in a block comment, unwrapping, and opening an empty one](img/block-comment.svg)

**Pressing it again removes the delimiters it added.** After wrapping, the inner text stays selected. The command recognises a commented selection in two forms: with the delimiters inside the selection, as when you select a commented region, or immediately outside it, as after a wrap. Without both forms, the second press would add another comment.

With nothing selected, it inserts an empty comment and places the caret between the spaces. All cursors are wrapped in one undo step.

| Language | Delimiters |
| --- | --- |
| Rust, TypeScript, JavaScript, Go, C, C++, Java, CSS, JSONC, SQL | `/*` `*/` |
| HTML, XML, Markdown | `<!--` `-->` |
| Lua | `--[[` `]]` |
| Python | `"""` `"""` |

HTML, XML and Markdown are listed even though [the lexer](highlighting.md) does not colour them, because wrapping a selection needs only the delimiters, not a grammar.

Some languages are intentionally not listed. **Shell, YAML, TOML, Makefile, Dockerfile and JSON** have no block comment, and VS Code defines none for them; the key reports that the language has none. **Ruby** is excluded although VS Code offers `=begin` / `=end`. Each of these must be alone at the start of a line, so wrapping a selection in the middle of a line would produce text that Ruby cannot parse.

**Python's `"""` is a string, not a comment.** VS Code inserts it, and it prevents the enclosed code from running. It is an expression statement, however, so it is valid only where a statement is allowed. deco follows VS Code here.

## A new line starts where the old one started

`editor.autoIndent` copies the indentation to the new line, and opens a block when the caret is between a pair of brackets.

![Typing a brace, pressing enter, and typing inside the block](img/auto-indent.svg)

| `editor.autoIndent` | On `enter` |
| --- | --- |
| `none` | Column zero |
| `keep` | The previous line's indentation |
| `brackets` (default) | And one level deeper after an opening bracket |

`advanced` and `full` resolve to `brackets`. In VS Code, both also apply the `indentationRules` from a language configuration. deco has no such rules to read, so it reports which of the five modes it uses.

**`{|}` and `enter` opens a block**: the closing bracket moves to its own line at the outer indent, and the caret is placed on an indented line between them. This works with [auto-closing brackets](#auto-closing-brackets): typing `{` produces `{}`, and `enter` opens it.

Pressing enter *inside* a line's indentation copies only the indentation before the caret. For example, pressing enter two spaces into an eight-space indent gives a new line indented by two. Each cursor gets the indent of its own line.

`ctrl+enter` (`editor.action.insertLineAfter`) has always copied the indentation. `enter` is bound to `type` with a newline, which is a plain insertion, so it previously started the new line at column zero.

### An indent you press past is taken back

`editor.trimAutoWhitespace` (default true) removes an auto-inserted indent from a line you leave empty, so an extra enter does not leave trailing whitespace in the file.

![Pressing enter twice, and the abandoned line coming back empty](img/trim-auto-whitespace.svg)

Only an indent **inserted by deco** can be trimmed. Whitespace you typed is not trimmed, and an inserted indent is no longer trimmed once you type anything else on that line.

Two checks keep this safe:

- The line is checked **against the buffer** before anything is deleted. A line is trimmed only if it still contains exactly the inserted whitespace and nothing else, so a stale record does not delete text.
- The trim is part of **the same transaction** as the edit that left the line, so one `ctrl+z` undoes one action.

The trim happens on the next **edit**, not on the next cursor movement. VS Code trims when the caret leaves the line; deco waits until the next edit so the trim can be added to that edit's transaction. The whitespace is not visible in between, and the saved file is the same in both cases unless you save before the next edit.

## A file cannot talk to your terminal

A terminal *interprets* the bytes written to it. A document containing `\x1b[31m` would recolour everything after it, `\x07` rings the bell, and `\x1b]52;c;…\x07` is OSC 52, which **writes the clipboard** on terminals that support it: iTerm2, kitty, foot, recent xterm, Windows Terminal, and tmux with `set-clipboard on`.

deco therefore never writes a control character as itself. Each is replaced by its Unicode Control Pictures glyph, such as `␛` for escape, `␇` for bell and `␡` for delete. Each glyph is one column wide, so the substitution does not change the layout.

| `editor.renderControlCharacters` | What is drawn |
| --- | --- |
| `true` (default) | The picture, in `editorWhitespace.foreground` |
| `false` | A blank of the same width |

The setting chooses between the glyph and a blank. It cannot send the raw byte, because that would let the file's content control the terminal.

The substitution is applied at every terminal write, not only where the document text is drawn, because text from other sources also reaches the terminal:

- a **file name** appears in the tab bar and the status bar;
- a **search result** shows a line from another file in a prompt row;
- a **configuration problem** quotes a settings file value, such as a theme name or a broken keybinding. The binary prints these *before the alternate screen opens*, directly to the shell's terminal. A cloned repository's `.vscode/settings.json` is untrusted text, the same concern that applies to a workspace-defined [language server](language-servers.md#configuring-a-server);
- `deco --print-config` prints the resolved theme, language and font family, which all come from a settings file.

The renderer substitutes characters in the document according to the setting. The painter and the command-line output substitute them unconditionally. Applying the substitution at the write also covers output sources added later.

### What this does not cover

**Bidirectional overrides.** `U+202E` and related characters reorder the surrounding characters, so a line can display differently from its actual content. This is the Trojan Source class of attack, which matters most in code that will be compiled. These characters are printable rather than control characters, so this substitution does not change them. VS Code handles them with a separate setting (`editor.unicodeHighlight.*`) that deco does not read.

## Auto-closing brackets

`editor.autoClosingBrackets` inserts the closing bracket or quote when you type the opening one, and types over an existing closer.

![Typing a bracket, a quote, and typing the closers back over them](img/auto-closing-brackets.svg)

| `editor.autoClosingBrackets` | Where a bracket closes itself |
| --- | --- |
| `never` | Nowhere |
| `languageDefined` (default) | Before whitespace, the end of a line, or one of `;:.,=}])>` |
| `beforeWhitespace` | Only before whitespace or the end of a line |
| `always` | Wherever the caret is |

Each value defines *where* a bracket is closed. Closing in the middle of a word would turn `word` into `wo(r)rd`, so the default in VS Code and deco is conditional.

The pairs are defined per language, which is what `languageDefined` means. Two cases are notable:

- **Rust's `'` is a lifetime.** Closing it would turn `&'a str` into `&''a str`, so Rust has no apostrophe pair. rust-analyzer's language configuration omits it for the same reason.
- **Markdown, HTML and XML have no apostrophe pair either.** Apostrophes in prose are more common there than quoted strings, and auto-closing would produce `don''t`.

TypeScript and JavaScript add a backtick for template literals. All other languages use `()`, `[]`, `{}`, `""` and `''`.

A quote both opens and closes, so **typing over an existing quote is tried first**. Typing `"` before a `"` moves past it instead of opening another pair.

One keystroke is one undo step, so `ctrl+z` after `(` removes both brackets. With several cursors, either all cursors insert a pair or none do.

### What it deliberately does not do

- **Surround a selection.** Typing `(` with text selected replaces the selection. Wrapping is controlled by `editor.autoSurround`, a separate setting that deco does not read. `ctrl+shift+a` surrounds a selection with comment delimiters.
- **Remember which closers it inserted.** Typing `)` before any `)` types over it. VS Code types over only closers it inserted, which requires a per-document list invalidated by other edits. The behaviours differ only when both brackets were typed manually and a third closer is typed.
- **Delete both halves on backspace.** That is `editor.autoClosingDelete`, which deco also does not read.

## Multiple cursors

`ctrl+d` has two behaviours, as in VS Code. The first press selects the word under the caret. Each later press adds a cursor at the next occurrence, wrapping at the end of the file and skipping occurrences that already have a cursor. When every occurrence is selected, deco reports it.

![Selecting a word, adding a cursor at the next occurrence, and typing at both](img/multi-cursor.svg)

| Key | Command | What it does |
| --- | --- | --- |
| `ctrl+d` | `editor.action.addSelectionToNextFindMatch` | Select the word, then add a cursor per occurrence |
| `ctrl+shift+l` | `editor.action.selectHighlights` | A cursor on every occurrence at once |
| `ctrl+k ctrl+d` | `editor.action.moveSelectionToNextFindMatch` | Skip this occurrence instead of adding to it |
| `ctrl+alt+up` / `ctrl+alt+down` | `editor.action.insertCursorAbove` / `…Below` | A cursor on the line above or below |
| `escape` | `removeSecondaryCursors` | Back to one cursor |

With a selection already made, `ctrl+d` searches for the **selected text** rather than a word, so selecting `oo` matches inside every `foo`. Matching is case-sensitive, so `FOO` does not match. (The find bar behaves differently; see [Find and replace](find-and-replace.md).)

`ctrl+shift+l` makes the **last** occurrence primary, so the view scrolls to the last occurrence before you type.

Expanding a bare caret expands **every** caret to its own word, as in VS Code. A caret with no word under it stays a caret rather than selecting whitespace, and existing selections are not changed.

## Word wrap

`editor.wordWrap` breaks long lines to fit the window instead of letting them run past the right edge. `alt+z` toggles wrapping for the current file.

![A long line running off the edge, then wrapped, then walked with the arrow keys](img/word-wrap.svg)

| Key | Command |
| --- | --- |
| `alt+z` | `editor.action.toggleWordWrap` |

| `editor.wordWrap` | Where it breaks |
| --- | --- |
| `"off"` (default) | Nowhere; a long line is cut off at the edge |
| `"on"` | At the window's width |
| `"wordWrapColumn"` | At `editor.wordWrapColumn`, whatever the window's width |
| `"bounded"` | At whichever of those two is narrower |

`"bounded"` limits line length on a wide screen and still wraps at the window width in a narrow window.

The break is placed **after whitespace**, at the last break opportunity that fits, with two exceptions:

- A space that would overflow does not force a break. It extends past the right edge, where it is not visible. Breaking before it would start the next row with a space that looks like indentation.
- Whitespace before a row's first word is not a break opportunity. Otherwise an indented line could break immediately after its indent, leaving a row with only whitespace and starting the text at column zero, which hides the line's indentation depth.

A run with no whitespace breaks at the width. For code such as a URL or a base64 blob, any break position is arbitrary. For Chinese, Japanese and Korean, which do not put spaces between words, this is the correct behaviour. Full line breaking according to Unicode UAX #14, which for example prevents a closing bracket from starting a row, needs a table that deco does not include and would add a dependency for presentation only.

### The arrow keys move by row

With wrapping on, `down` moves one row, not one document line. Moving by line would skip all rows of the current line, which in prose can be most of a paragraph. `home` and `end` move to the ends of the *row*, with two exceptions: on a line's first row, `home` stops at the first non-whitespace character and then at column zero, and on a line's last row, `end` moves to the end of the line.

For the same reason, the sticky column used by vertical motion is measured **within the row**. A column measured from the line's start would not correspond to the caret's screen position.

`end` and `home` clear the sticky column, so a following `down` uses the new caret position.

### What it costs

The cost does not grow with the file size. The scroll position is stored as a document line plus an offset into it, rather than as a row count from the top of the file, which would require wrapping the whole file on every keystroke. Drawing and scrolling therefore cost time proportional to the window height. Finding the maximum scroll position has the same cost, because it walks backwards from the last line.

### The continuation row keeps the line's indent

`editor.wrappingIndent` sets the indentation of continuation rows. The default is `same`, as in VS Code, so a wrapped block of code keeps its visual structure.

![The same wrapped line under same, none and deepIndent](img/wrapping-indent.svg)

| `editor.wrappingIndent` | Where a continuation row starts |
| --- | --- |
| `none` | Column zero |
| `same` (default) | As deep as the line's own indentation |
| `indent` | One `editor.tabSize` deeper |
| `deepIndent` | Two deeper |

With `none`, the second row of a nested line starts at the same column as the surrounding unindented lines and can be misread as code. The deeper settings keep a wrapped row distinguishable from a separate statement.

The indent is **dropped entirely**, not reduced, once it would take more than half the width. Otherwise a deeply nested line would wrap into a column only a few characters wide. A partial indent would not align with anything, so none is used.

The indent affects the wrap itself: a row indented by four columns has four fewer columns for text, and its tab stops change. Vertical motion keeps the **screen** column, so `down` across two rows with different indents moves straight down. A goal column inside the indent moves the caret to the row's first character.

### What is not there

- **A wrap marker.** VS Code draws none either, but some editors mark the break. In deco, the blank gutter on a continuation row is the only indication.
- **The GPU frontend does not wrap.** It has no chrome yet and lays out one line per row. See the [README](https://github.com/sabas0ba/deco#readme).

Pressing `alt+z` does not write the setting anywhere, because deco [does not write configuration files](configuration.md#colour-themes). The toggle applies per document, and therefore per tab, so turning it on for one Markdown file does not affect code in another tab. Pressing it twice restores the configured `editor.wordWrap` value, including a `[language]` override, rather than assuming `"on"`.

## Positions are UTF-16 code units

Every position in deco is a line and a UTF-16 code-unit offset, as in the Language Server Protocol and `vscode.Position`. The caret moves past an emoji in one press and backspace removes the whole emoji. Because the editor uses the same units as a language server, positions need no conversion.

Text is stored in a rope, so an edit near the start of a large file costs the same as one near the end. Every edit is invertible, and the undo history stores the inverse edit rather than a copy of the document.
