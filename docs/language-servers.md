# Language servers

deco speaks the Language Server Protocol directly: `Content-Length` framing,
JSON-RPC 2.0, the full lifecycle, and position-encoding negotiation. It does not
route through an extension host, so a server works whether or not the matching
VS Code extension exists.

## Diagnostics

Diagnostics arrive by `textDocument/publishDiagnostics` and replace the previous set for that document. The protocol requires a server to publish the complete list each time, and an empty list means the problems are fixed. The status bar counts errors and warnings; `F8` and `shift+F8` move between them.

![The status bar tally, and F8 walking the problems](img/diagnostics.svg)

| Key | Command |
| --- | --- |
| `F8` | `editor.action.marker.next` |
| `shift+F8` | `editor.action.marker.prev` |

`F8` visits problems in **file order**, not publication order. Servers publish in the order analysis finishes, and following that order would move the cursor unpredictably. Navigation wraps around and reports the problem it moved to. Positions are clamped, because a diagnostic can refer to text that has changed since, for example lines deleted before the server published an update.

Information and hint diagnostics are not included in either count. They do not require action, and the status bar only has room for errors and warnings.

## Hover

![A hover box for the symbol under the caret](img/hover.svg)

`ctrl+k ctrl+i` requests hover text and draws it below the caret, or above the caret if there is not enough room below. The hover never covers the status bar, which the editor uses for all other messages, including why a hover might be wrong. `escape` dismisses it, and so does moving the cursor, because the hover describes the previous cursor position.

The protocol's `contents` field has four shapes across protocol versions and servers: a string, a `MarkedString`, an array of either, or a `MarkupContent`. deco reads all four and flattens them to plain lines.

## Completion

![Opening the completion list, narrowing it by typing, and accepting an item](img/completion.svg)

| Key | Command |
| --- | --- |
| `ctrl+space` | `editor.action.triggerSuggest` |
| `down` / `up` | `selectNextSuggestion` / `selectPrevSuggestion` |
| `tab` / `enter` | `acceptSelectedSuggestion` |
| `escape` | `hideSuggestWidget` |

The list opens on `ctrl+space` and on a trigger character requested by the server, such as `.` or `::`. Typing narrows the list and also inserts the character into the document, so the list always matches the typed text; backspace widens it again. Ranking prefers a prefix match, then a case-insensitive prefix match, then a subsequence match. The selected item is the best match for the typed text and is re-chosen as the list narrows or widens. This matches VS Code's default, `editor.suggestSelection: "first"`. deco does not read that setting, so the other values are not available.

A server's `preselect` still determines the row selected when the list *opens*. It applies to the query as it was when the list was requested.

The marker in the left column is the item's kind: `f` function, `v` value, `t`
type, `m` module, `k` keyword, `s` snippet, `·` anything else.

### Snippet tab stops

Completions containing unique numeric fields (`$1`, `${1}`, `${1:arg}`) select
the first field after insertion. Type to replace its default, press `tab` to
advance in numeric order, or `shift+tab` to return. `$0` is the final cursor;
without it, the snippet ends at the end of the inserted text. `escape` ends
navigation and keeps the text. User keybindings may override the standard
`jumpToNextSnippetPlaceholder`, `jumpToPrevSnippetPlaceholder` and `leaveSnippet`
commands, using the `inSnippetMode` context key.

![Editing and navigating completion fields](img/snippet-tabstops.svg)

Ranges use UTF-16 positions and follow edits within the selected field, including
multiline text and paste. Leaving that field, undo/redo, or an edit that cannot
be tracked safely ends navigation. Insertion is one ordinary undo step; edits
to fields use the editor's existing undo grouping. State belongs to its document
and does not apply to another tab. Suggestion lists, find inputs and prompts
retain their own key handling.

Completion snippets also expand the following [VS Code snippet variables](https://code.visualstudio.com/docs/editing/userdefinedsnippets#_variables) using the active document when accepted:

| Variable | Value |
| --- | --- |
| `TM_FILENAME` | File name, including its extension; empty for an untitled document |
| `TM_FILENAME_BASE` | File name with its final extension removed |
| `TM_LINE_INDEX`, `TM_LINE_NUMBER` | Cursor line, counting from zero or one respectively |
| `TM_CURRENT_LINE` | Line contents, excluding its line ending |
| `TM_CURRENT_WORD` | Word at the cursor, using deco's word-selection rules |
| `TM_SELECTED_TEXT` | Primary selection's text, or empty if nothing is selected |

Use `$TM_FILENAME`, `${TM_FILENAME}` or a literal default such as `${TM_FILENAME:untitled}`. Empty values use the default when supplied. For example, `$TM_FILENAME(${1:arg})$0` in `main.rs` inserts `main.rs(arg)` and selects `arg`. Variables may repeat and do not create editable fields. Resolved text is not parsed again, so a selection containing `$1` is inserted literally. No filesystem, environment or clipboard access is performed.

This is a subset of [LSP snippet syntax](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#snippet_syntax).
Repeated indices (linked editing), nested fields or variable defaults, choices, other variables, transforms,
non-LF line separators and coincident empty fields are not supported. They use
the existing text-only fallback, with a status message; no partially parsed
navigation state is installed. User snippet files and `insertSnippet` are not
implemented. `snippetSupport: false` remains advertised until the full syntax
can be honoured; this support applies to servers that already send snippets.
Pasting a non-LF line separator also ends navigation while preserving the edit.

## References

`shift+f12` lists all references to the symbol under the cursor, and `enter` opens the selected file at that line. It uses the same list as project-wide search, so it filters as you type in the same way.

| Key | Command |
| --- | --- |
| `shift+f12` | `editor.action.goToReferences` |

Each row is `path:line: the line's text`, with the path shortened relative to the workspace root. A location outside the workspace keeps its full path, because the directory is then the useful part. The declaration is included, as in VS Code.

The line's text is taken from the **open document** when the location is in it, and from disk otherwise. The two differ when there are unsaved changes, and the list shows the text as it appears in the editor. Locations in schemes deco cannot open (`jdt:`, `untitled:`) are omitted instead of being listed as rows that do nothing.

## Go to symbol

`ctrl+shift+o` requests the symbols a document declares and shows them in the same list used by references and project search; `enter` goes to the selected one. The picker is documented with the other prompts under [Go to symbol](commands.md#go-to-symbol). This section describes the protocol.

| Key | Command |
| --- | --- |
| `ctrl+shift+o` | `workbench.action.gotoSymbol` |

`textDocument/documentSymbol` has **two** result shapes, and which one arrives
depends on the server:

```jsonc
// DocumentSymbol[]: a tree, with the nesting the file has
[{ "name": "Counter", "kind": 23, "range": {…}, "selectionRange": {…},
   "children": [{ "name": "bump", "kind": 6, … }] }]

// SymbolInformation[]: flat, each with a whole location and a container name
[{ "name": "bump", "kind": 6, "containerName": "Counter",
   "location": { "uri": "file:///…", "range": {…} } }]
```

Both are read and flattened into one list in document order, with a parent before its children, which is the order of the file. A nested symbol shows its full path, so a method three levels down is shown as `outer.middle.leaf`.

A symbol is positioned on `selectionRange`, which covers the identifier, not on `range`, which covers the whole definition and would place the cursor on a doc comment. `SymbolInformation`'s `location.uri` is deliberately **ignored**. The request named one document, so a result for another document is outside the specification, and following it could navigate somewhere unrelated.

A symbol with no name or no position is dropped, because it cannot be shown or navigated to. A symbol with an unrecognised `SymbolKind` is kept, because the kind comes from a newer specification version than this client supports and the name is still useful.

Accepting a symbol uses the same open-at-position path as a search result. For the document already on screen, this switches to its own tab, so unsaved changes are kept. Each row stores the path of the requested document, so a response that arrives after you switched tabs still navigates in the correct file.

## Semantic tokens

A server's semantic tokens classify names by meaning: for example, a type distinguished from a variable by its declaration, a shadowed binding, or a macro distinguished from a function. [The lexer](highlighting.md) cannot determine this. deco requests tokens for the whole document when the file opens and after each edit, and colours them with the theme's `semanticTokenColors`.

![The same file coloured by the lexer, then by the server, then with the setting off](img/semantic-tokens.svg)

In these frames, `LIMIT` is teal when coloured by the lexer, which only sees that it is capitalised, and blue when coloured by the server, which identifies it as a read-only binding.

Semantic tokens do not replace the lexer. A token type with no theme rule uses the lexer's colour, as does every character that no token covers, such as punctuation, whitespace and the comments most servers do not classify. The document is therefore fully coloured without a server, and more precisely coloured with one.

| Setting | Effect |
| --- | --- |
| `editor.semanticHighlighting.enabled: true` | Always draw them |
| `editor.semanticHighlighting.enabled: false` | Never draw them |
| absent, or `"configuredByTheme"` | The theme's own `semanticHighlighting` flag decides |

Deferring to the theme is VS Code's default. A theme written without semantic rules looks *worse* with semantic highlighting applied, because the few token types it defines override a lexer that was colouring everything consistently.

The wire format is a flat list of integers, five per token. Each token's position is relative to the previous token: `deltaStart` is relative to the previous token's column when both are on the same line, and is an absolute column otherwise. Tokens whose type is outside the legend the server announced at initialisation are dropped, but they still advance the position, because the next token's coordinates are relative to them.

Tokens describe the text they were computed from, so an edit discards them. Applied to shifted text, they would colour the wrong words, which is worse than lexer-only colouring while the next response is pending. Keypresses that only move the cursor send no request and keep the tokens, and no request is sent while another is outstanding.

The feature requires `full` document support and a non-empty legend. A server that offers only range or delta requests is treated as not supporting the feature, instead of colouring only part of the file.

## Formatting

`ctrl+shift+i` formats the document and `ctrl+k ctrl+f` formats the selection. Both keys require `editorHasDocumentFormattingProvider`, so they do not resolve when the server cannot format, and no failure is reported.

The request uses your settings: `editor.tabSize`, `editor.insertSpaces`, `files.trimTrailingWhitespace` and `files.insertFinalNewline`, resolved through the same settings layers as other settings, including any `[language]` override.

A batch of edits is applied as one transaction, from the end of the document towards the start, against the document as it was before the edits. The batch is one undo step, and no edit is applied at a position shifted by an earlier edit. Overlapping edits are rejected. The specification forbids them, and choosing which ones to apply could corrupt the file without notice.

## Code actions

![ctrl+. listing what the server offers and applying the one chosen](img/code-actions.svg)

`ctrl+.` requests the available actions for the selection, or for the caret if nothing is selected, so a quick fix works without first selecting the error. The actions are shown as a list. The chosen action's edit is applied through the same [`WorkspaceEdit`](#rename) path as a rename: completely or not at all, and undone with one `ctrl+z`.

The key requires `editorHasCodeActionsProvider`, so it does not resolve when the server offers no code actions.

**Code-action requests include the original diagnostic JSON.** Servers may use the opaque `data` field to construct a fix. deco's parsed diagnostic struct retains only fields needed for display, so reconstructing the request from that struct would lose information required by the server.

Diagnostics that **overlap** the selection are sent, not only those contained in it. A selection covering part of an error still refers to that error, and a caret touching a diagnostic also counts.

**An action without an edit is resolved before it is applied.** Servers that compute expensive refactorings send the titles first and compute the edit only for the chosen action. `codeAction/resolve` requests that edit. The action is sent back unchanged, including `data`, because the server uses it to identify the action. If the server has no `resolveProvider`, an action without an edit cannot be applied, and deco declines it with a message naming it.

The list shows actions that deco cannot perform instead of hiding them:

| What arrived | What happens |
| --- | --- |
| An action with an edit | Applied |
| An action with no edit, from a server that resolves | Resolved, then applied |
| An action with no edit, from one that does not | Declined, naming the action |
| A disabled action | Listed with the server's reason, in the second column; choosing it repeats the reason |
| A bare `Command` | Declined, naming the command — see below |
| An edit that creates, renames or deletes a file | Declined for **that action only**, naming the operation |

Because of the last row, the edit is parsed when an action is chosen, not when the menu is built. One unsupported entry does not remove the other, valid entries from the menu.

**A bare `Command` is not run.** Such actions require `workspace/executeCommand` and may cause the server to send a `workspace/applyEdit` request. deco does not implement this command-execution path and reports the unsupported command by name.

## Rename

![F2 renaming a symbol across two files, one of them not open, undone with ctrl+z](img/rename.svg)

`F2` renames the symbol under the caret in every location the server reports. The key requires `editorHasRenameProvider`, so it does not resolve with a server that cannot rename. The prompt opens with the current name selected: typing replaces it, and `end` keeps it so you can add a suffix. Accepting the unchanged name is refused without sending a request, because an edit per occurrence that changes nothing would mark every file that mentions the symbol dirty.

Rename was the first feature in deco to use a **`WorkspaceEdit`**: one change across any number of documents. A partially applied rename leaves a project that does not build, which is worse than no rename. The whole edit is therefore validated before any part of it is applied:

- **Every URI must resolve to a file.** A server-specific scheme or an `untitled:` document causes the edit to be rejected; it is not skipped.
- **Every version stated by the server must still be current.** Positions are valid only for the text they were computed from, so a response that arrives after a keystroke has moved lines is rejected with a message. A server using the older `changes` form states no versions, so there is nothing to check and the edit is applied without a version check.
- **Every document's edits must be applicable.** Overlapping ranges are detected at this step, using the same batched, back-to-front transaction as formatting.

Only then is anything written, and from that point nothing can fail.

**Files that are not open are opened, not written.** A rename usually changes mostly files that are not open. VS Code writes those to disk; deco opens them as tabs with unsaved changes, and the status line reports how many. There are two reasons. deco's core performs no I/O, which allows all editing behaviour to be tested without a terminal or a filesystem. deco also does not rewrite files you have not seen unless you save them. `ctrl+k s` saves them; `ctrl+z` reverts the changes.

**One `ctrl+z` undoes the whole rename** from any of the affected files, because every document records its part under one shared undo step. If you type in one of those files first, that input is undone separately. The shared step remains below it and is undone after the later edits.

A server that requests a file to be **created, renamed or deleted** as part of the change is refused with a message naming the operation. rust-analyzer does this when you rename a module. These operations arrive together with text edits that depend on them, so applying only the text edits would leave the project in a worse state than not renaming.

## Configuring a server

`deco.lsp.servers` is keyed by server identifier, and each definition lists the languages it serves. It uses deco's own namespace because VS Code has no equivalent setting; in VS Code, a server is provided by an extension.

```jsonc
{
  "deco.lsp.enabled": true,
  "deco.lsp.servers": {
    "rust-analyzer": {
      "languages": ["rust"],
      "command": "rust-analyzer",
      "args": [],
      "initializationOptions": { "cargo": { "features": "all" } }
    },
    "pylsp": {
      "languages": ["python"],
      "command": "pylsp",
      "args": ["--check-parent-process"]
    }
  }
}
```

A definition you supply takes precedence over the built-in definition for the same language, even when the identifiers differ. Otherwise the default would override a configured server without notice.

The command and its arguments are passed as a list and never as a shell string,
so nothing in them is interpreted by a shell.

**A server defined by workspace settings is refused, with a message naming it.** Otherwise a cloned repository could run a program of its choosing as soon as you open a file in it. The editor reports which server it refused and why, and falls back to the next candidate for that language, so a repository cannot disable the feature by defining a server that you then refuse.

**The same applies to a server defined by a remote.** A remote session's `machine-settings.json` is stored where anyone with an account on that machine can write it, and connecting to a machine does not mean trusting every file on it. A definition from that file is therefore refused and reported in the same way as a workspace definition. This prevents a remote machine from choosing programs that run when you connect. See [Settings that belong to the machine](remote.md#settings-that-belong-to-the-machine).

The refusal is recorded in the problem list, which `deco --print-config` prints and `F8` navigates, whether or not another server started for that language. It is also shown in the status bar when no server started, because the status bar has nothing else to report for language servers in that case.

There is **no option to trust a repository once.** VS Code has Workspace Trust, but deco would need to store that decision, and it [does not write configuration files](configuration.md#colour-themes) by design. To run a repository's own server, review the definition and copy it into your user settings. Workspace Trust makes that review easy to skip.

## Not built yet

`workspace/executeCommand` is not sent, so a code action that is only a [bare `Command`](#code-actions) is declined. Supporting it requires handling the `workspace/applyEdit` request the server may send back, in which the server asks the editor to change files instead of answering an editor request. This is a decision about what a server may change, not only a missing function call. deco currently answers such requests with `applied: false`.

`textDocument/codeLens`, `textDocument/inlayHint` and `textDocument/documentHighlight` are not requested. Each needs a display element the editor does not have yet: a line above the code, non-text content within a line, and a second kind of selection highlight.

`textDocument/prepareRename` is not sent. This request asks a server whether a position can be renamed *before* the user is prompted for a new name. deco prompts first and reports the server's refusal if there is one, so the user sees an unnecessary prompt when the position cannot be renamed.

Only the document on screen is synchronised with the server. A rename that opens
other files leaves those tabs unsaved and unknown to the server, so its answers
about them are based on what is still on disk until you save.

Changes are sent as full-document syncs. The incremental path exists in `deco-lsp`, but the editor does not yet track applied ranges. Semantic tokens are requested for the whole document for the same reason: `textDocument/semanticTokens/full/delta` requires keeping and patching the previous result, while a full request after each edit is correct without it.

Go-to-definition across files opens a new tab, or switches to the tab that already holds the file; see [Tabs](tabs.md). When a server returns several results, they are shown in the same list as references instead of deco choosing one.
