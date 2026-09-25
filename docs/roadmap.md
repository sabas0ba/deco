# Roadmap

> **Status:** this page describes planned features, their prerequisites and proposed implementation steps. Sections also identify completed prerequisites and link to their documentation. No delivery dates are specified.

Most of this page covers features that VS Code already has. The last part, [Beyond VS Code](#beyond-vs-code), covers features that are practical *because* deco is not built on Electron.

## Where deco stands against VS Code

Implemented features are documented, with animations, on the other pages of this site. The [top-level README](https://github.com/sabas0ba/deco#readme) contains the authoritative compatibility table. In summary: configuration, keybindings and themes are read with VS Code's meaning; editing, tabs, splits, find and replace, quick open, the command palette, workspace-wide search, syntax highlighting and the listed language-server features work; extensions run in a sandboxed host; remote sessions can open, edit and save files.

Features that VS Code has and deco does not, with their prerequisites:

| Missing | Depends on |
| --- | --- |
| Git — discard, push/pull ([the rest is built](git.md)) | nothing; each is its own decision about what it may lose or ask for |
| Integrated terminal | a PTY dependency; its home is the [panel](chrome.md) |
| Task runner (`tasks.json`, `ctrl+shift+b`) | the terminal, for somewhere to run |
| Test runner | the task runner, and later the extension host |
| Self-update | nothing — `cargo xtask dist` already builds what it would install |
| AI features — inline completions, chat, agent mode, MCP (`chat.disableAIFeatures`) | the extension host, ghost text, and the panel; agents additionally on `WorkspaceEdit` and the terminal; the off switch depends on nothing and comes first |
| Debugging (DAP) | the panel; by far the largest item here |
| Full snippet syntax and user snippet files ([numeric completion fields work](language-servers.md#snippet-tab-stops)) | linked/nested field tracking and variable/transform expansion |

These dependencies determine the order of the sections below. One foundation remains: **implementing the remaining extension-host APIs**. The other two are built. The **[side bar and panel](chrome.md)** provide regions for the file tree, source-control view and planned terminal. **`WorkspaceEdit`** ([below](#the-gaps-behind-the-features)) is already used by rename, code actions and workspace-wide replace, and is intended for the tree's mutations and an agent's turn.

Two rules from the existing implementation apply to every section below:

- **VS Code's identifiers, exactly.** The commands are `git.stage` and `workbench.action.terminal.toggleTerminal`, and the settings are `git.enabled` and `terminal.integrated.defaultProfile.linux`. deco does not add a `deco.*` synonym for anything VS Code already names, so an existing `keybindings.json` keeps its meaning.
- **A key whose feature is missing reports it.** `` ctrl+` `` is bound and reports that the integrated terminal is not implemented; a test over the default keymap checks that unimplemented commands report an error. Once the feature is implemented, the command performs the operation; for example, `ctrl+b` and `ctrl+j` now toggle the side bar and panel.

## Git

**Built.** The status bar shows the branch and the changes relative to it, the gutter marks changed lines, a source-control view stages, unstages and commits, and a local-branch picker switches branches after a checkout preflight. Git has [a page of its own](git.md); this section covers what remains.

**What VS Code has that deco still does not.**

- **Discard.** `git clean` and `git checkout --` discard work with no undo and no trash. For the same reason, deco's file tree does not delete without confirmation, and discard needs a comparable safeguard.
- **Push, pull, fetch.** These need credentials, and deco would have to be trusted to handle credential prompts. They should be designed deliberately, not added as an afterthought to a view that already works.
**Steps.**

1. Discard changes, with confirmation that identifies affected files and explains whether recovery is possible.
2. Push, pull and fetch, after credential prompting is implemented.

## The integrated terminal

**What VS Code has.** Terminals in the panel, ``ctrl+` `` to toggle, profiles,
and the `terminal.integrated.*` settings family.

**What deco has.** The [panel](chrome.md) to hold a terminal, and the binding: ``ctrl+` `` is already bound to `workbench.action.terminal.toggleTerminal` and reports by name that it is not implemented. The terminal implementation must handle nested terminal input and output because deco's TUI runs inside an existing terminal.

**The plan.** A PTY per terminal, a VT parser that updates a screen model, and the panel region drawing that model through the same pure render path, so a terminal's screen can be checked in CI like any other layout. This is the only section that adds a significant dependency (a PTY crate, and possibly a VT parser). The README publishes its count of 44 crates, so the addition is made once, listed in the dependency table, and justified by this feature alone. In the GUI the same model is drawn into the window; in the TUI the inner terminal's cells map onto the panel's cells, with colours through the existing theme layer. In a remote session the shell runs on the machine holding the files: the server gains a PTY endpoint over the existing `deco-remote` framing, in the same way that language servers already run there.

**Steps.**

1. Choose and justify the PTY dependency; spawn the user's shell (`terminal.integrated.defaultProfile.*`) and read its output.
2. A screen model: grid, cursor, scrollback, SGR colours mapped through
   `deco-theme`.
3. Panel view and focus routing: keys go to the shell when the terminal has focus, except the keys VS Code also reserves (`ctrl+j`, ``ctrl+` ``).
4. Remote PTYs over the session connection.

## The task runner

**What VS Code has.** `tasks.json` (build tasks, `ctrl+shift+b` =
`workbench.action.tasks.build`, `workbench.action.tasks.runTask` from the
palette), problem matchers that turn output into diagnostics.

**What deco has.** Nothing, including the bindings.

**The plan.** Read `.vscode/tasks.json`, the file users already have, with `deco-config`'s existing JSONC reader. A task runs in the integrated terminal, which is why this section follows the terminal section, and its exit status is shown in the status bar. Problem matchers follow: the named matchers (`$rustc`, `$tsc`) map output lines into the same diagnostics pipeline that LSP uses, so a compile error from a task is underlined in the same way as a language-server diagnostic.

**Steps.**

1. Parse `tasks.json` (`shell` and `process` types, `group.kind == "build"`);
   surface tasks in the palette under their VS Code command names.
2. Run in a terminal; report exit status.
3. The common problem matchers, into `deco-lsp`'s diagnostics store.
4. `${workspaceFolder}` and related variables, resolved against the session.

## The test runner

**What VS Code has.** A testing view fed by extensions through the
`vscode.tests` API; run/debug at the test, file and suite level; results inline
in the gutter.

**What deco has.** Nothing. (deco's *own* test suite is documented in
[Testing](testing.md); this chapter is about running *your* tests.)

**The plan.** Two stages. First, tests run as tasks: a `group.kind == "test"` task is bound to VS Code's test-task command and run in the terminal. This needs no new UI and is useful immediately. Second, a testing view once the extension host supports it. In VS Code, extensions *discover* tests, and deco will run those extensions instead of hardcoding one runner per language. The full feature therefore depends on the extension-host work described below, and the mediated API it needs (`tests.*`) requires an entry in the capability table, as `readFile` has.

**Steps.**

1. Test-group tasks through the task runner.
2. The `vscode.tests` surface in the host shim, brokered like the rest.
3. A side-bar testing view rendering what extensions report; run through tasks,
   results to gutter marks.

## Self-update

**What VS Code has.** Background download and install, `update.mode` to turn it
off.

**What deco has.** `cargo xtask dist` builds the release archive and writes its
`.sha256` beside it; the release workflow publishes both for seven targets. No
updater.

**The plan.** `deco --update`: query GitHub Releases for the latest tag, compare versions, download the archive for this target, verify the checksum, and replace the running binary atomically (write beside it, then rename over it). Windows needs special rename handling, which `cargo xtask cross`'s Wine run exists to test. Update checks would run only on explicit request. The proposed implementation would support `update.mode` values `none` and `manual`, and display release information only after a requested check. A package-manager install, where the user should not replace the binary directly, is detected and refused with a message naming the correct command.

**Steps.**

1. Version check against the Releases API; `--update --check-only` prints and
   exits.
2. Download, verify against the published `.sha256`, stage beside the binary.
3. Atomic replace, per platform, with tests where tests can run (Wine covers
   the Windows path daily).
4. The read-only-install detection and its refusal message.

## AI features

**What VS Code has.** Copilot integrated into the core rather than shipped separately: inline completions as ghost text, a chat view, and an agent mode that edits files. For users who want none of these, the single setting `chat.disableAIFeatures` hides all of them.

**What deco has.** No AI integration is implemented. The proposed features would be optional and require an extension.

**The plan.** Provide AI features through **extensions** using capability checks for supported file, process and network operations. The required extension APIs and policy controls described below still need implementation.

- **Disable declared AI integrations before activation.** The proposed `chat.disableAIFeatures: true` setting would block extensions that declare AI features and hide their UI. This requires a manifest declaration and activation checks; it would not identify undeclared AI use by arbitrary extension code.
- **Using AI is an explicit grant, not a default.** An AI extension declares the host it connects to, for example `{"capability": "network", "host": "api.anthropic.com"}`. The declaration is visible before anything runs and is decided under `extensions.permissions.default` like every other capability. A local model uses the same declaration with a loopback host, such as Ollama on `localhost`. The manifest therefore shows whether your code is sent to a vendor or stays on the local machine.
- **Use the editor's rendering and extension APIs.** Inline completions would require ghost-text rendering; chat would use a side-bar or panel view. Both would receive data through brokered extension APIs such as `InlineCompletionItemProvider` and the chat participant API.

**Steps.**

1. Implement `chat.disableAIFeatures` and reject activation of extensions whose manifests declare AI features while it is enabled.
2. Ghost-text rendering in both frontends, as its own feature.
3. `InlineCompletionItemProvider` through the host shim, brokered like the
   rest.
4. A chat view in the side bar or panel, after the activation and completion APIs.

### Agent integration

**Agent integration** would allow a model to edit multiple files and invoke tools. It requires separate support for edit review, process execution, permissions and MCP (Model Context Protocol), beyond completion and chat APIs. External CLI agents could also run in the planned integrated terminal.

- **Check tool calls through the capability broker.** File access would use resolved path scopes; process and network access would require their own declarations and policy decisions. Agent integration also needs session-scoped grants, revocation and an audit trail. These controls and the currently unsupported operations must be implemented before enabling agent tools.
- **An agent's edits arrive as a `WorkspaceEdit`.** The multi-file, undoable edit used by rename is also the right unit for an agent's changes: applied atomically, reviewable as a diff before or after, and undone as **one step**, so `ctrl+z` recovers from a bad agent turn. This is the main reason `WorkspaceEdit` is treated as a foundation rather than a feature.
- **Add MCP server and tool permissions.** Initial support would target local JSON-RPC servers over stdio, with process supervision similar to `deco-lsp`. The proposed `mcp.json` integration would require permission to start each server and separate grants for its tools before offering them to the model.
- **External CLI agents are integrated through the terminal, not an API.** Users already run Claude Code and similar tools beside their editor. The integrated terminal would provide the initial integration: the agent runs there, and the editor detects what it changed. Files reload cleanly (this requires detecting external modification, which unsaved-conflict handling also needs), and the git gutter shows the agent's changes. Deeper integration, where the agent controls the editor, would use the same mediated API as extensions and nothing more.

**Steps (agents).**

1. Session-scoped grants and an activity log in the broker: the permission UX for high-volume callers, built before any agent uses it.
2. Agent edits as `WorkspaceEdit`s: atomic apply, one-step undo, a diff view
   of what a turn changed.
3. MCP: supervise `mcp.json` servers with the `deco-lsp` supervisor pattern,
   surface each tool as a grantable capability.
4. Terminal-first support for external CLI agents: external-change reload and
   gutter diffs are the integration.
5. Agent mode in the chat view, last, gated by `chat.agent.enabled` and
   inside `chat.disableAIFeatures` like everything else in this chapter.

## Debugging

**What VS Code has.** The Debug Adapter Protocol: breakpoints, stepping,
variables, the debug console, `launch.json`, `F5`.

**What deco has.** Nothing, and no near-term plan. This section records that status explicitly. DAP suits deco's architecture: it is a JSON protocol over stdio to a separate adapter process, structurally similar to `deco-lsp` and using the same framing, and `launch.json` already parses with the JSONC reader. The cost is the UI: breakpoints in the gutter, a stopped-state overlay, variables and watch views, and a debug console, each requiring its own editor view. Debugging is deliberately last. Every item above is smaller and more requested, and much of it is UI that debugging will build on.

**Steps, when it is begun.**

1. `deco-dap`: framing (shared with `deco-lsp`), lifecycle and capabilities, reusing the supervisor pattern for the third time.
2. Breakpoints as a session concept, gutter-rendered, sent on attach.
3. Launch/attach from `launch.json`; stop/continue/step with a stopped-line
   marker.
4. Variables and console views in the panel.

## Beyond VS Code

Everything above brings deco closer to VS Code. This section covers features that are practical *because* deco is not built on Electron, and that VS Code therefore cannot reasonably provide. The requirement for a section here is stricter: there must be a reason VS Code lacks the feature, not just that nobody has implemented it. The identifier rule also changes: where VS Code has no name for something, deco names it, and `deco.*` is the correct namespace.

### Several workspaces, switched between

**What VS Code does instead.** One folder per window. Multi-root workspaces (`.code-workspace`, `folders: []`) put several folders in *one* window, which is different: the folders share one settings resolution, one search and one set of language servers. Working on two projects requires two windows, and each window is an Electron process, so keeping several open has a significant memory cost. `workbench.action.switchWindow` is the closest equivalent.

**What deco has.** One root, fixed at launch. It is stored in `deco-tui::Driver::started_with`, derived from the file deco was started with, and never changes; the core has no concept of a workspace root. This is why `workbench.action.files.openFolder` is on the pending list: the file walk, search and language servers all depend on the root.

One part already exists: `deco-config`'s `Scope::Folder` is part of the settings layers, documented as *"a specific folder of a multi-root workspace"*, but nothing uses it yet.

**The plan.** A workspace consists of a root, the settings layers that root resolves (`Workspace` and `Folder` scope), and a set of tabs. Tabs are already stored as a zipper in the session, so a list of workspaces uses the same structure one level up. Switching replaces the tab set, re-resolves the settings, and re-roots quick open and search.

The root must first move into the session. It is currently held by a frontend, but switching must happen where the tabs, settings and context keys are. The same step implements `openFolder`, so both are one piece of work.

**The main cost is language servers, not tabs.** This is the reason the feature is practical in deco. VS Code is expensive per window because each window is a browser. deco's per-workspace cost is a set of LSP servers, which is significant but an order of magnitude smaller. By default, every workspace's **tabs** stay loaded, because they are buffers that were already open and keeping them adds no cost, and **servers run only for the active workspace**. Keeping servers running for several workspaces would be a setting with a documented cost.

**Identifiers.** `workbench.action.openRecent` keeps its name and meaning, as does the `.code-workspace` format if deco reads it. VS Code cannot *switch between workspaces loaded in the same process* and has no name for it, so this uses `deco.workspaces.*`. This is the one case where a new name is appropriate rather than a synonym.

**Steps.**

1. The root moves into `Session`, and `openFolder` changes it: re-walk, re-search,
   re-root the language servers. One root still, but a mutable one.
2. Several roots held at once, each with its own tabs and settings resolution;
   `deco.workspaces.switch` moves between them, and the status bar says which.
3. A workspace list in the side bar for selecting the active workspace.
4. Servers for the active workspace only, with a setting to keep servers for other workspaces running, and its cost documented.

## The gaps behind the features

Three smaller items block or limit the sections above and should be done first. They are listed in the README's "what is not built yet" and repeated here because the plans above depend on them.

- **~~`WorkspaceEdit`~~ — built.** A plan of per-document edits, validated
  against document versions before any write, applied all-or-nothing, and undone
  as one step; files no tab holds are opened rather than written. LSP rename
  (`F2`) is its first user and is documented in
  [Language servers](language-servers.md#rename), and
  [code actions](language-servers.md#code-actions) (`ctrl+.`) and
  [replace across the workspace](find-and-replace.md#replacing-across-the-workspace)
  (`ctrl+shift+h`) followed it. The mechanism is complete; the file tree's mutations are the remaining *caller*. An agent's turn has the same structure; see [Agent integration](#agent-integration).
- **~~Regular-expression search~~ — built.** `alt+r` switches the find bar and project search to the `regex` crate, which was already a dependency of `deco-keymap`, so no crate was added. Invalid patterns are reported, replacements expand capture groups, and remote search passes the option to the server. Look-around, backreferences and case-changing replacement references remain unsupported; see [Find and replace](find-and-replace.md#regular-expressions).
- **Full snippet support.** [Numeric completion fields are built](language-servers.md#snippet-tab-stops):
  Tab/Shift+Tab navigate, Escape exits, and ranges follow edits. Repeated indices,
  nested fields, choices, additional variables and transforms remain, followed by user snippet
  files. File-name, cursor-line, current-word and selected-text variables are supported in completions. Keep `snippetSupport: false` until the full LSP syntax is supported.
