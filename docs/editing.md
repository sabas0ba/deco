# Editing

Commands are addressed by VS Code's identifiers — `editor.action.commentLine`,
not `deco.comment` — so rebinding one in your own `keybindings.json` reaches the
same command deco runs by default.

## Lines and comments

`alt+up` / `alt+down` move the line or the selected block; `ctrl+/` toggles the
line comment using the open language's token; `ctrl+shift+alt+down` copies the
line downwards. Undo groups by time and by kind, so one `ctrl+z` takes back the
comment rather than one character of it.

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

Commenting is idempotent in both directions: `ctrl+k ctrl+c` on an
already-commented line leaves it alone rather than commenting it twice, and a
partly-commented selection becomes fully commented rather than inverting line by
line. Blank lines are skipped, and the token goes after the indentation rather
than at column zero.

### Block comments

`ctrl+shift+a` wraps the selection in one comment rather than commenting each line,
which is the difference worth having both for.

![Wrapping two lines in a block comment, unwrapping, and opening an empty one](img/block-comment.svg)

It is **its own inverse**: pressing it again removes what it just added. That needs
the wrap to leave the inner text selected, and it recognises a commented selection in
either shape — the delimiters inside the selection, which is what you get by
selecting a commented region, or immediately outside it, which is what a wrap leaves
behind. Recognising only one would make the second press comment the comment.

With nothing selected it opens an empty comment and puts the caret between the
spaces, because the point of pressing it there is to write the comment next. Every
cursor is wrapped, in one undo step.

| Language | Delimiters |
| --- | --- |
| Rust, TypeScript, JavaScript, Go, C, C++, Java, CSS, JSONC, SQL | `/*` `*/` |
| HTML, XML, Markdown | `<!--` `-->` |
| Lua | `--[[` `]]` |
| Python | `"""` `"""` |

HTML, XML and Markdown are here even though [the lexer](highlighting.md) does not
colour them: wrapping a selection needs the delimiters, not a grammar.

Two deliberate absences. **Shell, YAML, TOML, Makefile, Dockerfile and JSON** have no
block comment, and neither does VS Code claim one for them — the key reports the
language has none. **Ruby** is left out although VS Code offers `=begin` / `=end`:
those must each sit alone at the start of a line, so wrapping a selection in the
middle of one produces text Ruby will not parse, and a command that corrupts the file
is worse than a command that declines.

**Python's `"""` is a string, not a comment.** It is what VS Code inserts and what a
Python programmer means by commenting a block out, and it does stop the code running
— but as an expression statement, so it is only sound where a statement is allowed.
Matching VS Code beats inventing a third answer.

## Multiple cursors

`ctrl+d` is two behaviours behind one key, as it is in VS Code. The first press
turns the caret into a selection of the word under it. Every press after that
adds a cursor at the next occurrence, wrapping at the end of the file and
skipping occurrences a cursor already sits on — so holding it walks the file
rather than stalling. Once every occurrence is selected it says so.

![Selecting a word, adding a cursor at the next occurrence, and typing at both](img/multi-cursor.svg)

| Key | Command | What it does |
| --- | --- | --- |
| `ctrl+d` | `editor.action.addSelectionToNextFindMatch` | Select the word, then add a cursor per occurrence |
| `ctrl+shift+l` | `editor.action.selectHighlights` | A cursor on every occurrence at once |
| `ctrl+k ctrl+d` | `editor.action.moveSelectionToNextFindMatch` | Skip this occurrence instead of adding to it |
| `ctrl+alt+up` / `ctrl+alt+down` | `editor.action.insertCursorAbove` / `…Below` | A cursor on the line above or below |
| `escape` | `removeSecondaryCursors` | Back to one cursor |

With a selection already made, `ctrl+d` searches for the **selected text** rather
than a word, so selecting `oo` matches inside every `foo` — which a word-based
search would miss. Matching is exact: you selected precisely that text, so `FOO`
is a different string. (The find bar is the opposite way round — see
[Find and replace](find-and-replace.md).)

`ctrl+shift+l` makes the **last** occurrence primary, so the view scrolls to the
end of the file and you can see how far the change reaches before you type.

Expanding a bare caret expands **every** caret to its own word, as VS Code does.
The cursors were placed deliberately and expanding each of them keeps that
placement. A caret with no word under it stays a caret rather than selecting the
whitespace it sits in, and a selection you already made is left as you made it.

## Positions are UTF-16 code units

Every position in deco is a line and a UTF-16 code-unit offset, which is what the
Language Server Protocol and `vscode.Position` use. That is why a caret moves
past an emoji in one press and a backspace removes the whole thing: the editor
counts the same units a language server does, so no conversion sits between them
to be wrong.

Text is held in a rope, so an edit near the start of a large file costs the same
as one near the end, and every edit is invertible — the undo history stores the
inverse rather than a copy of the document.
