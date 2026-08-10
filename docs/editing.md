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
| `ctrl+]` / `ctrl+[` | `editor.action.indentLines` / `…outdentLines` |

Commenting is idempotent in both directions: `ctrl+k ctrl+c` on an
already-commented line leaves it alone rather than commenting it twice, and a
partly-commented selection becomes fully commented rather than inverting line by
line. Blank lines are skipped, and the token goes after the indentation rather
than at column zero.

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

Expanding a bare caret leaves any other cursors where they are. They were placed
deliberately, and discarding them to answer a question about the word under one
of them would lose more than it gains.

## Positions are UTF-16 code units

Every position in deco is a line and a UTF-16 code-unit offset, which is what the
Language Server Protocol and `vscode.Position` use. That is why a caret moves
past an emoji in one press and a backspace removes the whole thing: the editor
counts the same units a language server does, so no conversion sits between them
to be wrong.

Text is held in a rope, so an edit near the start of a large file costs the same
as one near the end, and every edit is invertible — the undo history stores the
inverse rather than a copy of the document.
