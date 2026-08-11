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
case-insensitive prefix, then a subsequence, and the selection follows the same
item as the list narrows rather than jumping to whatever is now at the top.

The marker in the left column is the item's kind: `f` function, `v` value, `t`
type, `m` module, `k` keyword, `s` snippet, `·` anything else.

**Snippets are inserted without their placeholders.** deco advertises
`snippetSupport: false` and several servers send them anyway, so `foo(${1:arg})`
becomes `foo(arg)` and the status bar says so. Inserting the placeholder syntax
literally would be worse; there are no tab stops to jump between yet.

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

## Not built yet

Rename, the references list and code actions are parsed but not wired: nothing
renders a reference list, and applying a rename means editing files that are not
open, which deco cannot do while it holds one document. Those keys say the feature
is not implemented rather than pretending. Changes are sent as full-document
syncs; the incremental path exists in `deco-lsp` but the editor does not yet track
applied ranges.

Go-to-definition across files opens a new tab, or switches to the tab already
holding the file — see [Tabs](tabs.md). When a server returns several results it
takes the first and says how many there were, because nothing renders a list of
locations yet.
