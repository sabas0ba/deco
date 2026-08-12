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

## A new line starts where the old one started

`editor.autoIndent` carries the indentation across a newline, and opens a block when
the caret is between a pair of brackets.

![Typing a brace, pressing enter, and typing inside the block](img/auto-indent.svg)

| `editor.autoIndent` | On `enter` |
| --- | --- |
| `none` | Column zero |
| `keep` | The previous line's indentation |
| `brackets` (default) | And one level deeper after an opening bracket |

`advanced` and `full` resolve to `brackets`. Both mean this plus the
`indentationRules` a language configuration contributes, and deco has none to read —
so it says which of the five it is doing rather than accepting a value whose name
promises more.

**`{|}` and `enter` opens a block**: the closer moves to its own line at the outer
indent and the caret is left on an indented line between them, which is the shape
everybody types next. It pairs with
[auto-closing brackets](#auto-closing-brackets) — typing `{` produces `{}`, and
`enter` opens it.

Pressing enter *inside* a line's indentation carries only what the caret had reached,
not the whole indent: two spaces into an eight-space indent gives a new line indented
two. And each cursor gets its own indent, since each is on its own line.

`ctrl+enter` (`editor.action.insertLineAfter`) has always copied the indentation,
because it is a command that knows it is making a line. `enter` is bound to `type`
with a newline in it — a plain insertion — so it went to column zero, and the same
editor indented on one key and not the other.

### An indent you press past is taken back

`editor.trimAutoWhitespace` (default true) removes an auto-inserted indent from a line
you abandon, so one press of enter too many does not leave four spaces behind for a
diff to find.

![Pressing enter twice, and the abandoned line coming back empty](img/trim-auto-whitespace.svg)

Only an indent **deco inserted** is trimmable. Whitespace you typed is yours, and an
indent stops being trimmable the moment you type anything else on that line — at which
point it is that line's indentation rather than a leftover.

Two things make it safe:

- The line is checked **against the buffer** before anything is deleted. Only a line
  that still holds exactly the whitespace that was put there and nothing else is
  trimmed, so the record of what was inserted is a record rather than an authority: a
  stale entry does nothing instead of costing you text.
- The trim goes into **the same transaction** as the edit that abandoned the line, so
  one `ctrl+z` takes back one action. Its own undo step would mean pressing `ctrl+z`
  twice for one press of enter.

It happens on the next **edit**, not on the next cursor movement. VS Code trims when
the caret leaves the line; deco waits until something is typed, which is when there is
a transaction to fold the trim into. Between the two the whitespace is invisible, and
the file on disk is the same either way — unless you save in between, which VS Code
would also do.

## A file cannot talk to your terminal

deco draws into a terminal, and a terminal *interprets* what it is written. A document
containing `\x1b[31m` would recolour everything after it; `\x07` rings the bell; and
`\x1b]52;c;…\x07` is OSC 52, which **writes the clipboard** on every terminal that
supports it — iTerm2, kitty, foot, recent xterm, Windows Terminal, tmux with
`set-clipboard on`.

So no control character is ever written as itself. Each is replaced by its Unicode
Control Pictures glyph — `␛` for escape, `␇` for bell, `␡` for delete — one column
each, so the substitution moves nothing that was laid out around it.

| `editor.renderControlCharacters` | What is drawn |
| --- | --- |
| `true` (default) | The picture, in `editorWhitespace.foreground` |
| `false` | A blank of the same width |

The setting chooses between the glyph and a blank. It cannot choose to send the byte:
that is not a rendering option, it is a way of handing your terminal to whoever wrote
the file.

The substitution happens twice over, deliberately. The renderer does it for a
document's own text, where the setting applies; and the painter does it again,
unconditionally, to every span it writes. Text reaches the screen from places that are
not the open document — a **file name** appears in the tab bar, and a **search result**
carries a line of somebody else's file into a prompt row — so the last line of defence
is at the write, where it cannot be forgotten.

### What this does not cover

**Bidirectional overrides.** `U+202E` and its relatives reorder the characters around
them, so a line can display as something other than what it says — the Trojan Source
class of attack, which matters most in code that will be compiled. Those characters are
printable rather than control, so nothing here touches them, and VS Code handles them
under a different setting (`editor.unicodeHighlight.*`) that deco does not read. Named
here rather than left implied.

## Auto-closing brackets

`editor.autoClosingBrackets` closes a bracket or a quote as you open it, and steps
over a closer you have already got.

![Typing a bracket, a quote, and typing the closers back over them](img/auto-closing-brackets.svg)

| `editor.autoClosingBrackets` | Where a bracket closes itself |
| --- | --- |
| `never` | Nowhere |
| `languageDefined` (default) | Before whitespace, the end of a line, or one of `;:.,=}])>` |
| `beforeWhitespace` | Only before whitespace or the end of a line |
| `always` | Wherever the caret is |

Each value is a rule about *where*, not about whether. Closing in the middle of a
word turns `word` into `wo(r)rd`, which is why VS Code's default — and deco's — is
conditional.

The pairs are the language's, which is what `languageDefined` means. Two entries in
that table are worth stating:

- **Rust's `'` is a lifetime.** `&'a str` is ordinary code and `&''a str` is what
  closing it would write, so Rust has no apostrophe pair. rust-analyzer's own language
  configuration leaves it out for the same reason.
- **Markdown, HTML and XML have no apostrophe pair either.** An apostrophe in prose is
  far more common there than a quoted string, and `don''t` is worse than nothing.

TypeScript and JavaScript add a backtick, since a template literal is a quote there.
Everything else gets `()`, `[]`, `{}`, `""` and `''`.

A quote both opens and closes, so **stepping over is tried first**: in front of a `"`
the useful answer is to move past it rather than to open another pair.

One keystroke is one undo step — `ctrl+z` after `(` takes back both halves, because
one keystroke wrote them. And with several cursors, either all of them close or none
do: a keystroke that inserted a pair in some places and a bare bracket in others is
not an edit anybody can undo by looking at it.

### What it deliberately does not do

- **Surround a selection.** Typing `(` with text selected replaces it, as it always
  has. Wrapping instead is `editor.autoSurround`, a separate setting deco does not
  read — and closing a bracket *around* a replacement while leaving the replacement
  out would be neither behaviour. `ctrl+shift+a` does surround, for comments.
- **Remember which closers it inserted.** Typing `)` in front of any `)` steps over
  it. VS Code tracks the ones it added and steps over only those; the state that needs
  is a per-document list invalidated by every other edit, and the two answers differ
  only where somebody typed both halves by hand and then typed a third closer.
- **Delete both halves on backspace.** That is `editor.autoClosingDelete`, also
  unread.

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

## Word wrap

`editor.wordWrap` breaks long lines to fit the window instead of running them off
the right edge, and `alt+z` turns it on for the file you are looking at.

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

`"bounded"` is the one worth knowing about: it keeps prose to a readable measure
on a wide screen without letting a narrow window wrap the same text twice.

The break goes **after whitespace**, at the last opportunity that fits, with two
qualifications that exist because the obvious rule reads badly:

- A space that would overflow does not force a break; it hangs past the right
  edge, where it is invisible. Breaking before it would start the next row with a
  space, which reads as indentation the file does not have.
- Whitespace before a row's first word is not an opportunity. An indented line
  would otherwise break immediately after its indent, spending a row on a lone tab
  and starting the text at column zero — losing the one cue that says how deep the
  line is.

A run with no whitespace in it breaks at the width. For code that is a URL or a
base64 blob, where every break is arbitrary; for Chinese, Japanese and Korean it
is the *right* answer, since they put no spaces between words. Proper line
breaking — Unicode UAX #14, which knows that a closing bracket may not begin a
row — needs a table deco does not carry, and it would be the first dependency
added for cosmetics.

### The arrow keys move by row

This is the half of word wrap that is easy to get wrong. With wrapping on, `down`
moves one row and not one document line, because a row is what the key looks like
it moves by; moving by line would pass over however many rows the current line
occupies, which in prose is most of a paragraph. `home` and `end` are the ends of
the *row* — except on a line's first row, where `home` keeps its usual trick of
stopping at the first non-whitespace and then at column zero, and on a line's
last row, where `end` is the end of the line.

The sticky column a vertical motion keeps is measured **within the row** for the
same reason. Measured from the line's start it would be a number with no meaning
on screen, and every press of `down` through a wrapped paragraph would land
somewhere unrelated to where the caret looked like it was.

`end` and `home` clear that sticky column, so a `down` after them measures afresh
from where they landed rather than returning to whatever was last aimed at.

### What it costs

Nothing that grows with the file. The scroll position is anchored to a document
line plus an offset into it, rather than to a count of rows from the top of the
file: counting rows from the top means wrapping the whole file to find out where
the window is, on every keystroke. Anchored this way, drawing and scrolling both
cost the height of the window — and so does finding the furthest the window may
scroll, which walks backwards from the last line rather than forwards from the
first.

### The continuation row keeps the line's indent

`editor.wrappingIndent` decides how far a continuation row is pushed in, and it
defaults to `same` — VS Code's default too, and the reason a wrapped block of code
still reads as one block.

![The same wrapped line under same, none and deepIndent](img/wrapping-indent.svg)

| `editor.wrappingIndent` | Where a continuation row starts |
| --- | --- |
| `none` | Column zero |
| `same` (default) | As deep as the line's own indentation |
| `indent` | One `editor.tabSize` deeper |
| `deepIndent` | Two deeper |

At `none` the second row of a nested line starts beside the unindented lines around
it, which is how a wrap comes to be misread as code. The deeper settings make a
wrapped row impossible to mistake for a statement of its own.

The indent is **dropped** — not trimmed — once it would take more than half the
width. Past that a wrapped line is more indent than text, and a deeply nested one
would be wrapped into a column a few characters wide. A partial indent would line
the continuation up with nothing, so the whole of it goes.

It is not only cosmetic, which is why it reaches into the wrap itself: a row pushed
in by four columns has four fewer to fill, and its tab stops land differently. The
caret follows — a vertical motion keeps the column **of the screen**, so `down`
across two rows pushed in by different amounts still goes straight down. A goal
column that falls inside the indent lands on the row's first character, there being
nothing further left on that row.

### What is not there

- **A wrap marker.** VS Code draws nothing either, but some editors mark the
  break, and the gutter's blank continuation row is the only signal here.
- **The GPU frontend does not wrap.** It has no chrome to draw at all yet, so it
  lays out one line per row — see the [README](https://github.com/sabas0ba/deco#readme).

The setting is not written anywhere when you press `alt+z`. deco
[does not write configuration files](configuration.md#colour-themes), and a
keystroke that silently edited one would be a poor way to find that out. The
toggle is per document — so per tab — and turning it on to read one Markdown file
leaves the code in the next tab alone. Pressing it twice restores whatever
`editor.wordWrap` asked for, including a `[language]` override of it, rather than
assuming `"on"`.

## Positions are UTF-16 code units

Every position in deco is a line and a UTF-16 code-unit offset, which is what the
Language Server Protocol and `vscode.Position` use. That is why a caret moves
past an emoji in one press and a backspace removes the whole thing: the editor
counts the same units a language server does, so no conversion sits between them
to be wrong.

Text is held in a rope, so an edit near the start of a large file costs the same
as one near the end, and every edit is invertible — the undo history stores the
inverse rather than a copy of the document.
