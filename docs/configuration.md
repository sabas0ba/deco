# Configuration

deco reads VS Code's own file formats, with VS Code's own key names. That is the
whole point: an existing `settings.json` and `keybindings.json` should mean the
same thing here, and it is a constraint on every new feature rather than a shim
bolted on at the edges.

## Where files are read from

deco keeps everything under one root, and falls back to VS Code's location so an
existing setup works without being copied.

| Platform | deco | VS Code (read-only fallback) |
| --- | --- | --- |
| Linux / BSD | `$XDG_CONFIG_HOME/deco`, else `~/.config/deco` | `~/.config/Code/User` |
| macOS | `~/Library/Application Support/deco` | `~/Library/Application Support/Code/User` |
| Windows | `%APPDATA%\deco` | `%APPDATA%\Code\User` |

Under that root: `settings.json`, `keybindings.json`, `extensions/`, `snippets/`.
VS Code splits these — user JSON under `Code/User` but extensions under
`~/.vscode/extensions` on every platform — and deco applies that quirk when
reading VS Code's, while keeping its own together.

**Nothing is ever written back to VS Code's directory.** The fallback is one-way.

## settings.json

JSONC: comments and trailing commas are accepted, because VS Code's own default
settings file contains both and refusing to parse it would make the compatibility
claim hollow.

Layers apply in VS Code's order, each overriding the one before:

```
Default  <  User  <  Remote  <  Workspace  <  Folder
```

Language-specific overrides work as they do in VS Code, and are resolved against
the open document's language:

```jsonc
{
  "editor.tabSize": 4,
  "editor.insertSpaces": true,
  "files.insertFinalNewline": true,

  "[rust]": {
    "editor.tabSize": 4
  },
  "[makefile]": {
    "editor.insertSpaces": false
  }
}
```

Settings deco resolves into an open document's behaviour: `editor.tabSize`,
`editor.insertSpaces`, `editor.detectIndentation`, `editor.wordSeparators`,
`editor.wordWrap`, `editor.wordWrapColumn`, `editor.lineNumbers`,
`editor.renderWhitespace`, `editor.cursorStyle`,
`editor.cursorSurroundingLines`, `editor.scrollBeyondLastLine`,
`editor.rulers`, `editor.fontFamily`, `editor.fontSize`, `editor.lineHeight`,
`workbench.colorTheme`, `files.eol`, `files.trimTrailingWhitespace`,
`files.insertFinalNewline`, plus `extensions.*` for the host and deco's own
`deco.lsp.*` (see [Language servers](language-servers.md)).

Three of those are resolved and **not yet read by anything**:
`editor.renderWhitespace`, `editor.rulers` and `editor.cursorStyle`. Named here
rather than left to be discovered, because a setting that parses and does nothing is
worse than one that is rejected. `editor.fontFamily`, `editor.fontSize` and
`editor.lineHeight` are the GPU frontend's alone — a terminal has no font size — and
the GPU frontend does not wrap, so `editor.wordWrap` is the terminal's alone in the
other direction. The
[top-level README](https://github.com/sabas0ba/deco#readme) tracks what is unbuilt.

Unknown keys are kept rather than rejected. A settings file written for VS Code
contains a great many of them, and failing on the first one would make the file
unusable.

### The file outranks your indentation setting

`editor.detectIndentation` is **on** by default, in VS Code and here. It is why
opening somebody else's two-space project and pressing `tab` does not reindent it
to four: your `editor.tabSize` is a preference for files that have no answer of
their own, and a file that has one outranks it.

![tab in a two-space file, then in a four-space one](img/detect-indentation.svg)

The status bar says what one `tab` inserts, because nothing in the text does, and
it is what decides whether a diff is one line or forty. `(detected)` is added only
when the file's indentation **differed** from your settings and won — a two-space
file read as two-space where `editor.tabSize` already said two overrode nothing,
and a permanent note about that would be noise on most files.

Two halves, guessed separately:

- **Tabs or spaces** is a vote: how many indented lines begin with a tab against
  how many begin with a space. An even split says nothing and the setting stands,
  because a mixed file is usually one halfway through being converted and guessing
  would finish the conversion in whichever direction the coin landed.
- **How wide** comes from the *differences* between consecutive lines' indents, not
  from the indents themselves. A file indented by four has lines starting at 0, 4,
  8 and 12 columns, and every one of those is a multiple of two — counting
  multiples would call it a two-space file. The differences are all four. Ties go
  to the smaller width, in VS Code's own order.

A tab-indented file settles *tabs or spaces* and says nothing about how wide to
draw a tab, so `editor.tabSize` still decides that. Only the first
10,000 lines are examined, which is VS Code's limit too: a file's indentation is
evident long before that, and a generated file of half a million lines is not worth
scanning to be told the same thing.

Set `"editor.detectIndentation": false` to have your settings win outright. It can
arrive in workspace settings after the file is open and still takes effect — the
file's answer is remembered rather than re-read, so nothing is copied to apply it.

## keybindings.json

The same format, including chords, `when` clauses, per-platform `mac` keys, and
`-command` removals:

```jsonc
[
  { "key": "ctrl+alt+n", "command": "editor.action.insertLineAfter",
    "when": "editorTextFocus && !editorReadonly" },

  // Take a default away.
  { "key": "ctrl+k ctrl+d", "command": "-editor.action.moveSelectionToNextFindMatch" }
]
```

Later rules win, as they do in VS Code, so a user binding overrides a default with
the same key and `when` clause. Context keys are VS Code's, verbatim —
`editorTextFocus`, `textInputFocus`, `editorHasSelection`,
`editorHasMultipleSelections`, `suggestWidgetVisible`, `findWidgetVisible`,
`findInputFocussed`, `editorHasDiagnostics`, `editorHasDefinitionProvider`,
`editorHasDocumentFormattingProvider`, `isMac`, `isWindows` — so a `when` clause
copied out of an existing file means the same thing.

A broken `keybindings.json` does not stop the editor from opening. Each entry that
fails to parse is reported through the session's problem list and skipped: an
editor that refuses to start because of a typo in a config file cannot be used to
fix that typo.

## Colour themes

`workbench.colorTheme` names a theme. Two are built in — `Default Dark Modern` and
`Default Light Modern` — and a theme extension from the marketplace works as-is,
because a theme is declarative and starts no host process.

`ctrl+k ctrl+t` switches between them.

![Switching from the dark theme to the light one](img/color-theme.svg)

The right-hand column is `dark`, `light` or `high contrast`, from the
contribution's `uiTheme`. It is the part of the choice a label often does not say,
and it is what tells you whether the screen is about to go white.

The list is the two built-in themes — first, because they are the ones that always
work — followed by every `contributes.themes` entry of every extension under deco's
extensions directory **and VS Code's**, so a theme you installed for VS Code is
offered here without being copied. One label is offered once; the same extension
installed under two versions is the usual reason for a duplicate. Nothing is read
while listing: a picker over forty themes would otherwise parse forty files and
thirty-nine of them for nothing.

**The choice lasts the session.** Making it stick means putting
`workbench.colorTheme` in your settings, which the status bar says when the theme
changes.

**deco does not write settings files, and that is the decision rather than a gap.**
It reads them and never writes them, so your `settings.json` is a file you own: what
is in it is what you put there, comments and formatting included, and nothing
appears in it because of a key you pressed.

The cost is real and worth stating. VS Code writes `workbench.colorTheme` when you
pick a theme and is not thought rude for it, so this is a divergence you may not
want: a theme chosen with `ctrl+k ctrl+t` has to be written down by hand to
survive. The status bar says which line to add, and that is the whole of the
mechanism.

The same answer settles a question it is easy to reach from the other direction:
there is no per-workspace "I trust this repository" for
[a workspace-defined language server](language-servers.md#configuring-a-server),
because remembering that answer would mean writing it somewhere.

A theme that cannot be read reports why and leaves the current one alone, because
the alternative is an editor with no colours.

What is read from a theme file: `colors`, `tokenColors` (including TextMate scope
matching), `semanticTokenColors`, and `include` chains for themes that build on
another. Naming a theme deco cannot find falls back to the dark theme and says so
rather than starting with no colours.

`.tmTheme` (plist) themes are **not** supported, and neither are `-` scope
exclusions in a scope selector.

The terminal frontend composites translucent colours — selections, find
highlights — against the editor background, because a terminal cell has no alpha.

## Nothing here fails closed

Every configuration path degrades rather than refusing: an unknown theme falls
back, a broken keybinding is skipped, an unparseable workspace settings file is
reported and ignored, an unknown setting is kept. The editor collects what went
wrong into a list the frontend can show. This is deliberate — configuration is
exactly the thing you need a working editor to repair.
