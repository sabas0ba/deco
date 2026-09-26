<img src="docs/assets/deco-mark.svg" alt="" width="84" align="right">

# deco

A lightweight, VS Code-compatible text editor written in Rust. No Electron.

deco reads VS Code's `settings.json`, `keybindings.json` and colour theme formats. Supported settings and commands use VS Code's identifiers. It runs in a terminal or a GPU-accelerated window. Code extensions run in a separate Node process, inside a container by default; supported file operations require permission checks through deco's capability broker.

**Status: early.** File editing and the compatibility layers described below are implemented and tested. See [what is not built](#what-is-not-built-yet) for missing features and frontend limitations.

```console
$ deco src/main.rs                # terminal
$ deco src/main.rs --frontend gui # a window, if built with --features gui
$ deco --print-config             # why isn't my setting applying?
```

## Install

### A prebuilt binary

Each release on the [releases page](https://github.com/sabas0ba/deco/releases/latest) provides one archive per platform and one `SHA256SUMS` file covering all archives.

| Platform | Archive |
| --- | --- |
| Linux x86-64 | `deco-x86_64-unknown-linux-gnu.tar.gz` |
| Linux x86-64, static | `deco-x86_64-unknown-linux-musl.tar.gz` |
| Linux ARM64 | `deco-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon | `deco-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `deco-x86_64-apple-darwin.tar.gz` |
| Windows x86-64 | `deco-x86_64-pc-windows-msvc.zip` |
| Windows ARM64 | `deco-aarch64-pc-windows-msvc.zip` |

Download the archive for your machine, verify it against `SHA256SUMS`, and put the binary on your `PATH`:

```console
$ curl -fsSLO https://github.com/sabas0ba/deco/releases/latest/download/deco-x86_64-unknown-linux-gnu.tar.gz
$ curl -fsSLO https://github.com/sabas0ba/deco/releases/latest/download/SHA256SUMS
$ sha256sum --ignore-missing -c SHA256SUMS
$ tar xzf deco-x86_64-unknown-linux-gnu.tar.gz
$ install -Dm755 deco-x86_64-unknown-linux-gnu/deco ~/.local/bin/deco
```

On macOS `shasum -a 256 -c SHA256SUMS` is the same check. On Windows, unzip the
archive and compare with `Get-FileHash deco-x86_64-pc-windows-msvc.zip`.

**Check the checksum before running the binary.** Compare the downloaded archive with the release's `SHA256SUMS`. Installation uses archive extraction; no shell-script installer is provided.

**Keep the archive's `extension-host/` beside the binary.** Move the whole extracted directory rather than the binary alone; otherwise code extensions cannot start. `deco` looks for the host next to its binary and at `../share/deco/`, and `DECO_HOST_BOOTSTRAP` sets its location explicitly. Editing, themes, language servers and remote work with the binary alone.

macOS binaries are not notarized, so Gatekeeper quarantines a downloaded binary. After checking the hash, run `xattr -d com.apple.quarantine deco`.

### With cargo

```console
$ cargo install --locked --git https://github.com/sabas0ba/deco --tag v0.1.0 deco
```

This installs only the terminal build and only the binary, so code extensions do not start unless `DECO_HOST_BOOTSTRAP` points to an `extension-host/src/bootstrap.js` from a checkout or a release archive. Add `--features gui` for the GPU frontend, which adds 111 crates and increases build time accordingly.

### From a checkout

```console
$ git clone https://github.com/sabas0ba/deco && cd deco
$ cargo run -p deco -- src/main.rs
```

See [Building](#building) for other build and test commands.

## Documentation

[`docs/`](docs/README.md) documents each feature with an animation and is published at [sabas0ba.github.io/deco](https://sabas0ba.github.io/deco/):

| | |
| --- | --- |
| [Editing](docs/editing.md) | Motion, line and block comments, multiple cursors, word wrap, undo |
| [Tabs](docs/tabs.md) | Several documents, one per tab; splitting; what a tab keeps |
| [Chrome](docs/chrome.md) | The side bar and the panel: `ctrl+b`, `ctrl+j`, where the space comes from, and where the keyboard is |
| [The file tree](docs/files.md) | Walking the workspace, opening files, and what it costs to open a big one |
| [Git](docs/git.md) | The branch, changed-line marks, side-by-side diffs, and a view that stages and commits |
| [Syntax highlighting](docs/highlighting.md) | Scopes, languages, choosing one, and why not tree-sitter |
| [Find and replace](docs/find-and-replace.md) | `ctrl+f`, `ctrl+h`, `F3`, the multi-cursor find keys, and replacing across the workspace |
| [Running commands](docs/commands.md) | The command palette, quick open, go to symbol, search in files, go to line |
| [Language servers](docs/language-servers.md) | Diagnostics, hover, definition, references, completion, symbols, semantic tokens, formatting, rename, code actions |
| [Configuration](docs/configuration.md) | `settings.json`, `keybindings.json`, themes, and where they are read from |
| [Extensions](docs/extensions.md) | The capability model, and why an extension gets less power here |
| [Remote](docs/remote.md) | SSH, container and WSL authorities, and the server that runs in the remote environment |
| [Testing](docs/testing.md) | Unit tests, end-to-end scenarios, and what each one is for |
| [Roadmap](docs/roadmap.md) | What VS Code has that deco does not, the plan for each, and what is worth building because deco is not Electron |

The animations are generated from deco's renderer by `cargo xtask docs`. CI runs `cargo xtask docs --check`, so the animations must match the current code.

![Multiple cursors added with ctrl+d](docs/img/multi-cursor.svg)

## Why these choices

**Rust, not Electron.** The editor is a native binary with a rope-backed text
model. The terminal build uses 52 third-party crates in total, and the extension host uses no npm packages. See [Dependencies](#dependencies).

The following measurements use a release build, one file and a 120×40 window:

| | 1,000 lines | 200,000 lines (10 MiB) |
| --- | --- | --- |
| Open | under 1 ms | 21 ms |
| Draw a frame | 304 µs | 304 µs |
| One keystroke | 8 µs | 10 µs |

Drawing and typing time does not grow with the file, because the hot paths are bounded by the **window** rather than the document. The lexer resumes from the earliest line an edit changed, wrapping and drawing process only the visible rows, and the rope makes an edit in the middle of ten megabytes cost the same as one at the start. Opening is linear in the file size.

A CI test checks this scaling: drawing and typing at 200,000 lines must stay within an order of magnitude of the same operations at 1,000 lines. The test compares a ratio rather than absolute time, so a loaded runner alone does not fail it. It detects an accidental walk from line zero, which would be about two hundred times slower rather than ten.

**VS Code's own identifiers everywhere.** Commands are
`editor.action.commentLine`, not `deco.comment`. Settings are `editor.tabSize`.
Context keys are `editorHasSelection`. Implemented features use these identifiers so existing configuration entries can address the corresponding settings and commands.

**Frontend-agnostic core.** Crates below `deco-tui` and `deco-gui` do not depend on terminal or window APIs. Both frontends use the same command set and separate layout calculation, testable without a display, from drawing.

## Compatibility

| VS Code feature | deco |
| --- | --- |
| `settings.json` (JSONC, `[language]` overrides, scope layering) | Yes |
| `keybindings.json` (chords, `when` clauses, `-command` removals, per-platform keys) | Yes |
| Colour themes (`colors`, `tokenColors`, `semanticTokenColors`, `include` chains) | Yes |
| Syntax highlighting | 19 languages, from a lexer — see [why not tree-sitter](docs/highlighting.md#why-not-tree-sitter) |
| Command identifiers | Yes, for implemented commands |
| Theme extensions from the marketplace | Yes — declarative, no host process; `ctrl+k ctrl+t` lists them |
| Code extensions (`main`) | Commands run: the palette lists them, choosing one starts a sandboxed host. The surface an extension can reach is registering a command, the message and status-bar calls, the `workspace.fs` family, and `workspace.applyEdit` — everything else is refused by name, see [Extensions](docs/extensions.md#what-an-extension-can-reach-today) |
| Remote SSH / containers / WSL | Open, edit and save a file on the remote environment with `--remote ssh-remote+host`, `--remote-install` installs deco there if it is missing, `--forward 3000` forwards a remote port, language servers and Git run remotely, `ctrl+shift+f` searches the remote environment, an extension's file access goes through the connection, and the remote machine's `machine-settings.json` is applied as the `remote` scope. Extension hosts still run locally — see [Remote](docs/remote.md) |
| Language servers (LSP) | Diagnostics, hover, go-to-definition, references, completion, symbols, semantic tokens, formatting, rename (`F2`, across files, one undo step), code actions (`ctrl+.`, with `codeAction/resolve`) |
| Find and replace (`ctrl+f`, `ctrl+h`, `F3`, `ctrl+d`, `ctrl+shift+l`, `ctrl+shift+h`) | Literal or regular-expression search (`alt+r`), with capture groups in replacements; replace across the workspace is one undoable edit |
| Search in files (`ctrl+shift+f`) | Yes — bounded and synchronous, and reports when a limit is reached |
| Command palette (`ctrl+shift+p`), quick open (`ctrl+p`), go to line (`ctrl+g`) | Yes |
| Side bar and panel (`ctrl+b`, `ctrl+j`, `workbench.sideBar.location`) | Regions, focus and the context keys — see [Chrome](docs/chrome.md). The side bar holds the file tree; the panel has no views yet and shows a label for the planned view |
| File tree / explorer (`ctrl+shift+e`, `list.*`, `revealInExplorer`) | Navigate, expand and open files — directories are read one at a time, `files.exclude` is honoured, and remote workspaces are supported |
| Changing files from the tree (`explorer.newFile`, `explorer.newFolder`, `renameFile`, `deleteFile`) | New file, new folder, rename and delete, with an undo of the tree's own; a rename retargets the open tab. Deleting is confirmed and cannot be undone — there is no trash, so `files.enableTrash` is not honoured. No drag, and none of it over a remote connection yet — see [The file tree](docs/files.md) |
| Git — status bar, gutter marks, diff view and source-control view (`workbench.view.scm`, `git.stage`, `git.commit`, `git.checkout`) | The branch, its distance from its upstream and how many files differ from `HEAD`; `┃`/`│`/`▔` beside added, changed and removed lines, following the buffer rather than the file on disk; and `ctrl+shift+g` for a view that opens side-by-side diffs, stages, unstages and commits. Local branches can be listed and switched after a preflight that accounts for local work; the same reads and operations run on the remote machine in a remote session. No discard and nothing that reaches the network — see [Git](docs/git.md) |
| Word wrap (`editor.wordWrap`, `editor.wrappingIndent`, `alt+z`) | Yes in the terminal |
| Detected indentation (`editor.detectIndentation`) | Yes — the status bar shows when detected indentation overrides the setting |
| Auto-closing brackets (`editor.autoClosingBrackets`) | Yes — no `autoSurround`, no `autoClosingDelete` |
| Auto-indent (`editor.autoIndent`) | Yes — `advanced` and `full` resolve to `brackets` because there is no language configuration |
| Trimming an auto-indent (`editor.trimAutoWhitespace`) | Yes — on the next edit rather than the next cursor move |
| Auto-save (`files.autoSave`) | `off` and `afterDelay`; the focus-driven values are reported as not honoured |
| Control characters (`editor.renderControlCharacters`) | Yes — and never written to the terminal as themselves, whatever the setting |
| `renderWhitespace`, `rulers`, `lineNumbers`, `cursorStyle` | Yes in the terminal — `cursorStyle`'s thin and hollow shapes map to the nearest supported shape |
| `.tmTheme` (plist) themes, `-` scope exclusions | No |

Settings are read from deco's configuration directory, with VS Code's (`Code/User/settings.json`) as a fallback, so an existing setup works without copying it. **deco does not write settings files**, neither VS Code's nor its own. As a result, a theme selected with `ctrl+k ctrl+t` must be added to the settings manually to persist; the status bar shows the line to add.

## Extensions, and why they are not like VS Code's

A VS Code extension is arbitrary JavaScript running with your full privileges. It can read `~/.ssh/id_ed25519`, open a socket and spawn a shell, and the extension API neither exposes nor prevents this. Installing an extension grants its author and every package in its `node_modules` access to everything your account can reach.

deco runs extensions in a separate Node process and restricts their access to system resources through four layers:

0. **A container**, from an image pinned by digest, with `--network=none`, `--read-only`, `--cap-drop=ALL` and **no mount of your workspace**. Extensions access files through the broker, so the container does not need the project. If no container runtime is installed, deco refuses to start the host rather than running it without this layer. `"deco.extensions.sandbox": "process"` explicitly selects running without a container. See [Extensions](docs/extensions.md#the-container).
1. **Node's permission model** (`--permission`, Node 22.13+) blocks filesystem, child-process and worker access below the JavaScript level, where extension code cannot bypass it. No `--allow-child-process` or `--allow-fs-write` is passed. The flag is also used inside the container, so each layer applies independently.
2. **The host bootstrap** removes the network globals and refuses to load `fs`, `net`, `http`, `child_process` and similar modules. A blocked call produces a clear error that names its brokered replacement. Node's permission model does not cover the network, and this layer covers that gap.
3. **The capability broker** checks every request that passes the other layers.

The broker's rules:

- **Deny by default.** A capability not declared in the manifest is refused and never offered to the user, so an extension cannot request consent at runtime for a capability it did not declare.
- **Declaration is an upper limit, not a grant.** A declared capability still requires a decision: remembered, prompted for, or refused by policy.
- **Scopes are checked on resolved paths**, so `..` cannot escape `workspace` access, and `/project-secrets` is not treated as a child of `/project`.

An extension declares what it wants in a `deco` section that VS Code ignores:

```jsonc
{
  "name": "my-extension",
  "main": "./out/extension.js",
  "deco": {
    "capabilities": [
      { "capability": "readFile", "scope": { "kind": "workspace" } },
      { "capability": "network", "host": "*.example.com" }
    ]
  }
}
```

**Compatibility limitation:** an extension written for VS Code declares no capabilities, so under deco it starts with none and fails when it accesses the filesystem or the network. deco does not infer a declaration, because the only alternative would be to grant everything without notice. `extensions.permissions.default` selects `prompt` (ask once, remember), `deny` (suitable for shared machines and CI) or `allow` (the declaration is the only check).

Theme and grammar extensions have no `main` and never start a host process, so they need no capabilities.

## Layout

```
crates/
  deco-core     rope buffer, UTF-16 positions, selections, invertible edits, undo,
                literal search
  deco-syntax   a lexer per language, emitting TextMate scopes for the theme
  deco-config   JSONC reader; default < user < remote < workspace < folder
  deco-keymap   key parsing, when-clause engine, chord resolution, default keymap
  deco-lsp      LSP client: framing, lifecycle, capabilities, sync, diagnostics,
                semantic tokens, and the supervisor that runs a server
                and pumps its stdio
  deco-theme    colour themes: TextMate scopes, semantic tokens, include chains
  deco-editor   the command set and the editor session — no terminal, no window
  deco-ext      manifests, activation, and the capability model
  deco-remote   remote authorities, SSH/WSL/container transports, wire framing
  deco-scm      what `git` says: porcelain v2 parsed, a Myers line diff, the
                binary run, no library
  deco-tui      terminal frontend (crossterm)
  deco-gui      GPU frontend (winit + wgpu + glyphon), behind the `gui` feature
  deco          the binary
extension-host/ the sandboxed Node host and the `vscode` API shim
```

Dependencies run one way: `deco-core` depends on no other deco crate, and the frontends depend on all of them.

## What is not built yet

This section lists missing features and limitations of existing features. Larger features that do not exist yet, such as an integrated terminal, tasks, a test runner, self-update and debugging, each have a plan in the [Roadmap](docs/roadmap.md):

- **Git reviews, stages, commits and switches local branches.** The status bar shows the branch and the number of changed files, the gutter marks changed lines, and `ctrl+shift+g` opens a view with side-by-side diffs that stages, unstages and commits. `git.checkout` lists branches and shows the effect before switching. Git support does not **discard** changes, because `git clean` has no undo and no trash, and does not **access the network**, which requires credentials. In a remote session, status, committed text and writes run on the machine that holds the repository. The GPU frontend computes the gutter marks but does not draw them yet. See [Git](docs/git.md).
- **The file tree has no watcher, no mouse and no remote.** `ctrl+b` shows it, `ctrl+shift+e` focuses it, and the arrow keys navigate it. A directory is read when it is expanded, so the cost of a large workspace depends on the visible rows. Creating, renaming and deleting are built, with a separate undo stack for the tree. Missing: a file created by another program appears only when the directory is read again; the tree is keyboard-only in both frontends, so there is no drag and no way to move a file to another folder; and create, rename and delete do not work over a remote connection yet. See [The file tree](docs/files.md#not-built-yet).
- **The panel is built and empty.** `ctrl+j` opens the panel region with layout, focus and VS Code's context keys, and both frontends draw it. The terminal, problems and output views are not implemented yet, and the panel shows a label saying so. See [Chrome](docs/chrome.md).
- **Remote development runs everything except the extension host in the remote environment.** `deco --remote ssh-remote+myhost --workspace /home/u/project src/main.rs` starts `deco --server --stdio` in the remote environment, fetches the file, and writes it back on `ctrl+s`; `ctrl+p` lists the remote workspace. The server refuses all paths outside the directory it was given, including paths reached through symlinks. `--remote-install` sends the local binary to a remote that has no deco, only when requested and only to a deco server. `--forward 3000` forwards a remote port using the remote deco as the tunnel, so it works over containers and WSL as well as SSH; both ends listen only on loopback. Language servers and Git run on the remote, using the same `deco.lsp.servers` definitions and source-control commands as locally. Project search also runs on the machine that holds the files, and an extension's `readFile`/`writeFile` calls are answered through the connection rather than from the local disk. The remote's `machine-settings.json` becomes the `remote` settings layer. It is untrusted, so a language server it defines is refused, as one defined by workspace settings is. `--remote-install-download` provisions a remote of a *different* platform by downloading that release and checking it against the release's `SHA256SUMS` before sending it. It is separate from `--remote-install` because network access requires a broader permission than copying the running binary. Missing: the extension *host* still runs locally, so a capability to run a program runs it on the local machine.
- **A code action that is only a server command is declined.** Diagnostics, hover (`ctrl+k ctrl+i`), go-to-definition (`F12`), references (`shift+f12`), document symbols (`ctrl+shift+o`), completion (`ctrl+space`), semantic tokens, formatting (`ctrl+shift+i`), rename (`F2`) and code actions (`ctrl+.`, including `codeAction/resolve`) work. `ctrl+.` does not run an action that consists only of a command for the client to execute. That requires `workspace/executeCommand`, whose effect arrives as a server request to change files. deco reports the command name instead. Changes are sent as full-document syncs. The incremental path exists in `deco-lsp`, but the editor does not yet track applied ranges, and only the visible document is synchronised.
- **Numeric snippet tab stops and document variables work for completions.** Unique `$1`, `${1}` and
  `${1:arg}` fields support Tab/Shift+Tab navigation and `$0` finishes. The
  [supported subset and demonstration](docs/language-servers.md#snippet-tab-stops)
  describe the limits. Full LSP `snippetSupport` remains false; unsupported
  snippets use the existing text-only fallback and report that in the status bar.
- **Go-to-definition across files opens a new tab** (or switches to the tab already holding the file), so unsaved work in the current document is kept. Multiple results are shown as a list.
- **Syntax highlighting is lexical, and terminal-only.** 19 languages are highlighted by a hand-written lexer that emits TextMate scopes, which the theme layer resolves in the same way as scopes from a grammar. See [Syntax highlighting](docs/highlighting.md). A lexer cannot handle structural cases, such as distinguishing a type from a variable by its declaration, or a language embedded in another language. For this reason Markdown, HTML and XML are not highlighted, and `ctrl+k m` sets a file's language when its name does not identify it. A language server's **semantic tokens** cover these cases and are drawn over the lexer's colouring where a server provides them. The GPU frontend draws one colour per line.
- **The extension host is connected, and the API it can use is small.** An installed extension's commands are listed in the palette. Choosing one starts the host under `node`, and the session answers its requests through the broker: messages, `workspace.fs` and `workspace.applyEdit`. The rest of the API is missing: there is no activation on opening a file or on startup, and no editor state, quick pick, tree views, webviews or debug adapters. The `process`, `network`, `env`, `clipboard`, `secrets` and `openExternal` capabilities pass through the broker and are then refused by name because they are not implemented. See [What an extension can reach](docs/extensions.md#what-an-extension-can-reach-today).
- **Search has no persistent results view.** `ctrl+f` and `ctrl+h` open a find bar with a query, a replacement, a match count and highlighting. `F3`, `enter`, `ctrl+alt+enter` and `alt+c` / `alt+w` / `alt+r` work as in VS Code, the multi-cursor keys (`ctrl+d`, `ctrl+shift+l`, `ctrl+k ctrl+d`) use the same search, and `ctrl+shift+f` searches every file in the workspace with separate matching options. [Regular expressions](docs/find-and-replace.md#regular-expressions) use the `regex` crate's syntax, which has no look-around or backreferences. Search-in-files matches are shown in a picker rather than a results view. Replacing across the workspace is built: `ctrl+shift+h` prompts for the search text and the replacement, and applies the change as one undoable edit.
- **Tabs, splits, quick open and search in files.** Several documents can be open at once, one per tab (see [Tabs](docs/tabs.md)). `ctrl+p` opens any file in the workspace, `ctrl+shift+f` searches all of them within limits and reports when a limit is reached, `ctrl+o` opens a file outside the workspace by path, `ctrl+k s` saves every edited tab and `ctrl+shift+s` saves one to another path. Each remaining keybinding in this group **names the missing feature** instead of doing nothing, and a test over the whole default keymap enforces this. See [Running commands](docs/commands.md).
- **The GPU frontend draws text, a gutter and a caret.** Selection and current-line rectangles are computed and tested but not yet drawn. There is no scrollbar, minimap or mouse input. It lays out one line per row without whitespace markers or rulers, so `editor.wordWrap`, `editor.renderWhitespace` and `editor.rulers` have no effect there. It also has no chrome, so `ctrl+f` is refused rather than opening a find bar that cannot be shown.
- **Four settings with defaults in deco are not used yet:** `editor.tabCompletion`, `editor.largeFileOptimizations`, `files.encoding` and `workbench.editor.enablePreview`. `editor.largeFileOptimizations` currently has no effect to disable: VS Code uses it to stop tokenizing and wrapping above a file size, and in deco both are already bounded by the window (see the table above). They are listed here because a shipped default could suggest that the setting is supported. The three `extensions.host.*` defaults (`enabled`, `startupTimeoutMs`, `maxOldSpaceSizeMb`) are not read either: the extension host, described above, uses built-in limits.
- **Bidirectional overrides are not marked.** `U+202E` and related characters can make a line display differently from its actual content, which is the Trojan Source class of attack. They are printable rather than control characters, so the control-character substitution does not change them. VS Code handles them with `editor.unicodeHighlight.*`, which deco does not read.
- **Word wrap breaks at whitespace rather than by Unicode UAX #14.** Whitespace breaking does not prevent a closing bracket from starting a row. UAX #14 support needs a table that would add a dependency for presentation only. See [Word wrap](docs/editing.md#word-wrap).

## Building

Rust 1.85 or newer.

Every CI step is a `cargo xtask` subcommand, so it can be reproduced locally with the same command:

```console
$ cargo xtask ci              # fmt, clippy, rustdoc and the tests
$ cargo xtask ci --lint-only  # …or just the checks
$ cargo xtask cross           # the Windows and macOS targets, from Linux
$ cargo xtask cross --check-only  # …without the Wine half
$ cargo xtask host-test       # the extension host's own tests
$ cargo xtask docs            # regenerate the animations in docs/img
$ cargo xtask docs --check    # …or check they still match the code
$ cargo xtask dist            # build and package a release for this machine
$ cargo xtask dist --target aarch64-apple-darwin
```

The release workflow runs the same `cargo xtask dist` code, so a release can be tested locally without pushing a tag. It writes the archive and its `.sha256` to `dist/`.

## Releasing

A release is created from a tag. There are two ways to create one, with the same result:

- **From a checkout.** `git tag -a v0.1.0 -m "deco 0.1.0" && git push origin v0.1.0`.
- **From the Actions tab.** Run **Release** against `main` and enter the tag. The workflow creates the tag at the commit the run was dispatched from and builds it in the same run, so creating a release needs no terminal and no push rights beyond permission to run the workflow. An existing tag is released as it is, not moved.

In both cases the workflow builds all seven targets with the same `cargo xtask dist`, merges the per-artifact hashes into one `SHA256SUMS`, and publishes the release with a body taken from this repository:

```console
$ cargo xtask release-notes --tag v0.1.0   # what the release will say
```

The notes are the `## 0.1.0` section of [CHANGELOG.md](CHANGELOG.md), not a separate description entered in the release form. This keeps one description per version, stored in the repository. **A tag with no section fails the release**, because a failed workflow can be run again but a published release cannot be unpublished. A unit test checks on every push that the changelog has a section for the current version, so the error is found before tagging.

## What CI runs where

Routine checks run on Linux. macOS and Windows runner minutes cost ten and two times as much as Linux minutes, so the macOS and Windows runners are used only for a release tag, for `workflow_dispatch`, and for a pull request labelled `ci:full`. Every shipped build is still built and tested on its target platform.

Actions also bills each job's wall-clock time rounded up to the minute, which is most of the cost for checks that take twenty seconds. Each check therefore runs on the least frequent event that still covers what it checks, and checks triggered by the same event share a job:

| When | What runs |
| --- | --- |
| every push to a pull request | `check` — fmt, clippy, rustdoc, commit messages, `docs --check` — and `test` — the workspace suite and the extension host |
| a merge to `main` | `test` alone. A pull request's jobs already ran against the merge of its head and base, so the remaining risk is that `main` changed after that run. Only the tests detect a semantic conflict in a clean merge |
| daily, on `main` | `cargo xtask cross`, the three Linux `dist` targets, the MSRV check and `cargo deny`. These checks are not usually affected by code changes such as renaming a function; they fail when a dependency, a target or the advisory database changes |
| a pull request labelled `ci:full`, or `workflow_dispatch` | all of the above, plus the real macOS and Windows runners |
| a release tag | all of the above, except the packaging jobs — the release workflow builds all seven targets from the same `cargo xtask dist` on the same tag, and its copy is the one that ships |

Label a pull request `ci:full` for platform-specific changes and for dependency updates, because dependency updates can change the MSRV and the supply-chain policy results.

Between tags, `cargo xtask cross` replaces the macOS and Windows runners with checks on a Linux runner:

- **A type check of all four shipped Apple and Windows triples.** `cargo check` stops before linking, so it needs no MSVC toolchain or Apple SDK, only the prebuilt `std` from rustup. It compiles code that a Linux build does not compile: the `%APPDATA%` branch of the config paths, the `cmd`-rather-than-`ctrl` branch of the keymap, and, because it uses `--all-features`, the GPU frontend's per-platform windowing.
- **The test suite as Windows binaries, under Wine.** The tests are built for `x86_64-pc-windows-gnu` with MinGW and run through cargo's target runner, so the `#[cfg(windows)]` paths execute, including process spawning. Two tests are skipped: crossterm on Windows sends its commands to the console rather than to the given writer, and Wine on a CI runner has no terminal from which to create a console. A real Windows runner has a console and runs them for a release tag.

These checks do not fully replace the native runners. macOS gets a compile check and no runtime check. Wine uses the GNU ABI rather than MSVC, and where Wine's Win32 implementation differs from Microsoft's, a test can pass under Wine and fail on Windows. The checks also run daily rather than on every push, so a regression they would detect can remain on `main` for a day, and a regression only a native runner detects can remain until the release tag. Label a pull request `ci:full` to run both earlier for platform-specific changes: paths, terminal or process APIs, `#[cfg]`, or dependencies with per-platform code.

Running these checks locally requires the targets and the Windows toolchain:

```console
$ rustup target add x86_64-pc-windows-msvc aarch64-pc-windows-msvc \
    x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-gnu
$ sudo apt-get install -y mingw-w64 wine64
$ cargo xtask cross
```

`--check-only` skips the Wine tests, so Wine is not required, and `--wine-only` skips the type checks.

## Commit messages

[Conventional Commits](https://www.conventionalcommits.org/), checked in CI:

```console
$ cargo xtask commitlint                          # the commits this branch adds
$ cargo xtask commitlint --range main..HEAD
```

```
feat(lsp): complete identifiers from the language server
fix(tui): stop a long completion label overflowing the row
ci: check commit messages against conventional commits
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`,
`chore`, `revert`. A scope is optional and is usually a crate without its `deco-`
prefix. `!` before the colon marks a breaking change.

The checker is about eighty lines in `xtask/src/commitlint.rs` rather than a Node package, for the reason in [Dependencies](#dependencies). In CI it is a step in the existing lint job rather than a separate job, which would repeat checkout, toolchain setup and caching and run three more third-party actions for a string check.

It checks only the commits a branch adds. The convention was adopted after the project started, and rewriting merged commits would change history that others have already pulled.

The GPU frontend is behind a feature flag because wgpu and winit dominate build
time:

```console
$ cargo build --release -p deco                  # terminal only
$ cargo build --release -p deco --features gui   # both frontends
```

On Linux the GPU build needs `libx11-dev` and `libxkbcommon-dev`.

## Language servers

A server is configured under `deco.lsp.servers`, keyed by an id you choose:

```jsonc
{
  "deco.lsp.enabled": true,
  "deco.lsp.servers": {
    "rust-analyzer": {
      "languages": ["rust"],
      "command": "rust-analyzer",
      "args": [],
      "env": { "RA_LOG": "info" },
      "initializationOptions": { "cargo": { "features": "all" } }
    }
  }
}
```

`rust-analyzer`, `typescript-language-server`, `gopls` and `pyright` are defined by default and require only that the program is on `PATH`. deco cannot install a language server, so a missing server is reported directly.

**A server defined by a workspace is not started.** `command` is a program that runs with your privileges, and `.vscode/settings.json` can come with a cloned repository, so cloning alone must not execute a program. A definition from workspace or folder scope is refused and named in the status bar; move it into your own `settings.json` to use it. This also applies when the workspace redefines an id you already trust: a workspace definition of `rust-analyzer` does not inherit the built-in entry's trust and does not replace your own definition.

`command` and `args` form an argument vector. No shell is used, so a `command` containing `;` or `$(…)` is treated as a program name containing those characters.

Supported features: diagnostics (counted in the status bar, navigated with `F8` / `shift+F8`), hover (`ctrl+k ctrl+i`, dismissed with `escape`), go-to-definition (`F12`), references (`shift+f12`), go to symbol (`ctrl+shift+o`), completion (`ctrl+space`, or automatically on a trigger character specified by the server), semantic tokens drawn over the lexer's colouring, and formatting (`ctrl+shift+i` for the document, `ctrl+k ctrl+f` for a selection).

In the completion list, `up`/`down` move the selection, `tab` or `enter` accepts, `escape` closes, and typing filters the list locally without a new server request.

Formatting sends your `editor.tabSize`, `editor.insertSpaces`, `files.trimTrailingWhitespace` and `files.insertFinalNewline`, so the server formats according to the project's settings rather than its own defaults. The whole batch of edits is one undo step. Overlapping edits are rejected without changing the document because their result is not well-defined.

The keys are gated on VS Code's context keys, such as `editorHasDefinitionProvider` and `suggestWidgetVisible`, which are set from the server's reported capabilities. A binding is active only when its feature is available, and `enter` keeps its normal behaviour when no list is open.

## Dependencies

An editor has access to your source code, and every dependency is code that you trust with that access. The dependency graph is therefore kept small, and its size is checked:

| Build | Third-party crates |
| --- | --- |
| `deco` (terminal only, what releases ship) | 52 |
| `deco --features gui` | 163 |
| `xtask` (build tooling, never shipped) | 49 |
| extension host (Node) | **0** |

Every crate in the terminal build has a long publishing history and wide use: `ropey` for the text rope, `crossterm` for the terminal, `serde`/`serde_json`, `thiserror`/`anyhow`, `regex` (rust-lang), the `unicode-*` crates from the unicode-rs project, and `sha2` from RustCrypto.

`sha2` has one purpose: `--remote-install-download` checks a release archive against the `SHA256SUMS` published with that release, and **that check is implemented in deco**. Downloading and unpacking are delegated to `curl` and `tar`, which are available on every supported platform. This is why the count above is 52 rather than 93: an in-process HTTPS client adds about forty crates and a vendored TLS stack. deco computes and compares the checksum before passing the archive to `tar`. See [`crates/deco-remote/src/fetch.rs`](crates/deco-remote/src/fetch.rs). There are no git dependencies and no vendored forks. Every entry in `Cargo.lock` resolves to crates.io, and `cargo deny` fails the build otherwise.

Dependency rules:

- **`Cargo.lock` is committed and CI passes `--locked`.** Versions change only through a reviewable diff instead of being re-resolved on every run, so a newly published malicious version cannot enter an unreviewed build.
- **`cargo deny` runs in CI** (`cargo xtask deny`) and checks RustSec advisories, crate sources, licences and a banned list. [`deny.toml`](deny.toml) documents the purpose of each check, and every advisory exemption has a reason and a removal condition.
- **GitHub Actions are pinned to commit SHAs**, because tags are mutable and anyone who can move `@v4` would otherwise gain write access to this repository's CI.
- **The editor parses its own command line** ([`cli.rs`](crates/deco/src/cli.rs)) instead of using a derive-based argument parser. This removes fourteen crates, including a procedural macro that runs on the build machine, at the cost of about a hundred lines.
- **The extension host has no npm dependencies**, and a test fails if one is added. It is the only process that intentionally loads untrusted code, so its trusted side contains only reviewed code.

The GPU frontend is the exception: `wgpu`, `winit` and `glyphon` add 111 crates, so it is behind a feature flag and not in the shipped binary.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
