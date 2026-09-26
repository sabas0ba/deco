# Configuration

deco reads VS Code's configuration formats and uses the same setting and command identifiers for supported features. Unsupported settings and frontend differences are listed below.

## Where files are read from

deco keeps its configuration under one root and falls back to VS Code's location, so an existing setup works without copying it.

| Platform | deco | VS Code (read-only fallback) |
| --- | --- | --- |
| Linux / BSD | `$XDG_CONFIG_HOME/deco`, else `~/.config/deco` | `~/.config/Code/User` |
| macOS | `~/Library/Application Support/deco` | `~/Library/Application Support/Code/User` |
| Windows | `%APPDATA%\deco` | `%APPDATA%\Code\User` |

The root contains `settings.json`, `keybindings.json`, `extensions/` and `snippets/`. VS Code stores user JSON under `Code/User` but extensions under `~/.vscode/extensions` on every platform. deco uses those separate locations when reading VS Code's files, and keeps its own files under one root.

**Nothing is ever written back to VS Code's directory.** The fallback is one-way.

## settings.json

Settings use JSONC: comments and trailing commas are accepted.

Layers apply in VS Code's order, each overriding the one before:

```
Default  <  User  <  Remote  <  Workspace  <  Folder
```

**Remote** is used only in a remote session. It is read from `machine-settings.json` on the remote machine and contains machine-specific settings. It is fetched over the connection because the local machine cannot read it directly, and it is **not trusted**. See
[Settings that belong to the machine](remote.md#settings-that-belong-to-the-machine).

Language-specific overrides work as in VS Code and are resolved for the open document's language:

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
`editor.wordWrap`, `editor.wordWrapColumn`, `editor.wrappingIndent`,
`editor.autoClosingBrackets`, `editor.autoIndent`, `editor.trimAutoWhitespace`,
`files.autoSave`, `files.autoSaveDelay`, `editor.renderControlCharacters`,
`editor.lineNumbers`,
`editor.renderWhitespace`, `editor.cursorStyle`,
`editor.cursorSurroundingLines`, `editor.scrollBeyondLastLine`,
`editor.rulers`, `editor.fontFamily`, `editor.fontSize`, `editor.lineHeight`,
`workbench.colorTheme`, `files.eol`, `files.trimTrailingWhitespace`,
`files.insertFinalNewline`, plus `extensions.*` for the host and deco's own
`deco.lsp.*` (see [Language servers](language-servers.md)) and
`deco.extensions.*` (below).

`files.eol` is **the line ending for new files**, as documented by VS Code. A file that already has line endings keeps them regardless of the setting, so opening a CRLF file with `"files.eol": "\n"` does not change every line. The setting applies to untitled buffers and to files with no line terminator. `auto`, the default, uses the platform's line ending.

### Settings a workspace cannot set

Most settings are resolved by precedence, and the source layer does not matter. Three settings are different because they **control how much authority executing code receives**, and a `.vscode/settings.json` can come with a cloned repository:

| Key | Default | What it does |
| --- | --- | --- |
| `deco.extensions.sandbox` | `"container"` | `"container"` runs the extension host in a container; `"process"` runs it as an ordinary child process. |
| `deco.extensions.containerRuntime` | first of `podman`, `docker` found | `"podman"`, `"docker"`, or an absolute path to one. Nothing else is accepted. |
| `deco.extensions.containerImage` | a digest-pinned Node image | The image the host runs in. **Must** be pinned as `name@sha256:<64 hex>`. |

These are read only from deco's defaults and your user settings. Workspace, folder and remote values are ignored and reported. The same rule applies to `deco.lsp.servers` for the same reason. See [Language servers](language-servers.md).

If the sandbox is `"container"` and no runtime can be found, deco **refuses to start the extension host** and names this setting in the error. It does not fall back to `"process"`. See [Extensions](extensions.md#the-container) for the reason.

Four keys have a **default** in deco but are not used yet: `editor.tabCompletion`, `editor.largeFileOptimizations`, `files.encoding` and
`workbench.editor.enablePreview`. Changing these values currently has no effect.

`editor.fontFamily`, `editor.fontSize` and `editor.lineHeight` apply only to the GPU frontend, because a terminal has no font size. The GPU frontend does not wrap or draw whitespace, so `editor.wordWrap`, `editor.wrappingIndent`, `editor.renderWhitespace`, `editor.rulers` and `editor.lineNumbers: "interval"` apply only to the terminal frontend. The [top-level README](https://github.com/sabas0ba/deco#readme) lists features that are not built.

Unknown keys are kept rather than rejected, because settings files written for VS Code contain many keys that deco does not use.

### The file outranks your indentation setting

`editor.detectIndentation` is **on** by default, as in VS Code. With it, pressing `tab` in a project indented with two spaces inserts two spaces rather than four. `editor.tabSize` applies to files whose indentation cannot be detected; detected indentation takes precedence.

![tab in a two-space file, then in a four-space one](img/detect-indentation.svg)

The status bar shows what one `tab` inserts, because the text does not show it. `(detected)` is added only when the file's indentation **differed** from your settings and was used. For example, a two-space file with `editor.tabSize` set to two shows no note.

The two properties are detected separately:

- **Tabs or spaces** is decided by count: indented lines that begin with a tab against indented lines that begin with a space. On an even split, the setting is used, because a mixed file is often partway through a conversion.
- **Width** is taken from the *differences* between consecutive lines' indents, not from the indents themselves. A file indented by four has lines starting at columns 0, 4, 8 and 12, and each is a multiple of two, so counting multiples would detect two spaces. The differences are all four. Ties go to the smaller width, in VS Code's order.

A tab-indented file determines *tabs or spaces* but not the display width of a tab, so `editor.tabSize` still sets that. Only the first 10,000 lines are examined, which is also VS Code's limit.

Set `"editor.detectIndentation": false` to always use your settings. The setting takes effect even if it is added in workspace settings after the file is open, because the detection result is stored and the file is not scanned again.

### What the view settings draw

Four settings change how the text area looks rather than how it behaves.

![The defaults, then whitespace everywhere, then rulers and interval line numbers](img/view-settings.svg)

| Setting | Values | What deco does |
| --- | --- | --- |
| `editor.renderWhitespace` | `none`, `selection` (default), `boundary`, `trailing`, `all` | `·` per space, `→` per tab, in `editorWhitespace.foreground` |
| `editor.rulers` | a list of columns | Tints that column, in `editorRuler.foreground` |
| `editor.lineNumbers` | `on` (default), `off`, `relative`, `interval` | `interval` numbers every tenth line, and the caret's |
| `editor.cursorStyle` | `line` (default), `block`, `underline`, and the `-thin` / `-outline` variants | Sets the terminal's caret shape |

**Whitespace.** `selection` is VS Code's default and shows markers only in the selection. `boundary` marks all whitespace *except* a single space between two words; otherwise it would be the same as `all`. A tab is drawn as one arrow at its starting column, with the rest of its span blank, as in VS Code. This keeps a tab distinguishable from the spaces it replaces.

**Rulers** are drawn as a thin line *between* two columns in VS Code, but a terminal has no space between cells. deco therefore tints the ruler column's cells at a quarter strength, so the column is visible and its text remains readable. Selections and find matches are drawn over the ruler tint.

**The caret shape** is set with `DECSCUSR`, the terminal escape sequence for caret shape, and restored when deco exits. Two details:

- **Nothing is sent unless `editor.cursorStyle` is set.** Without the setting, deco keeps the terminal's configured caret. Setting the key, even to its default `"line"`, enables the change.
- **`line-thin` and `block-outline` are mapped** to `line` and `block`. `DECSCUSR` supports a bar, a block and an underline, with no thin or hollow variants, so deco uses the nearest shape.

`editor.cursorBlinking` is not read. The caret blinks, which is VS Code's default.

### Saving on a delay

`files.autoSave: "afterDelay"` writes the file `files.autoSaveDelay` milliseconds after the last edit. It is **off by default**, as in VS Code, so files are written automatically only if you enable it.

The timer restarts on every edit, so the save does not happen while you are still typing. The save runs on an *idle* poll, the same poll that receives language-server diagnostics, so incoming keys postpone it. A clean document is never written, so an idle editor does not keep updating the file's modification time, which other tools may watch.

`files.autoSaveDelay` is clamped to at least 100 ms, so the file is not written on every keystroke.

**`onFocusChange` and `onWindowChange` are not supported.** Setting either adds an entry to the problem list shown at startup, where an unknown colour theme is also reported. Both require focus events: an editor losing focus corresponds to a tab switch, and a window losing focus is a terminal event that not every terminal sends. The warning prevents these modes from failing without notice.

## keybindings.json

deco uses VS Code's format, including chords, `when` clauses, per-platform `mac` keys, and `-command` removals:

```jsonc
[
  { "key": "ctrl+alt+n", "command": "editor.action.insertLineAfter",
    "when": "editorTextFocus && !editorReadonly" },

  // Take a default away.
  { "key": "ctrl+k ctrl+d", "command": "-editor.action.moveSelectionToNextFindMatch" }
]
```

Later rules take precedence, as in VS Code, so a user binding overrides a default with the same key and `when` clause. Context keys use VS Code's names, including `editorTextFocus`, `textInputFocus`, `editorHasSelection`, `editorHasMultipleSelections`, `suggestWidgetVisible`, `findWidgetVisible`, `findInputFocussed`, `editorHasDiagnostics`, `editorHasDefinitionProvider`, `editorHasDocumentFormattingProvider`, `isMac` and `isWindows`, so a `when` clause copied from an existing file has the same meaning.

A broken `keybindings.json` does not stop the editor from opening. Each entry that fails to parse is reported through the session's problem list and skipped, so the editor can still be used to fix the file.

## Colour themes

`workbench.colorTheme` names a theme. Two are built in: `Default Dark Modern` and `Default Light Modern`. Theme extensions from the marketplace work without changes, because a theme is declarative and starts no host process.

`ctrl+k ctrl+t` switches between them.

![Switching from the dark theme to the light one](img/color-theme.svg)

The right-hand column shows `dark`, `light` or `high contrast`, taken from the contribution's `uiTheme`. Theme labels often do not include this information.

The list starts with the two built-in themes, which are always available. It then lists every `contributes.themes` entry from every extension in deco's extensions directory **and VS Code's**, so themes installed for VS Code are available without copying. Each label is listed once; duplicates usually come from two installed versions of the same extension. Theme files are not read while the list is built.

**The choice lasts for the session.** To keep it, add `workbench.colorTheme` to your settings. The status bar shows the setting when the theme changes.

**deco does not write settings files.** Selecting a theme leaves `settings.json`, including its comments and formatting, unchanged. To keep a theme selected with `ctrl+k ctrl+t` across sessions, add the `workbench.colorTheme` setting shown in the status bar.

For the same reason, there is no per-workspace trust setting for [a workspace-defined language server](language-servers.md#configuring-a-server), because storing that decision would require writing a file.

If a theme cannot be read, deco reports the reason and keeps the current theme.

deco reads these parts of a theme file: `colors`, `tokenColors` (including TextMate scope matching), `semanticTokenColors`, and `include` chains for themes based on another theme. If the named theme cannot be found, deco uses the dark theme and reports the problem.

`.tmTheme` (plist) themes are **not** supported, and neither are `-` scope
exclusions in a scope selector.

The terminal frontend blends translucent colours, such as selections and find highlights, with the editor background, because a terminal cell has no alpha channel.

## Configuration errors do not stop the editor

Configuration errors do not prevent the editor from starting. An unknown theme falls back to the default, a broken keybinding is skipped, an unparseable workspace settings file is reported and ignored, and an unknown setting is kept. The editor collects these problems into a list that the frontend can show, so the configuration can be repaired from within the editor. The one exception is the extension sandbox: if `deco.extensions.sandbox` is `"container"` and no runtime can be found, the editor starts but the extension host does not (see [Settings a workspace cannot set](#settings-a-workspace-cannot-set)).
