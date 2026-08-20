# Language servers

deco speaks the Language Server Protocol directly: `Content-Length` framing,
JSON-RPC 2.0, the full lifecycle, and position-encoding negotiation. It does not
route through an extension host, so a server works whether or not the matching
VS Code extension exists.

## Diagnostics

Diagnostics arrive by `textDocument/publishDiagnostics` and replace the previous
set for that document, which is what the protocol means: a server publishes the
complete list each time, and an empty list is how it says the problems are fixed.
The status bar tallies errors and warnings; `F8` and `shift+F8` walk them.

![The status bar tally, and F8 walking the problems](img/diagnostics.svg)

| Key | Command |
| --- | --- |
| `F8` | `editor.action.marker.next` |
| `shift+F8` | `editor.action.marker.prev` |

`F8` visits problems in **file order**, not publication order — servers emit in
whatever order analysis finished, and "next" has to mean next in the file or the
cursor jumps around unpredictably. It wraps, and it reports what it landed on.
Positions are clamped, because a diagnostic can outlive the text it describes: you
may have deleted the offending lines before the server caught up.

Information and hints are folded into neither tally. They are not problems you
have to act on, and a status bar has room for the two that are.

## Hover

![A hover box for the symbol under the caret](img/hover.svg)

`ctrl+k ctrl+i` asks for hover text and draws it below the caret, or above when
there is no room — a box hanging off the bottom of the terminal is worse than one
covering the line above. The status bar is never covered: it is where the editor
reports everything else, including why a hover might be wrong. `escape` dismisses
it, and so does moving the cursor, since a hover describing where the cursor *was*
is worse than none.

The protocol's `contents` field has four different shapes across protocol
versions and servers — a string, a `MarkedString`, an array of either, or a
`MarkupContent` — and deco reads all four, flattening to plain lines.

## Completion

![Opening the completion list, narrowing it by typing, and accepting an item](img/completion.svg)

| Key | Command |
| --- | --- |
| `ctrl+space` | `editor.action.triggerSuggest` |
| `down` / `up` | `selectNextSuggestion` / `selectPrevSuggestion` |
| `tab` / `enter` | `acceptSelectedSuggestion` |
| `escape` | `hideSuggestWidget` |

The list opens on `ctrl+space` and also on a trigger character the server asked
for — `.` or `::`. Typing narrows it in place and the same keystroke goes into the
document, so the list and the file always agree about what has been typed; a
backspace widens it again. Ranking prefers a prefix match, then a
case-insensitive prefix, then a subsequence, and the selection is the best match
for what has been typed, re-chosen as the list narrows and widens — which is what
`editor.suggestSelection: "first"`, VS Code's default, does. deco does not read
that setting, so the other values are not available.

A server's `preselect` still decides the row the list *opens* on: a server that
knows the likely answer puts it there, and it is answering the query as it stood
when the list was asked for.

The marker in the left column is the item's kind: `f` function, `v` value, `t`
type, `m` module, `k` keyword, `s` snippet, `·` anything else.

**Snippets are inserted without their placeholders.** deco advertises
`snippetSupport: false` and several servers send them anyway, so `foo(${1:arg})`
becomes `foo(arg)` and the status bar says so. Inserting the placeholder syntax
literally would be worse; there are no tab stops to jump between yet.

## References

`shift+f12` lists everything that refers to the symbol under the cursor, and
`enter` opens that file at that line. It is the same list a project-wide search
produces — the question is the same one, *which of these places do you want to be*
— so it filters as you type in the same way.

| Key | Command |
| --- | --- |
| `shift+f12` | `editor.action.goToReferences` |

Each row is `path:line: the line's text`, with the path shortened against the
workspace root; a location outside the workspace keeps its whole path, because
there the directory is the informative part. The declaration is included, because
"find all references" that omits the definition is a surprising answer and VS Code
includes it too.

The line's text comes from the **open document** when the location is in it, and
from disk otherwise — the two differ exactly when there are unsaved changes, and
the list has to describe what you are looking at. Locations in a scheme deco
cannot open (`jdt:`, `untitled:`) are left out rather than listed as rows that do
nothing.

## Go to symbol

`ctrl+shift+o` asks for the names a document declares and offers them as the same
list references and project search use — `enter` goes to one. The picker itself is
documented with the other prompts, under
[Go to symbol](commands.md#go-to-symbol); what belongs here is the protocol.

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

Both are read, and both flatten to one list in document order — a parent before
its children — because that is the order a reader of the file expects. A nested
symbol keeps its whole path, so a method three levels down reads as
`outer.middle.leaf`.

A symbol is positioned on `selectionRange`, which is the identifier, and not on
`range`, which covers the whole definition and would land the cursor on a doc
comment. `SymbolInformation`'s `location.uri` is deliberately **ignored**: the
request named one document, so a server answering about another is out of spec, and
trusting it would let a symbol list navigate somewhere unrelated.

A symbol with no name or no position is dropped — there would be nothing to pick
or nowhere to go — but an unrecognised `SymbolKind` is not: that is a newer
specification than this client, and the name is the useful part.

Accepting a symbol goes through the same open-a-file-at-a-position path a search
result does. For the document already on screen that is a tab switch onto itself,
so unsaved changes survive; and because each row carries the path that was asked
about, an answer that arrives after you switched tabs still navigates the right
file.

## Semantic tokens

A server's semantic tokens say what a name *is* — a type told from a variable by
its declaration, a shadowed binding, a macro apart from a function — which is
precisely what [the lexer](highlighting.md) cannot know. deco asks for the whole
document's tokens when the file opens and again after each edit, and colours what
comes back from the theme's `semanticTokenColors`.

![The same file coloured by the lexer, then by the server, then with the setting off](img/semantic-tokens.svg)

In those frames `LIMIT` is teal to the lexer, which can only see that it is
capitalised, and blue to the server, which knows it is a read-only binding.

The lexer is not replaced. A token type the theme has no rule for falls back to
the lexer's colour, and so does every character no token covers — punctuation,
whitespace, comments most servers do not classify. The result is a document that
is fully coloured whether or not a server is running, and more precisely coloured
when one is.

| Setting | Effect |
| --- | --- |
| `editor.semanticHighlighting.enabled: true` | Always draw them |
| `editor.semanticHighlighting.enabled: false` | Never draw them |
| absent, or `"configuredByTheme"` | The theme's own `semanticHighlighting` flag decides |

Deferring to the theme is VS Code's default and the right one: a theme written
without semantic rules looks *worse* with them applied, because the few types it
does resolve overrule a lexer that was colouring everything consistently.

The wire format is a flat list of integers, five per token, each token's position
stated relative to the one before it — and `deltaStart` is relative to the
previous token's column only when the two share a line, otherwise it is an
absolute column. Tokens naming a type outside the legend the server announced at
initialisation are dropped, but still advance the position, since a following
token's coordinates are relative to the one deco could not name.

A classification describes the text it was computed from, so an edit discards it
rather than keeping it: a token list applied to shifted text colours the wrong
words, which is worse than the lexer alone for the moment the answer takes.
Keypresses that only move the cursor send nothing and keep the tokens, and a
request is not made while one is already outstanding.

The feature needs `full` document support and a non-empty legend. A server
offering only ranges or delta updates is treated as not offering the feature at
all, rather than half-colouring the file.

## Formatting

`ctrl+shift+i` formats the document and `ctrl+k ctrl+f` the selection. Both keys
are gated on `editorHasDocumentFormattingProvider`, so they do not resolve at all
when the server cannot format — the key reports nothing rather than reporting a
failure.

The options sent are yours: `editor.tabSize`, `editor.insertSpaces`,
`files.trimTrailingWhitespace` and `files.insertFinalNewline`, resolved through the
same settings layering as everything else, including any `[language]` override.

A batch of edits is applied as one transaction, back to front against the
pre-edit document, so it is one undo step and no edit lands in a position that a
previous edit moved. Overlapping edits are refused rather than guessed at: the
specification forbids them, so a server sending them is broken, and picking which
to honour would corrupt the file silently.

## Configuring a server

`deco.lsp.servers` is keyed by a server identifier, and each definition says
which languages it serves. It is deco's own namespace rather than one of VS
Code's, because in VS Code a server arrives inside an extension and there is no
equivalent setting.

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

A definition you supply wins over the built-in one for the same language, even
though the two have different identifiers — otherwise configuring a server would
silently lose to the default.

The command and its arguments are passed as a list and never as a shell string,
so nothing in them is interpreted by a shell.

**A server defined by workspace settings is refused by name.** A cloned
repository can otherwise run a program of its choosing the moment you open a file
in it. The editor says which server it declined and why, and falls back to the
next candidate for that language — so a repository cannot disable the feature by
defining a server you then decline.

The refusal is recorded in the problem list, which `deco --print-config` prints
and `F8` walks, whether or not another server started for that language. It also
reaches the status bar when nothing started, because with no server running there
is nothing else the row could be saying.

There is **no "I trust this repository" to say once.** VS Code has Workspace Trust;
deco would have to remember the answer somewhere, and it
[does not write configuration files](configuration.md#colour-themes) by design. So
the way to run a repository's own server is to copy the definition into your user
settings, having read it — which is the step Workspace Trust makes it easy to skip.

## Not built yet

Rename and code actions are parsed but not wired. Applying a rename means editing
files that are not open — possible now that there are tabs, but it needs a
`WorkspaceEdit` applied across several documents as one undoable action, which
does not exist yet. Those keys say the feature is not implemented rather than
pretending.

Changes are sent as full-document syncs; the incremental path exists in
`deco-lsp` but the editor does not yet track applied ranges. Semantic tokens are
whole-document for the same reason: `textDocument/semanticTokens/full/delta` needs
the previous result held and patched, and a full request after each edit is
correct without it.

Go-to-definition across files opens a new tab, or switches to the tab already
holding the file — see [Tabs](tabs.md). When a server returns several results
they are offered as the same list references uses, rather than guessed between.
