# Find and replace

Search is literal by default; `alt+r` switches to [regular expressions](#regular-expressions). The find bar searches the active document. For workspace search, use [Find in Files](commands.md#search-in-files); workspace replacement is described [below](#replacing-across-the-workspace).

## Finding

`ctrl+f` opens the bar with the selection, if there is one, as the query. The query is selected, so typing replaces it instead of appending. Typing narrows the query, `enter` and `F3` move forward, `shift+enter` and `shift+F3` move back, and both directions wrap. `alt+c`, `alt+w` and `alt+r` toggle case sensitivity, whole-word matching and regular expressions; a capital letter in `[aa ww rr]` means the option is on.

![Opening the find bar, stepping through matches, and toggling whole-word](img/find.svg)

| Key | Command |
| --- | --- |
| `ctrl+f` | `actions.find` |
| `F3` / `enter` | `editor.action.nextMatchFindAction` |
| `shift+F3` / `shift+enter` | `editor.action.previousMatchFindAction` |
| `alt+c` | `toggleFindCaseSensitive` |
| `alt+w` | `toggleFindWholeWord` |
| `alt+r` | `toggleFindRegex` |
| `escape` | `closeFindWidget` |

All matches are highlighted with `editor.findMatchHighlightBackground`, and the current match with `editor.findMatchBackground`. These are VS Code's theme keys, with VS Code's distinction between the current match and the others. The readout on the right shows `3 of 7`. If the selection no longer corresponds to a match, it shows only the total, such as `7 results`.

`F3` also works when the bar is closed. Without a query, it searches for the selection or for the word under the cursor, and reports the match position in the status bar, because the bar is not available to show a count.

**Case sensitivity is off by default**, as in VS Code's find widget. `ctrl+d` behaves the opposite way, because there you selected the exact text.

## Replacing

`ctrl+h` opens the same bar with a second row. `tab` and `shift+tab` move between
the two inputs, `enter` on the replacement replaces the current match and steps to
the next, and `ctrl+alt+enter` replaces every match **in one undo step**.

Keyboard focus starts in the field that still needs input: the **replacement** when the query was seeded from a selection or typed earlier, and the **query** when there is no query yet.

![Opening the replace row, filling both fields, and replacing every match](img/replace.svg)

| Key | Command |
| --- | --- |
| `ctrl+h` | `editor.action.startFindReplaceAction` |
| `enter` (on the replacement) | `editor.action.replaceOne` |
| `ctrl+alt+enter` | `editor.action.replaceAll` |
| `tab` / `shift+tab` | `deco.find.toggleField` |

Two cases deliberately replace nothing:

- **Replace with the cursor not on a match moves to a match without changing anything.** VS Code does the same. The keypress is ambiguous, and replacing text you cannot see is worse than requiring a second keypress.
- **A match that already equals the replacement is skipped.** Replacing `foo` with `foo` neither marks the file dirty nor adds an undo step. This case occurs in practice: a case-insensitive search for `foo` also finds `FOO`, so replacing `foo` with `foo` in `foo FOO` reports one replacement, not two.

An empty replacement deletes the matches.

## Regular expressions

`alt+r` switches the query to a regular expression, in the find bar and in the `ctrl+shift+f` / `ctrl+shift+h` prompt. The query is kept as typed when the mode changes. A query seeded from a selection is escaped in regex mode, so it still matches the selected text.

![Toggling regex mode and replacing with capture groups](img/regex.svg)

The syntax is that of the Rust [`regex`](https://docs.rs/regex/1/regex/#syntax) crate, which is close to the JavaScript syntax VS Code uses. Matching takes time linear in the length of the text, so no pattern can make a search hang. The differences from VS Code:

- Look-around (`(?=…)`, `(?<!…)`) and backreferences (`\1`) are not supported. They are reported as invalid patterns.
- `^` and `$` match at the start and end of every line. `.` does not match a line break; a pattern containing `\n` matches across lines.
- Matches of zero length, such as `^` on its own, are skipped.

An invalid pattern shows `Invalid regex` in the bar instead of a count. `enter`, `F3` or a replace command reports the parser's message, which names the problem and its position, in the status bar. A project search with an invalid pattern reports the error without reading any file.

In the replacement, the following references are expanded for each match:

| Reference | Inserts |
| --- | --- |
| `$1` … `$99` | The capture group; an empty string if the group did not take part in the match |
| `$0`, `$&` | The whole match |
| `$$` | `$` |
| `\n`, `\t`, `\\` | A line break, a tab, a backslash |

A reference to a group the pattern does not have, such as `$3` with two groups, is inserted literally. `$12` refers to group 12 if the pattern has one, and otherwise to group 1 followed by `2`, as in JavaScript. Case-changing references (`\u`, `\U`, `\l`, `\L`) are not supported. In literal mode the replacement is inserted as typed.

## Replacing across the workspace

![ctrl+shift+h replacing a term in two files, one of them not open, undone with ctrl+z](img/replace-in-files.svg)

`ctrl+shift+h` replaces a term throughout the workspace. It prompts twice, first for the search term and then for the replacement, because deco shows one prompt at a time, whereas VS Code has a search view with two input boxes. The first prompt is the same one `ctrl+shift+f` opens, seeded from the selection or the word under the cursor.

| Key | Command |
| --- | --- |
| `ctrl+shift+h` | `workbench.action.replaceInFiles` |
| `ctrl+shift+f` | `workbench.action.findInFiles` |

**The whole replacement is one undoable action.** It is applied through the same [`WorkspaceEdit`](language-servers.md#rename) path as a rename: every file is checked before any file is written, and one `ctrl+z` in any affected file undoes all of it.

**Files that are not open are opened, not written.** Nothing is written to disk until you save. The status line reports how many tabs were opened, and `ctrl+k s` saves them. This also makes the operation reviewable, because the opened tabs contain the changes.

**Matches are found again before they are replaced.** The search identifies which *files* contain matches. The positions of the occurrences are then recomputed in the editor, using the buffer of any file that is open in a tab. This matters for a file edited since the search read it from disk: positions from disk would refer to outdated text, and replacing at them would change the wrong text and then save it over the real file. As a result, the reported count is the number of replacements made, not the number of matches found earlier.

An empty replacement removes every occurrence; this is allowed.

**A search that reached its limit reports it.** Project search is bounded (see [Running commands](commands.md#search-in-files)). The report states that there may be more matches, so a replace-all that did not cover every match is not mistaken for a complete one.

The match options are those of project search, not the find bar; case sensitivity or regex mode set in one does not change the other. In regex mode, each file's replacements expand capture groups against that file's own matches.

In a [remote session](remote.md), the search, reads and edits all go through the connection, so replacements change the files on the remote environment, not files at the same paths on the local machine.

## The find input is a text input

While the find bar has keyboard focus, `editorTextFocus` is false and `textInputFocus` is true. This follows VS Code, where the find box is a text input inside the editor. deco adopts this deliberately, with two consequences:

- `ctrl+v` pastes into the query, not into the file. `ctrl+z` cannot change the document while the bar is open; it is ignored, as are `ctrl+a` and `ctrl+x`, which would otherwise select or cut in the document behind the bar.
- `tab`, `ctrl+space` and `ctrl+k ctrl+i` do not resolve, because they are bound with `editorTextFocus`. No special handling is needed; the context key has the same meaning as in VS Code.

The query has a caret and at most one selection, which covers the whole query. The query is in that state when `ctrl+f` seeds it from a selection and after `ctrl+a`, so the first typed character replaces the seeded text instead of appending to it, and `ctrl+a` followed by backspace clears the field. `ctrl+c` and `ctrl+x` always act on the whole query. A query longer than the bar scrolls to keep the caret visible. On a terminal too narrow for everything, the count is hidden first, then the toggles, and the query last, because a search term you cannot see cannot be corrected.

## Whole-word matching differs from VS Code, on purpose

VS Code compiles the needle into `\bneedle\b` and therefore uses `\b`'s definition: a *transition* between a word character and a non-word character. With whole-word matching on, searching for `(` therefore matches `f(x)` but not ` ( `. This is a side effect of the regex engine, not intended behaviour.

deco constrains only the ends of the needle that are word characters. For every needle that begins and ends with a word character, which covers typical whole-word searches, the two behave the same. In regex mode the rule applies to the ends of each match: `foo\d` with whole word on finds `foo1` in `(foo1)` but not in `xfoo1`.
