# Extensions

A VS Code extension is arbitrary JavaScript running with your full privileges. It can read `~/.ssh/id_ed25519`, open a socket and spawn a shell, and the extension API neither shows nor prevents this. Installing an extension means trusting its author, and every package in its `node_modules`, with everything you can access.

deco runs code extensions in a separate Node process, inside a container by default. Direct filesystem and network access are restricted; supported operations are requested through deco's capability broker.

> **Supported features:** choosing an extension command from the palette starts its host and runs the command. Supported APIs include command registration, messages, logging, file operations and workspace edits, as listed [below](#what-an-extension-can-reach-today). Theme extensions do not require a host. TextMate grammar execution is not implemented; see [syntax highlighting](highlighting.md).

## Running one

Press `ctrl+shift+p` to see an extension's commands in the list alongside deco's own. For extension commands, the right-hand column shows the *extension's* name instead of the command identifier shown for deco's commands, because the source extension is the most relevant information for a contributed command.

![The command palette listing a command contributed by an extension](img/extension-commands.svg)

Choosing one starts the host, activates the extension, and runs the command. The first start in a container pulls the image, which can take a minute. The editor does not block during the pull, the status bar shows progress, and the requested command runs when the host is ready.

Extensions start only when a command is selected. The catalogue parses `onLanguage:` and `onStartupFinished`, but these events do not activate extensions yet.

An extension is started once and reused. If its host exits, the status bar reports it.

## Four independent layers

**0. A container.** By default the host runs inside a container, from an image identified by digest. This is the outermost and most recent layer. The three layers below it all run *inside* the Node process and therefore do not pin the runtime itself. See [The container](#the-container).

**1. Node's permission model.** The host runs with `--permission`, which blocks filesystem, child-process and worker access below the JavaScript level, where an extension cannot bypass it. `--allow-child-process` and `--allow-fs-write` are not passed. The flag is also passed inside the container; adding a layer does not remove another.

This flag requires **Node 22.13 or newer**. The permission model became stable in that version and the flag lost its `--experimental-` prefix, so older Node rejects the form deco passes. The pinned image for the default container provides Node 22.23, so the requirement applies to your own machine only if you disable the container.

**2. The host bootstrap.** It removes the network globals and refuses to load `fs`, `net`, `http`, `child_process` and similar modules, so a blocked call produces a clear error naming its brokered replacement instead of an opaque permission error. Node's permission model does not cover the network; this layer does.

The module loader is part of the sandbox, so the host's own code requires `node:path` and `node:module` with the prefix. An unprefixed `require` can be shadowed by a `node_modules` package of the same name. A host test asserts that no bare `require` of a non-builtin remains anywhere in `src/`.

**3. The capability broker.** It checks every request that does get through.

## The container

Layers 1 to 3 are enforced by Node or deco. In process mode, these checks depend on the locally installed Node runtime. Container mode also fixes the runtime image by digest and applies the isolation settings below.

Container mode provides the following:

- **The runtime is pinned by digest**, in the same way this project pins its CI actions. `docker.io/library/node:22-bookworm-slim@sha256:d649c27…` identifies specific image content, not a tag that can be moved. deco **refuses an image reference that is not pinned**, including one you configure yourself.
- **`--network=none`** disables external container networking at the operating-system level. This supplements the bootstrap's JavaScript-level restrictions on `fetch` and `net`.
- **`--read-only`**, one 16MB `noexec` `tmpfs`, and two read-only bind mounts: deco's own host code and the single extension being run. `--cap-drop=ALL` and `--security-opt=no-new-privileges` remove privilege escalation paths. `--memory` and `--pids-limit` confine a runaway extension to the container's resource limits.

### The workspace is not mounted

**The workspace is not mounted inside the container.** Extensions read and write workspace files through brokered requests performed by deco, subject to capability and path-scope checks.

If the workspace were bind-mounted, as in a dev container, the container would add little protection. The workspace contains the files an extension typically wants, and a mount would expose all of them, with the broker bypassed for anything the extension can access through `fs`.

### If there is no container runtime

deco refuses to start the host and reports the setting that allows you to proceed. It does **not** fall back to running the host without a container. A sandbox that degrades silently is worse than no sandbox, because the user cannot tell which one is active.

A workspace cannot make this decision either. `deco.extensions.sandbox`, `deco.extensions.containerRuntime` and `deco.extensions.containerImage` are read only from deco's defaults and **your own** settings. A `.vscode/settings.json` comes with a cloned repository and must not be able to disable isolation for its own extensions. Attempts to override these settings are reported.

### Turning it off

```jsonc
{
  // Runs the host as an ordinary child process, as deco did before containers.
  "deco.extensions.sandbox": "process"
}
```

This setting exists to distinguish a container problem from an extension problem. It removes layer 0 and does not pin the runtime, so `node` on your `PATH` must then be 22.13 or newer.

### What deco does not pass

`--user`. Under rootless Podman, the container's root is already your own unprivileged uid. Specifying a uid maps it into a subordinate range that cannot read the bind mounts, so the flag would break the common case without adding protection.

An extension inside the container sees deco's own two variables, the five variables the Node image sets in its layers (`PATH`, `HOME`, `HOSTNAME`, `NODE_VERSION`, `YARN_VERSION`), and, under Podman, `container=podman`, which OCI runtimes set so that software can detect the container. No variables from deco's environment are passed. A test checks this by starting a real container and having the extension report the variables it can see.

## What the broker enforces

- **Deny by default.** A capability the manifest does not declare is refused and never offered to you for approval. An extension cannot obtain consent at request time for a capability it did not declare in advance.
- **Declaration is a ceiling, not a grant.** A declared capability still needs a decision: a stored decision, a prompt, or a refusal by policy.
- **Scopes are checked on resolved paths**, so `..` cannot be used to leave `workspace` access, and `/project-secrets` is not treated as a child of `/project`.

Capabilities: `readFile`, `writeFile`, `process`, `network`, `env`, `clipboard`,
`secrets`, `openExternal`. Path scopes: `workspace`, `extensionStorage`,
`extensionInstall`.

A refusal states the reason: undeclared, denied by you, denied by policy, or outside the declared scope. A message without the reason would not tell you what to change.

## Declaring capabilities

In a `deco` section of `package.json`, which VS Code ignores:

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

`extensions.permissions.default` chooses what an undecided-but-declared capability
does:

| Value | Behaviour |
| --- | --- |
| `prompt` | Ask once, remember the answer (the default) |
| `deny` | Refuse silently — right for shared machines and CI |
| `allow` | The declaration becomes the only check |

<a id="the-honest-trade-off"></a>

## Compatibility limitations

VS Code extensions do not normally declare deco capabilities. Without those declarations, protected operations are rejected. Extension authors must declare the capabilities they need and use the supported brokered APIs; declaring a capability does not implement an unsupported API or grant permission by itself.

## Starting one

`deco_ext::connection` is the layer between the command line and the protocol. `Host::spawn` starts the process described by [`build_spec`](#four-independent-layers), and messages are exchanged as one JSON object per line in each direction.

The framing is newline-delimited, not the Language Server Protocol's `Content-Length` framing. No external specification applies because deco implements both ends. A reader can resynchronise at a newline, so an unreadable line loses one message instead of the rest of the stream.

**The program must be an absolute path.** The host's environment is built empty, so it has no `PATH` for the operating system to search, and a bare `node` fails with "no such file", which does not identify the cause. `Host::spawn` instead rejects it with an error that names the problem. This was found while writing the round-trip test described below.

### `dispatch` is the only way in

Every inbound request goes through one function. It is a pure function of the broker and the request, so every path through it can be tested without a process. It fails closed in two ways:

- a method that [`required_capabilities`] does not recognise is refused as unknown, so a host built from a newer deco cannot use functionality of an older editor by naming a method the editor does not know;
- a capability the manifest does not declare is refused by the broker regardless of later user approvals; the declaration is a ceiling, not a starting point.

Registering a command, showing a message and appending to the log need no declaration, because they only affect state that deco owns and shows to the user. The extension in the round-trip test therefore declares nothing and still works. Most extensions should follow this pattern.

### Tested against the real host, and against no host

The connection's own tests use a `Cursor` or a channel, because the Rust test suite must run where Node is not available, for example under Wine.

`crates/deco-ext/tests/host_round_trip.rs` covers the real host. It starts the real `extension-host` with the real `node`, activates a real extension, and checks that `commands.registerCommand` arrives and passes the capability check. It is marked `#[ignore]` so that `cargo test` stays portable, and `cargo xtask host-test` runs it. CI runs the same command in the one job that installs Node.

One test checks the environment of the **running process**, not of the spec. The extension reports every variable it can see, and any variable other than deco's two causes a failure. An extension that could read `$GITHUB_TOKEN` would defeat the other protections, so the check uses a real process instead of a `BTreeMap`.

A third test does the same **inside the container**, using the pinned image. It is the only check that the digest deco ships still provides a working Node, and that the mounts, translated paths, `--permission` with container roots and the `vscode` shim work when the filesystem is the container's. `cargo xtask host-test` selects it only when Podman or Docker is on the `PATH`, and **prints which one it selected**, because a skipped test would otherwise look like a passing one.

The container test checks the expected environment variables, including those supplied by the image and runtime. `PATH`, `HOME`, `HOSTNAME`, `NODE_VERSION` and `YARN_VERSION` are supplied by the container environment; their presence does not imply that deco forwarded its own environment.

## Which extensions start, and when

`deco_ext::catalogue` decides which extensions should be running, given the installed extensions and an event. It is pure: the directory walk is done by the frontend, as it is for themes. It applies three rules.

**Only code extensions activate.** An extension without `main` never starts a process. A theme's `"activationEvents": ["*"]` activates nothing, because there is no code to run; starting a sandboxed process only to read a JSON file would be wasteful. This is why marketplace themes work in deco.

**A contributed command activates its extension, with or without `onCommand:`.** VS Code stopped requiring the declaration in 1.74. deco follows this for a reason other than compatibility: a palette entry that does nothing is worse than either alternative, and the user selecting the command is the trigger. An empty `activationEvents` is still not a wildcard; such an extension activates only through its own commands.

**An activation event deco does not recognise activates nothing.** Activation is primarily a security control and secondarily a performance one: an extension that has not activated has no process and cannot make requests. Treating an unknown event as `*` would turn every future VS Code event into a startup activation.

Collisions are reported, not resolved silently. If the same extension is installed twice, the first copy is kept. If two extensions contribute the same command, the first keeps it and the second is notified; otherwise the second extension would appear broken with no visible reason.

## Running an extension's command

`commands.registerCommand` sends a command name from the extension to deco. Running the command goes in the other direction, as `$/executeCommand`, through `Host::execute_command`. The reply contains the value returned by the extension's callback. A command the host does not have produces an error reply naming it; the connection is not dropped.

This path existed in the `vscode` shim from the start but had **never been exercised**. It works: the round-trip test now activates the fixture, calls `roundTrip.hello`, and asserts that `"hello from the host"` is returned. The shim's command registry now also has its own tests, covering argument order, awaiting async callbacks, a `dispose()`d command no longer being callable, and a throwing command being reported without ending the session.

## What an extension can reach, today

Commands, messages, the filesystem and edits, all of which only affect state that deco owns and shows you:

| Call | What happens |
| --- | --- |
| `commands.registerCommand` | Recorded, so the command can be run |
| `window.showInformationMessage` and its warning, error and status-bar siblings | The message reaches the status bar |
| `log.append` | Kept in deco's own record of what extensions did |
| `fs.readFile` and `fs.writeFile` | The file, read or written where the session's files are — over the connection in a remote session |
| `fs.stat` and `fs.readDirectory` | What something is and what is directly in it, from the same place, in VS Code's own `FileStat` and `[name, type]` shapes |
| `fs.createDirectory`, `fs.delete`, `fs.rename`, `fs.copy` | The change, made where the session's files are |
| `workspace.applyEdit` | The edits, applied to the open document's buffer when it is open, and to the file when it is not |

**All other calls are refused with an error naming the call.** An extension that asks to spawn a process gets an error saying that deco does not implement it yet, not a fake exit code or an empty list of open editors. An extension can handle a refusal, but it cannot distinguish a placeholder empty result from a real one, and neither can a user investigating its behaviour.

A capability that the manifest declares and that has no stored decision **is prompted for**. The extension's request is held until the user answers. The prompt names the extension and describes the request in words, for example *"Acme Tools wants to read files under /home/u/project/notes.txt"*, not as a Rust value. The answer, including a refusal, is stored as described [below](#where-a-decision-lives). If refusals were not stored, the prompt would repeat, and a user might allow the request only to stop the prompts.

Only one prompt is open at a time. A request from a second extension while a prompt is shown is refused with that reason. A queue would ask about requests that may have been abandoned long before the user reads them.

A decision can be revoked. **Extensions: Forget a Permission Decision** in the command palette lists stored decisions, for example *"Acme Tools: refused — read files under /home/u/project/notes.txt"*. Choosing one makes that extension prompt again the next time it needs the capability. Without this command, a `deny` chosen by mistake would make the extension fail from then on, including in later sessions because decisions are stored, with no way to undo it and no indication that a decision is the cause.

### Where a decision lives

`permissions.json`, next to `settings.json`, with mode `0600` on Unix. The file contains no secrets. The mode protects against writes: anything that can write the file can grant capabilities to code running as you, so other accounts must not be able to edit it. The file is written after every answer instead of at shutdown, because an editor can be killed.

**A stored decision applies to one version of one extension.** Each entry records the extension version it was made for. After an update, the user is prompted again, and the prompt states the reason so that it does not look as if deco lost the decision. An extension allowed to read the workspace at 1.0.0 is different code at 1.1.0, so carrying the decision across versions would allow code the user has not reviewed. An update therefore requires a new prompt, by design.

A damaged permissions file is reported and treated as empty. Refusing to start the editor would be a worse failure, while being prompted again is recoverable.

`deco --print-config` shows which sandbox would be used, including the resolved runtime and the pinned image, or the reason no sandbox is available. In that case extensions do not start.

## What is still not connected

The table above lists all mediated APIs. There is no activation on file open or on startup. There is no editor state, quick pick, tree view, webview or debug adapter support. The `process`, `network`, `env`, `clipboard`, `secrets` and `openExternal` capabilities are declared and brokered, but are refused by name at the last step because nothing implements them.

### An edit goes through the editor, not past it

`workspace.applyEdit` takes a path and a list of LSP-shaped edits, and where they
land depends on whether that file is open:

- **Open** in any tab, not only the visible one: the edits go to its *buffer*. They become one undo step, the document becomes unsaved, and nothing is written. This matches VS Code. Writing to the file instead would be incorrect, because the next save of the document with unsaved changes would overwrite the edit.
- **Not open**: the file is read, edited and written through the same connection as other file operations.

Overlapping edits are rejected without changing the document because their result is not well-defined. Language-server edits use the same validation. An empty list succeeds, because an extension that computes no changes has not failed.

### What a write refuses

- **`useTrash` is refused, not ignored.** deco has no trash. If the option were ignored, an extension that requested a recoverable deletion would get an unrecoverable one without being told.
- **A non-empty directory requires `recursive`**, set by the caller. deco does not add it, because it distinguishes deleting one entry from deleting everything under it.
- **A rename or copy is checked at both paths.** `dispatch` allows the request only when every capability it needs is allowed. The target is a write. The source is a write for a rename, because moving a file out of a directory changes that directory, and a read for a copy. A denial at either path refuses the request without a prompt. When both paths need a decision, the target is asked about first, and after an allow the request stays held while the source is asked about.
- **A link is removed as a link.** Deletion resolves the path only to check its location, then removes the given name. Resolving the path and deleting the result would delete the link's target, leave the link, and report nothing. On the remote, a link that points outside the workspace cannot be used for deletion at all, because every path there is confined after canonicalisation, without exceptions.

A symbolic link is reported as a link, not as its target (65 for a link to a file, in VS Code's numbering). Following links would let a listing describe files outside the granted scope.

## Zero npm dependencies

The extension host has no `node_modules` at all, and a test in
`extension-host/test/dependencies.test.js` asserts it: `package.json` declares no
dependencies, and nothing under `src/` requires a non-builtin. This limits the third-party code used to enforce the sandbox.
