# Snippets

A snippet is text with fields to fill in. deco inserts snippets from two places: a language server's completions, and snippet files you write, in VS Code's format. Both use the same syntax and the same keys.

## Filling in fields

After insertion, the first field is selected. Type to replace its default, press `tab` to advance in numeric order, or `shift+tab` to return. `$0` is the final cursor; without it, the snippet ends at the end of the inserted text. `escape` ends navigation and keeps the text. User keybindings may override the standard `jumpToNextSnippetPlaceholder`, `jumpToPrevSnippetPlaceholder` and `leaveSnippet` commands, using the `inSnippetMode` context key.

![Editing and navigating completion fields](img/snippet-tabstops.svg)

- **A repeated index is one field in several places.** `${1:name} = $1` selects both occurrences as multiple cursors, so typing changes both. The default comes from the first occurrence that has one.
- **A field can contain fields.** In `${1:call(${2:arg})}`, `tab` goes from the whole call to `arg`. Typing over the outer field replaces the inner one, and `tab` then skips it, as in VS Code.
- **A choice offers its options.** `${1|pub,pub(crate)|}` inserts `pub` and opens a list of the options when navigation reaches the field. Choosing one puts it in every occurrence as one undo step; closing the list keeps `pub`.

Ranges use UTF-16 positions and follow edits within the selected field, including multiline text and paste. Leaving that field, undo/redo, or an edit that cannot be tracked safely ends navigation. Insertion is one ordinary undo step; edits to fields use the editor's existing undo grouping. State belongs to its document and does not apply to another tab. Suggestion lists, find inputs and prompts retain their own key handling. Pasting a line separator other than `\n` also ends navigation while preserving the edit.

## Variables

Snippets expand the [VS Code snippet variables](https://code.visualstudio.com/docs/editing/userdefinedsnippets#_variables) when they are inserted:

| Variable | Value |
| --- | --- |
| `TM_FILENAME` | File name, including its extension; empty for an untitled document |
| `TM_FILENAME_BASE` | File name with its final extension removed |
| `TM_DIRECTORY`, `TM_FILEPATH` | The file's directory and full path |
| `RELATIVE_FILEPATH` | The path relative to the workspace, or the full path outside it |
| `TM_LINE_INDEX`, `TM_LINE_NUMBER` | Cursor line, counting from zero or one respectively |
| `TM_CURRENT_LINE` | Line contents, excluding its line ending |
| `TM_CURRENT_WORD` | Word at the cursor, using deco's word-selection rules |
| `TM_SELECTED_TEXT` | Primary selection's text, or empty if nothing is selected |
| `CURSOR_INDEX`, `CURSOR_NUMBER` | `0` and `1`: a snippet is inserted at the primary cursor only |
| `CLIPBOARD` | The text last cut or copied in deco. deco keeps its own clipboard and does not read the system's |
| `WORKSPACE_NAME`, `WORKSPACE_FOLDER` | The workspace directory's name and path; empty without a workspace |
| `LINE_COMMENT`, `BLOCK_COMMENT_START`, `BLOCK_COMMENT_END` | The language's comment markers, as `ctrl+/` uses them; empty for a language without them |
| `CURRENT_YEAR`, `CURRENT_YEAR_SHORT` | `2026`, `26` |
| `CURRENT_MONTH`, `CURRENT_MONTH_NAME`, `CURRENT_MONTH_NAME_SHORT` | `10`, `October`, `Oct` |
| `CURRENT_DATE`, `CURRENT_DAY_NAME`, `CURRENT_DAY_NAME_SHORT` | `04`, `Sunday`, `Sun` |
| `CURRENT_HOUR`, `CURRENT_MINUTE`, `CURRENT_SECOND` | The time on a 24-hour clock, two digits each |
| `CURRENT_SECONDS_UNIX` | Seconds since 1970-01-01T00:00:00Z |
| `CURRENT_TIMEZONE_OFFSET` | The local offset from UTC, such as `+09:00` |
| `RANDOM`, `RANDOM_HEX` | Six random decimal or hexadecimal digits |
| `UUID` | A version 4 UUID |

Use `$TM_FILENAME`, `${TM_FILENAME}` or a default such as `${TM_FILENAME:untitled}`. Empty values use the default when supplied, and the default may contain fields, as in `${TM_SELECTED_TEXT:${1:body}}`. For example, `$TM_FILENAME(${1:arg})$0` in `main.rs` inserts `main.rs(arg)` and selects `arg`. Variables may repeat and do not create editable fields. Resolved text is not parsed again, so a selection containing `$1` is inserted literally.

The date and time are the local clock's: deco asks the operating system for the local time (`localtime_r` on Unix, `GetLocalTime` on Windows). The random values come from the standard library's randomly keyed hasher; they differ between insertions but are not suitable for secrets. No file is read and no program is run to resolve a variable.

## Transforms

A transform rewrites text with a regular expression, as `/regex/format/options`. On a variable, `${TM_FILENAME_BASE/(.*)/${1:/pascalcase}/}` turns `my_file` into `MyFile` when the snippet is inserted. On a tab stop, `${1:name}: ${1/(.*)/${1:/upcase}/}` adds an occurrence that is not typed into: it shows the stop's text transformed, and is brought up to date when `tab`, `shift+tab` or `escape` leaves the stop.

The format may use `$1`, `${1}`, `${1:/upcase}`, `/downcase`, `/capitalize`, `/camelcase`, `/pascalcase`, `${1:+if}`, `${1:?if:else}`, `${1:-else}` and `${1:else}`; the options are `g`, `i`, `m`, `s` and `u`. Text the expression does not match is kept. When it matches nothing at all and the format has an `else` text, the format is used with every group empty. Regular expressions use the [`regex` crate's syntax](https://docs.rs/regex/latest/regex/#syntax), which has no look-around and no backreferences.

## Your own snippets

Snippet files use [VS Code's format](https://code.visualstudio.com/docs/editing/userdefinedsnippets#_create-your-own-snippets): JSON with comments, one member per snippet.

```jsonc
{
  "Print to console": {
    "prefix": ["log", "print"],
    "body": ["console.log('$1');", "$0"],
    "description": "Log output to the console",
    "scope": "javascript,typescript"
  }
}
```

| File | Applies to |
| --- | --- |
| `snippets/<language>.json` in [deco's configuration directory](configuration.md) | That language only; `scope` is ignored |
| `snippets/*.code-snippets` in the same directory | The languages in `scope`, or every language without it |
| `.deco/*.code-snippets` and `.vscode/*.code-snippets` in the workspace | As above, for the workspace's team |

When deco's configuration directory has no `snippets` directory, VS Code's is read instead, as for `settings.json`. A workspace's `.vscode` directory is only searched for `*.code-snippets`, because it also holds JSON files that are not snippets. A snippet that cannot be read is reported with its file and name, and the rest of the file still loads.

- **As a completion.** Each prefix is an item in the completion list, beside the language server's items. `ctrl+space` lists them without a server too.
- **By name.** `Insert Snippet` (`editor.action.insertSnippet`) in the command palette lists the snippets for the current language and inserts the chosen one in place of the selection, so `$TM_SELECTED_TEXT` can wrap it. A keybinding can name one with `{ "name": "Print to console" }`, or give the text itself with `{ "snippet": "console.log($1)" }`.

A body without snippet syntax is inserted as it is. A body that uses syntax deco cannot expand, such as a variable not listed above, is reported and not inserted.

## Limits

This is the full [LSP snippet syntax](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#snippet_syntax). A completion from a language server that uses an unknown variable, or a regular expression the `regex` crate rejects, is inserted as text with its placeholders removed, with a status message. A remote session reads your own snippet files from this machine, but not the remote workspace's.
