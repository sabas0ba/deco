# Roadmap

> **State of this: nothing on this page is built.** Every page in these docs
> describes something that works and proves it with an animation. This one is
> the opposite: a comparison of deco against VS Code, and a chapter per missing
> feature — what it would look like here, and the steps to build it. Each
> chapter is a plan, not a promise; when one ships, its chapter moves out of
> this page and into a real one with an animation of its own.

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
| Sidebar and panel — the chrome itself | nothing; everything below sits in it |
| File tree (the explorer) | the sidebar |
| Git — gutter diffs, branch in the status bar, stage and commit | the sidebar, and a status-bar segment |
| Integrated terminal | the panel, and a PTY dependency |
| Task runner (`tasks.json`, `ctrl+shift+b`) | the terminal, for somewhere to run |
| Test runner | the task runner, and later the extension host |
| Self-update | nothing — `cargo xtask dist` already builds what it would install |
| AI features — inline completions, chat, agent mode, MCP (`chat.disableAIFeatures`) | the extension host, ghost text, and the panel; agents additionally on `WorkspaceEdit` and the terminal; the off switch depends on nothing and comes first |
| Debugging (DAP) | the panel; by far the largest item here |
| Regular-expression search | nothing — `deco-core::search` is deliberately literal so far |
| Snippet tab stops | nothing |

The dependencies are why the order below is the order. Two foundations are left
— the **sidebar and panel**, and **finishing the extension-host wiring** — and
most of what users actually ask for (the tree, git, the terminal) is a tenant of
one of them. The third, **`WorkspaceEdit`**, is
[built](#the-gaps-behind-the-features): rename and code actions land through it,
and so will replace-across-files, the tree's mutations and an agent's turn.

Two rules carry over from everything already built, and every chapter below is
written under them:

- **VS Code's identifiers, exactly.** The commands are `git.stage` and
  `workbench.action.terminal.toggleTerminal`, the settings are `git.enabled`
  and `terminal.integrated.defaultProfile.linux` — never a `deco.*` synonym for
  something VS Code already has a name for. That is what makes an existing
  `keybindings.json` keep meaning what it meant.
- **A key that waits on a feature says so.** `ctrl+b` and `ctrl+j` are already
  bound and already name the sidebar and the panel as the features they are
  waiting on; a test over the default keymap keeps every such key honest. Each
  chapter shipping turns one of those refusals into behaviour.

## The sidebar and the panel

**What VS Code has.** A left sidebar hosting views (explorer, search, source
control, extensions), a bottom panel hosting others (terminal, problems,
output), and commands to toggle and focus them.

**What deco has.** The bindings — `ctrl+b` is
`workbench.action.toggleSidebarVisibility`, `ctrl+j` is
`workbench.action.togglePanel` — and nothing behind them. The TUI lays out one
editor region plus the status bar and prompt; there is no notion of a second
region with focus of its own.

**The plan.** A `Region` layer in `deco-editor`'s layout: the frontend hands
the session a rectangle, the session splits it between editor, sidebar and
panel, and each region renders through the same pure-function path the editor
does today — so the split is testable in CI with no terminal attached, which is
the property everything else here already has. Focus becomes part of the
session (`sideBarFocus`, `panelFocus` join the when-clause context keys), so
the keymap can route keys the way VS Code does. The GUI frontend draws the same
regions; it gets them for free once they exist below the frontends.

**Steps.**

1. Add regions and focus to `deco-editor::layout`, with the editor as the only
   tenant, and tests asserting the arithmetic of the split.
2. Wire `ctrl+b` / `ctrl+j` to toggle empty regions, replacing their
   named refusals; add the context keys to `deco-keymap`'s when engine.
3. Teach `deco-tui`'s renderer to paint a region border and a placeholder body;
   same for `deco-gui`.
4. Define the view contract — what a tenant must implement to be given a
   region — against which every later chapter builds.

## The file tree

**What VS Code has.** The explorer: a workspace tree with create, rename,
delete, and reveal; `workbench.files.action.focusFilesExplorer`,
`revealInExplorer`.

**What deco has.** `ctrl+p` (quick open) over the workspace file list, which is
already enumerated with `files.exclude` honoured — see `deco-tui::files`. No
tree.

**The plan.** The first sidebar tenant. The same file enumeration that feeds
quick open feeds the tree; directories expand lazily so a large workspace costs
what its *visible* rows cost, which is the bounded-by-the-window rule every hot
path here already follows. Enter opens in a tab; create, rename and delete come
once `WorkspaceEdit` exists, so that a rename of an open file and the retarget
of its tab are one undoable thing. On a remote workspace the tree lists the far
end through the existing `deco-remote` file listing, exactly as quick open
already does.

**Steps.**

1. A tree model over the existing walker: visible rows only, expansion state in
   the session.
2. Render as a sidebar view; keyboard first (VS Code's explorer keys), mouse
   later with the GUI's mouse work.
3. Open-on-enter, reveal-active-file; the `filesExplorerFocus` context key.
4. Mutations (new file, rename, delete) after `WorkspaceEdit` lands.

## Git

**What VS Code has.** A source-control view (stage, unstage, commit, discard),
gutter decorations for changed lines, the branch and dirty count in the status
bar, and a `git.*` command family. VS Code implements all of it by running the
`git` binary, not by linking a library.

**What deco has.** Nothing. `.git` appears in the code only as a directory to
skip in search and a marker for finding the workspace root.

**The plan.** Shell out to `git`, as VS Code does — it keeps the dependency
count where it is (the binary adds no crate), it inherits the user's hooks and
config, and a machine without git simply has the feature absent rather than a
copy of libgit2 nobody asked for. Three stages, each useful alone:

1. **Status bar**: branch name and a changed-file count, from `git status
   --porcelain=v2 --branch`, refreshed on save and on focus. No UI to build —
   the status bar exists.
2. **Gutter**: changed/added/deleted line markers, from `git diff` of the
   buffer against `HEAD`, computed the way everything else is — for the visible
   rows, resumed from the earliest edited line.
3. **The source-control view**: a sidebar tenant listing changed files;
   `git.stage`, `git.unstage`, `git.commit` (message through the existing
   prompt), `git.checkout` through the existing picker. `git.enabled` and
   `git.decorations.enabled` are read with their VS Code meanings.

Diffing an open buffer against `HEAD` means diffing *unsaved* text; the diff
runs against the buffer's content handed to `git diff --no-index` (or computed
in-process against the blob), never against the file on disk pretending to be
the buffer.

**Steps.**

1. A `deco-scm` crate: spawn `git`, parse porcelain v2, one supervisor per
   workspace modelled on `deco-lsp`'s server supervisor.
2. Status-bar segment; refresh triggers; a workspace without git, or a machine
   without the binary, shows nothing and logs why once.
3. Line diff for the visible window; gutter marks in both frontends.
4. The sidebar view and the `git.*` commands, last, because the first two are
   most of the daily value.

## The integrated terminal

**What VS Code has.** Terminals in the panel, ``ctrl+` `` to toggle, profiles,
and the `terminal.integrated.*` settings family.

**What deco has.** The bindings — ``ctrl+` `` is already
`workbench.action.terminal.toggleTerminal`, refusing by name — and nothing
behind them. And one honest complication the plan has to answer: deco's TUI
*is already inside* a terminal.

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

**The plan.** Two honest stages. First, tests are tasks: a `group.kind ==
"test"` task bound to VS Code's test-task command, run in the terminal — no new
UI, immediately useful. Second, a real testing view once the extension host can
carry it, because in VS Code the things that *discover* tests are extensions,
and deco's decision is to run those rather than to hardcode one runner per
language. That makes the full feature a tenant of the extension-host chapter
below, and the mediated API it needs (`tests.*`) an entry in the capability
table like `readFile` before it.

**Steps.**

1. Test-group tasks through the task runner.
2. The `vscode.tests` surface in the host shim, brokered like the rest.
3. A sidebar testing view rendering what extensions report; run through tasks,
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
exercise). Explicitly invoked, never in the background: an editor that phones
home unasked contradicts the way everything else here treats ambient authority,
so `update.mode` is read but only `none` and `manual` are honoured, and the
status bar may *say* a release exists only if a check was asked for. A
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

**What deco has.** Nothing — no AI code, no account, and nothing that phones
home. Which is not only a gap: for one of the two camps this chapter has to
serve, it is the feature, and the plan below is written so that building for
the other camp never takes it away.

**The plan.** Opinion genuinely splits here — some people want AI assistance in
the editor, and some want it provably absent, not merely hidden. deco can
serve both honestly because of two decisions already made: AI arrives as
**extensions**, never as a built-in, and an extension has no ambient authority.

- **The off switch is read first, and it means more here.**
  `chat.disableAIFeatures: true` — VS Code's own key — is honoured as a hard
  gate: no AI-declaring extension activates, no AI surface renders. And where
  VS Code's switch hides features that are still installed, deco's sits on top
  of the capability broker: an extension can only reach a model through a
  declared `network` capability, so with the gate closed there is no path to a
  model at all — *provably absent* rather than out of sight. With nothing AI
  shipped by default, a fresh deco already behaves as if the switch were on.
- **Using AI is an explicit grant, not a default.** An AI extension declares
  the host it talks to — `{"capability": "network", "host": "api.anthropic.com"}`
  — which is visible before anything runs and decided under
  `extensions.permissions.default` like every other capability. A local model
  is the same declaration with a loopback host (an Ollama on `localhost`), and
  the difference between "my code goes to a vendor" and "my code stays on this
  machine" is readable off the manifest instead of taken on faith.
- **The surfaces are the ordinary ones.** Inline completions are ghost text —
  a rendering concern worth building once, since parameter hints and inlay
  hints want it too. Chat is a sidebar or panel tenant like the views before
  it. Both are fed through the host's mediated API
  (`InlineCompletionItemProvider`, the chat participant surface), so an AI
  extension is not a special kind of extension — it is an extension with an
  unusually interesting `network` declaration.

**Steps.**

1. Read `chat.disableAIFeatures` and enforce it in the broker — a gate an
   extension cannot argue with, not a UI preference; refuse activation of
   extensions whose manifest declares AI surfaces while it is set.
2. Ghost-text rendering in both frontends, as its own feature.
3. `InlineCompletionItemProvider` through the host shim, brokered like the
   rest.
4. A chat view as a sidebar/panel tenant, last — it is the largest surface and
   the least of the daily value.

### Agent integration

Completions and chat are the small half of what "AI features" now means. The
larger half is **agents**: a model that plans, edits several files, runs
commands and iterates — VS Code's agent mode (`chat.agent.enabled`), its MCP
support (Model Context Protocol servers offering tools to the model), and the
external CLI agents people run beside their editor. This is where deco's
architecture stops being a constraint on AI and starts being the point, so it
is planned as its own stage rather than left implied by "chat".

- **An agent is the capability model's hardest customer, and its best
  argument.** In VS Code an agent's tool calls run with the user's full
  privileges and safety is a per-call confirmation dialog. Under deco every
  tool an agent reaches for is already a brokered capability: file access is
  scoped and checked on resolved paths, running a program is a declared
  capability decided by policy, and the network is a named host. Nothing new
  has to be invented to make an agent safe — the broker does not care whether
  a `writeFile` was asked for by a keystroke or by a model. What agents add
  is *volume*, which the permission UX must absorb: session-scoped grants
  ("this agent may edit `src/` until this chat ends") rather than a dialog
  per call, and an audit trail of what was touched.
- **An agent's edits arrive as a `WorkspaceEdit`.** The multi-file, undoable
  edit that rename needs is exactly the unit an agent's changes should land
  as: applied atomically, reviewable as a diff before or after, and undone as
  **one step** — `ctrl+z` as the recovery from a bad agent turn. This is the
  strongest reason `WorkspaceEdit` is a foundation and not a feature.
- **MCP fits the broker better than it fits VS Code.** An MCP server is a
  local process speaking JSON-RPC over stdio — structurally what `deco-lsp`
  already supervises. `mcp.json` names the servers; starting one is a
  `process` capability, and each *tool* the server offers becomes a named,
  individually grantable capability rather than a blanket "the model may use
  tools". Deny-by-default then means a tool the user never approved is never
  offered to the model at all.
- **External CLI agents come through the terminal, not an API.** People
  already run Claude Code and its peers beside their editor. The integrated
  terminal is the honest first integration: the agent runs there, and the
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
a debug console — each a tenant of chrome that must exist first. It is
deliberately last: everything above it is smaller, more asked-for, and most of
it is chrome that debugging will then stand on.

**Steps, when it is begun.**

1. `deco-dap`: framing (shared with `deco-lsp`), lifecycle, capabilities —
   the supervisor pattern a third time.
2. Breakpoints as a session concept, gutter-rendered, sent on attach.
3. Launch/attach from `launch.json`; stop/continue/step with a stopped-line
   marker.
4. Variables and console as panel tenants.

## The gaps behind the features

Three smaller items block or shrink the chapters above and are worth doing
first; they are listed in the README's "what is not built yet" and repeated
here because the plans above lean on them.

- **~~`WorkspaceEdit`~~ — built.** A plan of per-document edits, validated
  against document versions before any write, applied all-or-nothing, and undone
  as one step; files no tab holds are opened rather than written. LSP rename
  (`F2`) is its first user and is documented in
  [Language servers](language-servers.md#rename), and
  [code actions](language-servers.md#code-actions) (`ctrl+.`) followed it. What
  still waits on a *caller* rather than on the mechanism: replace-across-files,
  and the file tree's mutations. An agent's turn is the same shape — see
  [Agent integration](#agent-integration).
- **Regular-expression search.** `deco-core::search` is literal on purpose;
  regex needs its own escaping rules and its own error reporting for a bad
  pattern (`alt+r` says so today). Decision to make: a regex crate dependency
  versus the subset a hand-written engine can honestly support. The find bar,
  multi-cursor find and search-in-files all inherit whichever lands.
- **Snippet tab stops.** deco advertises `snippetSupport: false` and flattens
  `foo(${1:arg})` to `foo(arg)`, saying so in the status bar. Tab stops are a
  session concept (ordered regions, `tab` cycles, `escape` leaves) and unlock
  both server completions and, later, user snippets.
