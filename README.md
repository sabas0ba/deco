<img src="docs/assets/deco-mark.svg" alt="" width="84" align="right">

# deco

A lightweight, VS Code-compatible text editor written in Rust. No Electron.

deco reads your existing `settings.json`, `keybindings.json` and colour themes
and means the same thing by them that VS Code does. It runs in a terminal or in
a GPU-accelerated window, and it runs VS Code extensions in a Node process that
has had its ambient authority taken away.

**Status: early.** The compatibility layers — the parts that decide whether VS
Code compatibility is achievable at all — are implemented and tested. The editor
is usable for editing files. Several headline features are not built yet;
[what is not built](#what-is-not-built-yet) is explicit about which.

```console
$ cargo run -p deco -- src/main.rs          # terminal
$ cargo run -p deco --features gui -- src/main.rs --frontend gui
$ cargo run -p deco -- --print-config       # why isn't my setting applying?
```

## Documentation

[`docs/`](docs/README.md) documents each feature with an animation of it
running, and is published at
[sabas0ba.github.io/deco](https://sabas0ba.github.io/deco/):

| | |
| --- | --- |
| [Editing](docs/editing.md) | Motion, line and block comments, multiple cursors, undo |
| [Tabs](docs/tabs.md) | Several documents, one per tab; splitting; what a tab keeps |
| [Syntax highlighting](docs/highlighting.md) | Scopes, languages, choosing one, and why not tree-sitter |
| [Find and replace](docs/find-and-replace.md) | `ctrl+f`, `ctrl+h`, `F3`, and the multi-cursor find keys |
| [Running commands](docs/commands.md) | The command palette, quick open, go to symbol, search in files, go to line |
| [Language servers](docs/language-servers.md) | Diagnostics, hover, definition, references, completion, symbols, semantic tokens, formatting |
| [Configuration](docs/configuration.md) | `settings.json`, `keybindings.json`, themes, and where they are read from |
| [Extensions](docs/extensions.md) | The capability model, and why an extension gets less power here |
| [Remote](docs/remote.md) | SSH, container and WSL authorities — and which half exists |

The animations are generated from deco's own renderer by `cargo xtask docs`, and
`cargo xtask docs --check` runs in CI — so a demonstration cannot show a feature
behaving in a way the code does not.

![Multiple cursors added with ctrl+d](docs/img/multi-cursor.svg)

## Why these choices

**Rust, not Electron.** The editor is a native binary with a rope-backed text
model. The terminal build pulls in 44 third-party crates in total, and the
extension host pulls in no npm packages at all — see
[Dependencies](#dependencies).

**VS Code's own identifiers everywhere.** Commands are
`editor.action.commentLine`, not `deco.comment`. Settings are `editor.tabSize`.
Context keys are `editorHasSelection`. That is what makes an existing
configuration mean the same thing here, and it is a constraint on every new
feature rather than a shim bolted on at the edges.

**Frontend-agnostic core.** Nothing below `deco-tui` / `deco-gui` knows what a
terminal or a window is. Both frontends drive the same command set, and both
split rendering into a pure function (testable in CI, no display needed) and a
thin painter.

## Compatibility

| VS Code feature | deco |
| --- | --- |
| `settings.json` (JSONC, `[language]` overrides, scope layering) | Yes |
| `keybindings.json` (chords, `when` clauses, `-command` removals, per-platform keys) | Yes |
| Colour themes (`colors`, `tokenColors`, `semanticTokenColors`, `include` chains) | Yes |
| Syntax highlighting | 19 languages, from a lexer — see [why not tree-sitter](docs/highlighting.md#why-not-tree-sitter) |
| Command identifiers | Yes, for implemented commands |
| Theme extensions from the marketplace | Yes — declarative, no host process; `ctrl+k ctrl+t` lists them |
| Code extensions (`main`) | Protocol and sandbox built; host not yet wired to the editor |
| Remote SSH / containers / WSL | Authorities and transports built; server not yet |
| Language servers (LSP) | Diagnostics, hover, go-to-definition, references, completion, symbols, semantic tokens, formatting |
| Find and replace (`ctrl+f`, `ctrl+h`, `F3`, `ctrl+d`, `ctrl+shift+l`) | Literal search only — no regular expressions |
| Search in files (`ctrl+shift+f`) | Yes — bounded and synchronous, and it says so |
| Command palette (`ctrl+shift+p`), quick open (`ctrl+p`), go to line (`ctrl+g`) | Yes |
| `.tmTheme` (plist) themes, `-` scope exclusions | No |

Settings are read from deco's own configuration directory, falling back to VS
Code's (`Code/User/settings.json`) so an existing setup works without being
copied. Nothing is ever written back to VS Code's directory.

## Extensions, and why they are not like VS Code's

A VS Code extension is arbitrary JavaScript running with your full privileges.
It can read `~/.ssh/id_ed25519`, open a socket and spawn a shell, and nothing in
the extension API makes that visible, let alone preventable. Installing one is
trusting its author and every package in its `node_modules` with everything you
can reach.

deco keeps the separate Node process — extensions are JavaScript, and there is
no way around that — but removes its ambient authority. Three independent
layers:

1. **Node's permission model** (`--permission`, Node 20+) blocks filesystem,
   child-process and worker access below JavaScript, where an extension cannot
   argue with it. No `--allow-child-process`, no `--allow-fs-write`.
2. **The host bootstrap** removes the network globals and refuses to load `fs`,
   `net`, `http`, `child_process` and friends, so a blocked call produces a
   clear error naming its brokered replacement rather than a permission trap.
   Node's permission model does not cover the network; this layer is why that
   gap is closed.
3. **The capability broker** checks every request that does get through.

The broker's rules:

- **Deny by default.** A capability the manifest never declared is refused
  outright and never offered to the user. Consent cannot be manufactured at
  request time by an extension that did not say up front what it wanted.
- **Declaration is a ceiling, not a grant.** A declared capability still needs a
  decision — remembered, prompted for, or refused by policy.
- **Scopes are checked on resolved paths**, so `workspace` access cannot be
  walked out of with `..`, and `/project-secrets` does not pass as a child of
  `/project`.

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

**The honest trade-off:** an extension written for VS Code declares nothing, so
under deco it starts with no capabilities and will break wherever it reaches for
the filesystem or the network. deco does not guess a declaration on its behalf —
the alternative to breaking it is granting it everything silently.
`extensions.permissions.default` chooses between `prompt` (ask once, remember),
`deny` (right for shared machines and CI) and `allow` (declaration becomes the
only check).

Theme and grammar extensions have no `main`, never start a host process, and so
need no capability at all.

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
  deco-tui      terminal frontend (crossterm)
  deco-gui      GPU frontend (winit + wgpu + glyphon), behind the `gui` feature
  deco          the binary
extension-host/ the sandboxed Node host and the `vscode` API shim
```

Dependencies run one way: `deco-core` depends on nothing of deco's, and the
frontends depend on everything.

## What is not built yet

Named plainly, because a list of what works is only useful next to one of what
does not:

- **Remote development is half built.** `deco-remote` parses VS Code's
  `ssh-remote+host`, `wsl+Distro` and `dev-container+id` authorities and the
  `vscode-remote://` URIs they appear in, builds the SSH, `wsl.exe` and
  `docker exec` commands to reach them, and speaks the framed protocol the two
  ends would use. What does not exist yet is the other end: there is no
  `deco --server`, no provisioning it onto a remote, and no port forwarding.
- **Rename and code actions are not wired.** Diagnostics, hover
  (`ctrl+k ctrl+i`), go-to-definition (`F12`), references (`shift+f12`),
  document symbols (`ctrl+shift+o`), completion (`ctrl+space`), semantic tokens
  and formatting (`ctrl+shift+i`) work. Applying a
  rename means editing files that are not open — possible now that there are
  tabs, but it needs a `WorkspaceEdit` applied across several documents as one
  undoable action, which does not exist yet. Those keys say the feature is not
  implemented rather than pretending. Changes are sent as full-document syncs;
  the incremental path exists in `deco-lsp` but the editor does not yet track
  applied ranges.
- **Snippets are inserted without their placeholders.** deco advertises
  `snippetSupport: false` and several servers send them anyway, so `foo(${1:arg})`
  becomes `foo(arg)` and the status bar says so. There are no tab stops to jump
  between yet.
- **Go-to-definition across files opens a new tab** (or switches to the tab
  already holding the file), so unsaved work is never at risk. Several results
  are offered as a list rather than guessed between.
- **Syntax highlighting is lexical, and terminal-only.** 19 languages are
  coloured from a hand-written lexer emitting TextMate scopes, which the theme
  layer resolves exactly as it resolves a real grammar's — see
  [Syntax highlighting](docs/highlighting.md). What a lexer cannot do is anything
  structural: a type told from a variable by its declaration, or a language
  embedded in another. Markdown, HTML and XML are deliberately left plain for that
  reason, and `ctrl+k m` says which language a file is when its name does not. A
  language server's **semantic tokens** fill exactly that gap and are
  drawn over the lexer's colouring where a server provides them. The GPU frontend
  draws one colour per line.
- **The extension host is not connected.** The protocol, the capability broker,
  the sandbox and the `vscode` shim all exist and are tested against each other;
  the editor does not yet start a host or dispatch to one.
- **Search is literal, and only within the open file.** `ctrl+f` and `ctrl+h`
  open a find bar with a query, a replacement, a match count and highlighting;
  `F3`, `enter`, `ctrl+alt+enter` and `alt+c` / `alt+w` work as they do in VS
  Code, and the multi-cursor keys (`ctrl+d`, `ctrl+shift+l`, `ctrl+k ctrl+d`)
  search the same way. What is missing is regular expressions —
  `deco-core::search` is deliberately literal, and a regex mode needs its own
  escaping rules and its own error reporting for an invalid pattern — and
  find-in-files, which needs more than one document. Both say so when pressed
  rather than reporting an unknown command.
- **Tabs, splits, quick open and search in files, but no file tree.**
  Several documents open at once, one per tab (see [Tabs](docs/tabs.md)); `ctrl+p`
  opens any file in the workspace and `ctrl+shift+f` searches all of them,
  bounded and saying so; `ctrl+o` types a path for a file outside it, `ctrl+k s`
  saves every edited tab and `ctrl+shift+s` saves one somewhere else. Every remaining
  keybinding in that family **names the feature it is waiting on** rather than
  doing nothing, and a test over the whole default keymap keeps it that way. See
  [Running commands](docs/commands.md).
- **The GPU frontend draws text, a gutter and a caret.** Selection and
  current-line rectangles are computed and tested but not yet painted; there is
  no scrollbar, minimap or mouse input — and no chrome, so `ctrl+f` refuses there
  rather than opening a find bar the frontend cannot show.

## Building

Rust 1.82 or newer.

Everything CI runs is a `cargo xtask` subcommand, so any CI step can be
reproduced locally with the command CI itself uses:

```console
$ cargo xtask ci              # fmt, clippy, rustdoc and the tests
$ cargo xtask ci --lint-only  # …or just the checks
$ cargo xtask host-test       # the extension host's own tests
$ cargo xtask docs            # regenerate the animations in docs/img
$ cargo xtask docs --check    # …or check they still match the code
$ cargo xtask dist            # build and package a release for this machine
$ cargo xtask dist --target aarch64-apple-darwin
```

`cargo xtask dist` is the same code the release workflow runs, so a release can
be rehearsed without pushing a tag. It writes the archive and its `.sha256` to
`dist/`.

A pull request is checked on Linux. macOS and Windows — whose runner minutes
bill at ten and two times a Linux one — are checked on every merge to `main`, on
`workflow_dispatch`, and on a release tag, so nothing lands or ships unbuilt on
them; a pull request only gives up the per-push signal. Label a pull request
`ci:full` to get that signal back for a change that needs it, which is worth
doing before merging anything platform-specific: a path, a terminal or process
API, a `#[cfg]`, or a dependency with per-platform code.

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

The checker is about eighty lines in `xtask/src/commitlint.rs` rather than a
Node package, for the reason in [Dependencies](#dependencies). In CI it is a step
in the existing lint job rather than a job of its own: a separate job would
re-run checkout, the toolchain and the cache — three more third-party actions
executing, for a check that reads strings.

It only reads the commits a branch adds. The convention was adopted partway
through, and rewriting merged commits to satisfy it would change history other
people have already pulled.

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

`rust-analyzer`, `typescript-language-server`, `gopls` and `pyright` are defined
out of the box, and assume only that the program is on `PATH` — deco cannot
install a language server, so a missing one is reported as plainly as possible
rather than dressed up.

**A server defined by a workspace is not started.** `command` is a program run
with your privileges, and `.vscode/settings.json` arrives with a cloned
repository — so cloning must not be enough to execute something. A definition
from workspace or folder scope is refused, by name, in the status bar; move it
into your own `settings.json` if you want it. This holds even when the workspace
shadows an id you already trust: overriding `rust-analyzer` does not inherit the
built-in entry's trust, and it does not push your own definition aside either.

`command` and `args` are an argument vector. No shell is involved at any point,
so a `command` containing `;` or `$(…)` is a program name with punctuation in it
and nothing more.

What is wired up today: diagnostics (tallied in the status bar, walked with
`F8` / `shift+F8`), hover (`ctrl+k ctrl+i`, dismissed with `escape`),
go-to-definition (`F12`), references (`shift+f12`), go to symbol (`ctrl+shift+o`),
completion — `ctrl+space` to ask, or automatically on a character the server
nominates — semantic tokens drawn over the lexer's colouring, and formatting (`ctrl+shift+i` for the document,
`ctrl+k ctrl+f` for a selection).

In the completion list, `up`/`down` move, `tab` or `enter` accepts, `escape`
closes, and typing narrows it locally rather than asking the server again.

Formatting sends your own `editor.tabSize`, `editor.insertSpaces`,
`files.trimTrailingWhitespace` and `files.insertFinalNewline`, so a server
formats to the project's conventions rather than to its own defaults. The whole
batch of edits is one undo step, and a server that sends overlapping edits — which
the protocol forbids — is refused rather than guessed at: a file you can still
fix by hand is worth more than one quietly mangled.

The keys are gated on VS Code's own context keys — `editorHasDefinitionProvider`,
`suggestWidgetVisible` and friends — set from what the server actually offers, so
a binding is live exactly when the feature is, and `enter` keeps its ordinary
meaning whenever no list is open.

## Dependencies

An editor is a program you give your source code to, and every dependency is
another party you are trusting to reach it. The graph is therefore kept small
on purpose, and the size is checked rather than assumed:

| Build | Third-party crates |
| --- | --- |
| `deco` (terminal only, what releases ship) | 44 |
| `deco --features gui` | 155 |
| `xtask` (build tooling, never shipped) | 49 |
| extension host (Node) | **0** |

Everything in the terminal build is a crate with a long publishing history and
more than one maintainer's worth of use behind it: `ropey` for the text rope,
`crossterm` for the terminal, `serde`/`serde_json`, `thiserror`/`anyhow`,
`regex` (rust-lang), and the `unicode-*` crates from the unicode-rs project.
There are no git dependencies and no vendored forks — every entry in
`Cargo.lock` resolves to crates.io, and `cargo deny` fails the build if that
stops being true.

The rules that keep it that way:

- **`Cargo.lock` is committed and CI passes `--locked`.** Versions change in a
  reviewable diff instead of being re-resolved on every run, so a freshly
  published malicious version cannot enter through a build that nobody read.
- **`cargo deny` runs in CI** (`cargo xtask deny`) over RustSec advisories,
  crate sources, licences and a banned list. [`deny.toml`](deny.toml) says what
  each check is defending against, and every advisory exemption carries a
  reason and the condition for removing it.
- **GitHub Actions are pinned to commit SHAs**, because a tag is mutable and
  `@v4` otherwise means write access to this repository's CI.
- **The editor parses its own command line** ([`cli.rs`](crates/deco/src/cli.rs))
  rather than taking a derive-based argument parser, which removes fourteen
  crates — including a procedural macro, i.e. code that runs on the build
  machine — in exchange for about a hundred lines.
- **The extension host has no npm dependencies at all**, and a test fails if
  one appears. It is the one process that deliberately loads untrusted code, so
  nothing unreviewed belongs on the trusted side of that boundary.

The GPU frontend is the outlier: `wgpu`, `winit` and `glyphon` bring 111 crates between them, which is why it is behind a feature flag and not in
the shipped binary.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
