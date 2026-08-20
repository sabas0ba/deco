# Find and replace

Search is **literal**, and within the open file. There is no regular-expression
mode and no find-in-files; both are recognised and say so when pressed rather
than reporting an unknown command.

## Finding

`ctrl+f` opens the bar, seeded from the selection if there is one. Typing narrows
the query, `enter` and `F3` step forward, `shift+enter` and `shift+F3` step back,
and both wrap. `alt+c` and `alt+w` toggle case sensitivity and whole-word
matching; a capital letter in `[aa ww]` means the option is on.

![Opening the find bar, stepping through matches, and toggling whole-word](img/find.svg)

| Key | Command |
| --- | --- |
| `ctrl+f` | `actions.find` |
| `F3` / `enter` | `editor.action.nextMatchFindAction` |
| `shift+F3` / `shift+enter` | `editor.action.previousMatchFindAction` |
| `alt+c` | `toggleFindCaseSensitive` |
| `alt+w` | `toggleFindWholeWord` |
| `escape` | `closeFindWidget` |

Every match is highlighted with `editor.findMatchHighlightBackground` and the
current one with `editor.findMatchBackground` — VS Code's own theme keys, and its
own distinction between the match you are on and the rest. The readout on the
right is `3 of 7`; if you move the cursor off a match it becomes `7 results`,
because claiming you are still on the third would be a lie.

`F3` works with the bar closed. With no query yet it searches for the selection,
or for the word under the cursor, and reports where it landed in the status bar —
which is the only place a count can go once the bar is gone.

**Case sensitivity defaults to off**, which is what VS Code's find widget does.
That is the opposite of `ctrl+d`, where you selected exactly the text you meant.

## Replacing

`ctrl+h` opens the same bar with a second row. `tab` and `shift+tab` move between
the two inputs, `enter` on the replacement replaces the current match and steps to
the next, and `ctrl+alt+enter` replaces every match **in one undo step**.

The keyboard starts on whichever field is left to fill in: on the **replacement**
when the query arrived seeded from a selection or was typed earlier, and on the
**query** when there is nothing to replace yet.

![Opening the replace row, filling both fields, and replacing every match](img/replace.svg)

| Key | Command |
| --- | --- |
| `ctrl+h` | `editor.action.startFindReplaceAction` |
| `enter` (on the replacement) | `editor.action.replaceOne` |
| `ctrl+alt+enter` | `editor.action.replaceAll` |
| `tab` / `shift+tab` | `deco.find.toggleField` |

Two deliberate refusals:

- **A replace pressed while the cursor is not on a match steps onto one instead
  of changing anything.** VS Code does the same, and it is the safe reading of an
  ambiguous keypress: replacing text you cannot see is worse than making you
  press the key twice.
- **A match that already reads as the replacement is left out.** Replacing `foo`
  with `foo` neither dirties the file nor adds an undo step. This is reachable
  rather than theoretical — a case-insensitive search for `foo` also finds `FOO`,
  so replacing `foo` with `foo` across `foo FOO` reports one replacement, not two.

An empty replacement deletes the matches, which is a legitimate thing to want.

## The find input is a text input

While the find bar has the keyboard, `editorTextFocus` is false and
`textInputFocus` is true — VS Code's distinction, because its find box is a text
input inside the editor. deco copies it deliberately, with two consequences worth
knowing:

- `ctrl+v` pastes into the query, not into the file. `ctrl+z` cannot rewrite the
  document from behind an open bar; it is swallowed, along with `ctrl+a` and
  `ctrl+x`, which would otherwise select or cut in a document you are not looking
  at.
- `tab`, `ctrl+space` and `ctrl+k ctrl+i` stop resolving at all, because they are
  bound to `editorTextFocus`. No special-casing — the context key means what it
  means in VS Code.

The query has a caret but no selection, so `ctrl+a`, `ctrl+c` and `ctrl+x` act on
the whole query. A query longer than the bar scrolls to keep the caret visible,
and on a terminal too narrow for everything the count is dropped first, the
toggles second, and the query last: a search term you cannot see is one you
cannot correct.

## Whole-word matching differs from VS Code, on purpose

VS Code compiles the needle into `\bneedle\b` and so inherits `\b`'s definition:
a *transition* between a word and a non-word character. With whole-word on,
searching for `(` therefore matches `f(x)` but not ` ( ` — behaviour that falls
out of the regex engine rather than out of anything anyone asked for.

deco constrains only the ends of the needle that are themselves word characters.
For every needle that begins and ends in a word character — every needle anyone
types with the option on — the two agree.
