# Roadmap

> **Status:** this page describes planned features, their prerequisites and proposed implementation steps. Sections also identify completed prerequisites and link to their documentation. No delivery dates are specified.

Most of the page is deco catching up with VS Code. The last part,
[Beyond VS Code](#beyond-vs-code), is not: it is what is worth building
*because* deco is not Electron.

## Where deco stands against VS Code

What already works is documented, with animations, on the other pages of this
site; the [top-level README](https://github.com/sabas0ba/deco#readme) keeps the
authoritative compatibility table. In one line: configuration, keybindings and
themes are read with VS Code's meaning; editing, tabs, splits, find and
replace, quick open, the command palette, workspace-wide search, syntax
highlighting and the listed language-server features work; extensions run in a
sandboxed host; remote sessions can open, edit and save.

What VS Code has and deco does not, grouped by how it blocks:

| Missing | Depends on |
| --- | --- |
| Git — discard, push/pull ([the rest is built](git.md)) | nothing; each is its own decision about what it may lose or ask for |
| Integrated terminal | a PTY dependency; its home is the [panel](chrome.md) |
| Task runner (`tasks.json`, `ctrl+shift+b`) | the terminal, for somewhere to run |
| Test runner | the task runner, and later the extension host |
| Self-update | nothing — `cargo xtask dist` already builds what it would install |
| AI features — inline completions, chat, agent mode, MCP (`chat.disableAIFeatures`) | the extension host, ghost text, and the panel; agents additionally on `WorkspaceEdit` and the terminal; the off switch depends on nothing and comes first |
| Debugging (DAP) | the panel; by far the largest item here |
| Regular-expression search | nothing — `deco-core::search` is deliberately literal so far |
| Full snippet syntax and user snippet files ([numeric completion fields work](language-servers.md#snippet-tab-stops)) | linked/nested field tracking and variable/transform expansion |

The dependencies are why the order below is the order. One foundation is left —
**implementing the remaining extension-host APIs**. The other two are built. The
**[side bar and panel](chrome.md)** provide regions for the file tree, source-control view and planned terminal; **`WorkspaceEdit`**
([below](#the-gaps-behind-the-features)) is what rename, code actions and
replace across the workspace already land through, and what the tree's mutations
and an agent's turn will.

Two rules carry over from everything already built, and every chapter below is
written under them:

- **VS Code's identifiers, exactly.** The commands are `git.stage` and
  `workbench.action.terminal.toggleTerminal`, the settings are `git.enabled`
  and `terminal.integrated.defaultProfile.linux` — never a `deco.*` synonym for
  something VS Code already has a name for. That is what makes an existing
  `keybindings.json` keep meaning what it meant.
- **A key that waits on a feature says so.** `` ctrl+` `` is bound and names the
  integrated terminal as what it is waiting on; a test over the default keymap
  checks that unimplemented commands report an error. Once the feature is implemented, the command performs the operation; for example, `ctrl+b` and `ctrl+j` now toggle the side bar and panel.

## Git

**Built.** The branch and what differs from it in the status bar, marks beside
the changed lines, a source-control view that stages, unstages and commits, and
a local-branch picker with checkout preflight. It has [a page of its own](git.md);
this chapter is what is left over.

**What VS Code has that deco still does not.**

- **Discard.** `git clean` and `git checkout --` throw away work with no undo
  and no trash. deco's tree refuses to delete quietly for the same reason, and
  this would need the same kind of answer.
- **Push, pull, fetch.** These need credentials, and a credential prompt is a
  thing an editor has to be trusted with. Worth doing deliberately rather than
  as an afterthought to a view that already works.
**Steps.**

1. Discard changes, with confirmation that identifies affected files and explains whether recovery is possible.
2. Push, pull and fetch, after credential prompting is implemented.

## The integrated terminal

**What VS Code has.** Terminals in the panel, ``ctrl+` `` to toggle, profiles,
and the `terminal.integrated.*` settings family.

**What deco has.** The [panel](chrome.md) to put one in, and the binding —
``ctrl+` `` is already `workbench.action.terminal.toggleTerminal`, refusing by
name. The terminal implementation must handle nested terminal input and output because deco's TUI runs inside an existing terminal.

**The plan.** A PTY per terminal, a VT parser feeding a screen model, and the
panel region painting that model — through the same pure render path, so a
terminal's screen is assertable in CI like any other layout. This is the one
chapter that adds a real dependency (a PTY crate, and possibly a VT parser);
the README counts its 44 crates in public, so the addition is made once, named
in the dependency table, and justified by this feature alone. In the GUI the
same model paints into the window; in the TUI the inner terminal's cells map
onto the panel's cells, colours through the existing theme layer. In a remote
session the shell belongs on the machine holding the files: the server side
grows a PTY endpoint over the existing `deco-remote` framing, the way language
servers already run over there.

**Steps.**

1. Choose and justify the PTY dependency; spawn the user's shell
   (`terminal.integrated.defaultProfile.*`), pump its output.
2. A screen model: grid, cursor, scrollback, SGR colours mapped through
   `deco-theme`.
3. Panel view, focus routing — keys go to the shell when the terminal has
   focus, except the escape hatch VS Code also keeps (`ctrl+j`, ``ctrl+` ``).
4. Remote PTYs over the session connection.

## The task runner

**What VS Code has.** `tasks.json` (build tasks, `ctrl+shift+b` =
`workbench.action.tasks.build`, `workbench.action.tasks.runTask` from the
palette), problem matchers that turn output into diagnostics.

**What deco has.** Nothing, including the bindings.

**The plan.** Read `.vscode/tasks.json` — the file users already have — through
`deco-config`'s existing JSONC reader. A task runs in the integrated terminal
(which is why this chapter follows that one) and its exit status lands in the
status bar. Problem matchers come second: the named matchers (`$rustc`, `$tsc`)
map output lines to the same diagnostics pipeline LSP already fills, so a
compile error from a task underlines code exactly as a language server's would.

**Steps.**

1. Parse `tasks.json` (`shell` and `process` types, `group.kind == "build"`);
   surface tasks in the palette under their VS Code command names.
2. Run in a terminal; report exit status.
3. The common problem matchers, into `deco-lsp`'s diagnostics store.
4. `${workspaceFolder}` and friends — the variable set, resolved against the
   session.

## The test runner

**What VS Code has.** A testing view fed by extensions through the
`vscode.tests` API; run/debug at the test, file and suite level; results inline
in the gutter.

**What deco has.** Nothing. (deco's *own* test suite is documented in
[Testing](testing.md); this chapter is about running *your* tests.)

**The plan.** Two stages. First, tests are tasks: a `group.kind ==
"test"` task bound to VS Code's test-task command, run in the terminal — no new
UI, immediately useful. Second, a real testing view once the extension host can
carry it, because in VS Code the things that *discover* tests are extensions,
and deco's decision is to run those rather than to hardcode one runner per
language. The full feature therefore depends on the extension-host work described
below, and the mediated API it needs (`tests.*`) an entry in the capability
table like `readFile` before it.

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

**The plan.** `deco --update`: ask GitHub Releases for the latest tag, compare
versions, download the archive for this target, verify the checksum, replace
the running binary atomically (write beside, rename over — with the Windows
rename dance, which is exactly what `cargo xtask cross`'s Wine run exists to
exercise). Update checks would run only on explicit request. The proposed implementation would support `update.mode` values `none` and `manual`, and display release information only after a requested check. A
package-manager install (where the binary is not the user's to replace) is
detected and refused with the right command named instead.

**Steps.**

1. Version check against the Releases API; `--update --check-only` prints and
   exits.
2. Download, verify against the published `.sha256`, stage beside the binary.
3. Atomic replace, per platform, with tests where tests can run (Wine covers
   the Windows path daily).
4. The read-only-install detection and its refusal message.

## AI features

**What VS Code has.** Copilot woven into the core rather than shipped beside
it: inline completions as ghost text, a chat view, an agent mode that edits
files — and, for the people who want none of that, `chat.disableAIFeatures`,
the one setting that hides all of it.

**What deco has.** No AI integration is implemented. The proposed features would be optional and require an extension.

**The plan.** Provide AI features through **extensions** using capability checks for supported file, process and network operations. The required extension APIs and policy controls described below still need implementation.

- **Disable declared AI integrations before activation.** The proposed `chat.disableAIFeatures: true` setting would block extensions that declare AI features and hide their UI. This requires a manifest declaration and activation checks; it would not identify undeclared AI use by arbitrary extension code.
- **Using AI is an explicit grant, not a default.** An AI extension declares
  the host it talks to — `{"capability": "network", "host": "api.anthropic.com"}`
  — which is visible before anything runs and decided under
  `extensions.permissions.default` like every other capability. A local model
  is the same declaration with a loopback host (an Ollama on `localhost`), and
  the difference between "my code goes to a vendor" and "my code stays on this
  machine" is readable off the manifest instead of taken on faith.
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
- **An agent's edits arrive as a `WorkspaceEdit`.** The multi-file, undoable
  edit that rename needs is exactly the unit an agent's changes should land
  as: applied atomically, reviewable as a diff before or after, and undone as
  **one step** — `ctrl+z` as the recovery from a bad agent turn. This is the
  strongest reason `WorkspaceEdit` is a foundation and not a feature.
- **Add MCP server and tool permissions.** Initial support would target local JSON-RPC servers over stdio, with process supervision similar to `deco-lsp`. The proposed `mcp.json` integration would require permission to start each server and separate grants for its tools before offering them to the model.
- **External CLI agents come through the terminal, not an API.** People
  already run Claude Code and its peers beside their editor. The integrated
  terminal would provide the initial integration: the agent runs there, and the
  editor's job is to notice what it changed — files reloading cleanly (watch
  for external modification, which unsaved-conflict handling needs anyway)
  and the git gutter showing the agent's diff. Deeper integration — the
  agent driving the editor — is the same mediated surface extensions get, and
  nothing more.

**Steps (agents).**

1. Session-scoped grants and an activity log in the broker — the permission
   UX for high-volume callers, built before any agent uses it.
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

**What deco has.** Nothing, and no near-term plan — this chapter exists so the
answer is written down rather than implied. DAP is the right shape for deco
(a JSON protocol over stdio to a separate adapter process — structurally a
sibling of `deco-lsp`, whose framing it literally shares), and `launch.json`
already parses with the JSONC reader. But the *surface* is the cost:
breakpoints in the gutter, a stopped-state overlay, variables and watch views,
a debug console, each requiring its own editor view. It is
deliberately last: everything above it is smaller, more asked-for, and most of
it is chrome that debugging will then stand on.

**Steps, when it is begun.**

1. `deco-dap`: framing (shared with `deco-lsp`), lifecycle, capabilities —
   the supervisor pattern a third time.
2. Breakpoints as a session concept, gutter-rendered, sent on attach.
3. Launch/attach from `launch.json`; stop/continue/step with a stopped-line
   marker.
4. Variables and console views in the panel.

## Beyond VS Code

Everything above is deco catching up. This section is the other direction: what
is worth building *because* deco is not Electron, and which VS Code therefore
cannot reasonably do. The bar for a chapter here is higher than for one above —
a feature VS Code lacks needs a reason it lacks it, or it is just a feature
nobody got round to — and the rule about identifiers changes shape: where VS
Code has no name for something, deco names it, and `deco.*` is the right
namespace rather than a synonym to avoid.

### Several workspaces, switched between

**What VS Code does instead.** One folder per window. Multi-root workspaces
(`.code-workspace`, `folders: []`) put several folders in *one* window, which is
a different thing: they share one settings resolution, one search, one set of
language servers. Working on two projects means two windows, and a window is an
Electron process — the reason having several open is a decision about memory
rather than a keystroke. `workbench.action.switchWindow` is as close as it gets.

**What deco has.** One root, fixed at launch. It lives in
`deco-tui::Driver::started_with`, is derived from the file deco was started
with, and never changes; the core has no concept of a workspace root at all.
That is exactly why `workbench.action.files.openFolder` is on the pending list —
the file walk, the search and the language servers are all anchored to it.

One piece is already in place: `deco-config`'s `Scope::Folder` exists in the
settings layering, documented as *"a specific folder of a multi-root
workspace"*, and nothing uses it yet.

**The plan.** A workspace is a root, the settings layers that root resolves
(`Workspace` and `Folder` scope), and a set of tabs. Tabs are already a zipper
inside the session, so a list of workspaces is the same shape one level up:
switching swaps the tab set, re-resolves the settings, and re-anchors quick open
and search.

The root has to move into the session first. It is in a frontend today, and
switching has to happen where the tabs, the settings and the context keys are.
That step is also where `openFolder` lands, so the two are one piece of work.

**What it costs is language servers, not tabs.** This is the whole argument for
doing it here. VS Code is heavy per window because a window is a browser; deco's
per-workspace cost is a set of LSP servers, which is real but an order of
magnitude smaller. So the default is: keep every workspace's **tabs** live —
they are buffers that were already open, and holding them costs what they
already cost — and supervise **servers for the active workspace only**. Keeping
several warm is then a setting whose price is stated rather than discovered.

**Identifiers.** `workbench.action.openRecent` keeps its name and its meaning,
as does the `.code-workspace` format if deco reads one. But *switching between
workspaces already loaded in this process* is something VS Code cannot do and
therefore has no name for, so that is `deco.workspaces.*` — the one case where
inventing a name is right rather than a synonym for someone else's.

**Steps.**

1. The root moves into `Session`, and `openFolder` changes it: re-walk, re-search,
   re-root the language servers. One root still, but a mutable one.
2. Several roots held at once, each with its own tabs and settings resolution;
   `deco.workspaces.switch` moves between them, and the status bar says which.
3. A workspace list in the side bar for selecting the active workspace.
4. Servers per active workspace, with the warm-set setting and its cost written
   down.

## The gaps behind the features

Three smaller items block or shrink the chapters above and are worth doing
first; they are listed in the README's "what is not built yet" and repeated
here because the plans above lean on them.

- **~~`WorkspaceEdit`~~ — built.** A plan of per-document edits, validated
  against document versions before any write, applied all-or-nothing, and undone
  as one step; files no tab holds are opened rather than written. LSP rename
  (`F2`) is its first user and is documented in
  [Language servers](language-servers.md#rename), and
  [code actions](language-servers.md#code-actions) (`ctrl+.`) and
  [replace across the workspace](find-and-replace.md#replacing-across-the-workspace)
  (`ctrl+shift+h`) followed it. What still waits on a *caller* rather than on the
  mechanism: the file tree's mutations. An agent's turn is the same shape — see
  [Agent integration](#agent-integration).
- **Regular-expression search.** `deco-core::search` is literal on purpose;
  regex needs its own escaping rules and its own error reporting for a bad
  pattern (`alt+r` says so today). Decision to make: a regex crate dependency
  versus the subset supported by a custom engine. The find bar,
  multi-cursor find and search-in-files all inherit whichever lands.
- **Full snippet support.** [Numeric completion fields are built](language-servers.md#snippet-tab-stops):
  Tab/Shift+Tab navigate, Escape exits, and ranges follow edits. Repeated indices,
  nested fields, choices, additional variables and transforms remain, followed by user snippet
  files. File-name, cursor-line, current-word and selected-text variables are supported in completions. Keep `snippetSupport: false` until the full LSP syntax is supported.
