# Extensions

A VS Code extension is arbitrary JavaScript running with your full privileges. It
can read `~/.ssh/id_ed25519`, open a socket and spawn a shell, and nothing in the
extension API makes that visible, let alone preventable. Installing one is
trusting its author — and every package in its `node_modules` — with everything you
can reach.

deco keeps the separate Node process, because extensions are JavaScript and there
is no way around that. It removes the ambient authority.

> **State of this:** a code extension's commands appear in the command palette, and
> choosing one starts its host in a container and runs it. What an extension can
> reach from there is still small — registering commands, saying something, logging
> — and everything else is refused by name. Theme and grammar extensions, which have
> no `main` and never start a host process, work fully.

## Running one

Press `ctrl+shift+p` and an extension's commands are in the list, beside deco's own.
The right-hand column is the *extension's* name rather than the command identifier
that deco's own commands show there: for something contributed from outside, which
extension it came from is the fact that decides whether you want it.

![The command palette listing a command contributed by an extension](img/extension-commands.svg)

Choosing one starts the host, activates the extension, and runs the command. The
first start in a container pulls the image, which can take a minute — the editor
does not block while it happens, the status bar says what is going on, and the
command you asked for runs when the host is ready rather than being forgotten.

Nothing starts on its own. `onLanguage:` and `onStartupFinished` are understood by
the catalogue and deliberately not acted on yet: while this is new, a process should
start only when you asked for something, because that is the version where a mistake
costs least. Opening a Rust file will not start three extensions.

An extension is started once and reused. If its host dies, the status bar says so.

## Four independent layers

**0. A container.** The host runs inside one by default, from an image named by
digest. This is the outermost layer and the newest; the three below it all live
*inside* the Node process, which left one assumption unpinned — the runtime
itself. See [The container](#the-container).

**1. Node's permission model.** The host runs with `--permission`, which blocks
filesystem, child-process and worker access below JavaScript, where an extension
cannot argue with it. No `--allow-child-process`, no `--allow-fs-write`. It is
passed inside the container too: a layer is not dropped because another one
arrived.

That flag is why the host needs **Node 22.13 or newer**: the permission model
became stable there and the flag lost its `--experimental-` prefix, and older
Node rejects the spelling deco passes. In the default container this is the
image's problem rather than yours — the pinned image carries Node 22.23 — and it
only becomes a requirement on your own machine if you turn the container off.

**2. The host bootstrap.** It removes the network globals and refuses to load
`fs`, `net`, `http`, `child_process` and friends, so a blocked call produces a
clear error naming its brokered replacement rather than an opaque permission trap.
Node's permission model does not cover the network; this layer is why that gap is
closed.

The module loader is part of the sandbox, which is why the host's own code
requires `node:path` and `node:module` with the prefix. An unprefixed `require`
can be shadowed by a `node_modules` package of that name, and the host has a test
asserting no bare `require` of a non-builtin survives anywhere in `src/`.

**3. The capability broker.** It checks every request that does get through.

## The container

Layers 1 to 3 are all enforced by the Node process or by deco. Layer 1 is the
only one an extension genuinely cannot argue with — and it is a feature of a
`node` binary deco borrows from whatever machine it is installed on. The version,
the build, and everything linked into it were outside deco's control.

A container closes that gap:

- **The runtime is pinned by digest**, the same way this project pins its CI
  actions. `docker.io/library/node:22-bookworm-slim@sha256:d649c27…` is a
  specific set of bytes, not a tag someone can move. deco **refuses an image
  reference that is not pinned**, including one you configure yourself.
- **`--network=none`** severs the network in the kernel. Layer 2 deletes `fetch`
  and refuses `net`, which produces a clear error — good manners, and manners are
  not a boundary. A native module has none.
- **`--read-only`**, one 16MB `noexec` `tmpfs`, and two read-only bind mounts:
  deco's own host code, and the single extension being run. `--cap-drop=ALL` and
  `--security-opt=no-new-privileges` leave nothing to escalate with. `--memory`
  and `--pids-limit` make a runaway extension the container's problem.

### The workspace is not mounted

This is the part that makes a container worth its cost here, so it is worth
stating plainly: **your project is not visible inside the container.** Extensions
read and write files through brokered requests that deco performs on their behalf,
so the container needs no view of the workspace at all.

Had the workspace been bind-mounted — as a dev container would — the container
would add very little. The files an extension actually wants are in there, and a
mount hands them over wholesale, with the broker bypassed for anything the
extension can reach through `fs`.

### If there is no container runtime

deco refuses to start the host, and says so, naming the setting that would let you
proceed. It does **not** fall back to running the host without a container. A
sandbox that silently degrades is worse than no sandbox, because nobody can tell
which one they have.

Neither does a workspace get to make that decision. `deco.extensions.sandbox`,
`deco.extensions.containerRuntime` and `deco.extensions.containerImage` are read
from deco's defaults and **your own** settings only. A `.vscode/settings.json`
arrives with a cloned repository, and a repository that could turn off the sandbox
that was about to contain its own extensions would make the sandbox decorative. An
attempt is reported rather than silently dropped.

### Turning it off

```jsonc
{
  // Runs the host as an ordinary child process, as deco did before containers.
  "deco.extensions.sandbox": "process"
}
```

This exists for telling a container problem apart from an extension problem. It
costs you layer 0 and pins nothing about the runtime, so `node` on your `PATH`
then has to be 22.13 or newer.

### What deco does not pass

`--user`. Under rootless Podman the container's root is already your own
unprivileged uid, and naming a uid there maps it into a subordinate range that
cannot read the bind mounts — so the flag would break the common case while adding
nothing to it.

An extension inside the container sees deco's own two variables, the five the Node
image sets in its own layers (`PATH`, `HOME`, `HOSTNAME`, `NODE_VERSION`,
`YARN_VERSION`), and — under Podman — `container=podman`, which OCI runtimes set
so software inside can tell where it is. Nothing from deco's environment crosses,
which is checked by starting a real container and asking the extension to report
what it can see.

## What the broker enforces

- **Deny by default.** A capability the manifest never declared is refused
  outright and never offered to you. Consent cannot be manufactured at request
  time by an extension that did not say up front what it wanted.
- **Declaration is a ceiling, not a grant.** A declared capability still needs a
  decision — remembered, prompted for, or refused by policy.
- **Scopes are checked on resolved paths**, so `workspace` access cannot be walked
  out of with `..`, and `/project-secrets` does not pass as a child of `/project`.

Capabilities: `readFile`, `writeFile`, `process`, `network`, `env`, `clipboard`,
`secrets`, `openExternal`. Path scopes: `workspace`, `extensionStorage`,
`extensionInstall`.

A refusal says which of these it was — undeclared, denied by you, denied by
policy, or outside the declared scope — because "permission denied" without the
reason is not something you can act on.

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

## The honest trade-off

An extension written for VS Code declares nothing, so under deco it starts with no
capabilities and will break wherever it reaches for the filesystem or the network.
deco does not guess a declaration on its behalf. The alternative to breaking it is
granting it everything silently, which is the thing this design exists to avoid.

## Starting one

`deco_ext::connection` is the layer between the command line and the protocol:
`Host::spawn` starts the process described by
[`build_spec`](#four-independent-layers), and one JSON object per line travels each
way.

Newline-delimited rather than the Language Server Protocol's `Content-Length` framing.
There is no specification to match here — both ends are deco's — and a newline is a
position a reader can resynchronise from, so an unreadable line costs one message
rather than the rest of the stream.

**The program must be an absolute path.** The host's environment is built from nothing,
so it carries no `PATH` for the operating system to search, and a bare `node` fails as
"no such file" — true, and no help to whoever configured it. `Host::spawn` refuses it
by name instead. That one was found by writing the round-trip test below and reading
the error it gave.

### `dispatch` is the only way in

Every inbound request goes through one function, and it is a pure function of the
broker and the request so that every path through it is testable without a process. It
fails closed twice:

- a method [`required_capability`] does not recognise is refused as unknown, so a host
  built from a newer deco cannot reach an older one's editor surface by naming
  something it has never heard of;
- a capability the manifest never declared is refused by the broker whatever the user
  has agreed to since — the declaration is a ceiling, not a starting point.

Registering a command, showing a message and appending to the log need no declaration
at all: they only touch state deco already owns and shows to the user. So the extension
in the round-trip test declares nothing and still works, which is the shape most
extensions should have.

### Tested against the real host, and against no host

The connection's own tests drive it over a `Cursor` or a channel, because the Rust
suite has to run where there is no Node — under Wine, for one.

`crates/deco-ext/tests/host_round_trip.rs` is the other half: it starts the real
`extension-host` with the real `node`, activates a real extension, and watches
`commands.registerCommand` arrive and pass the capability seam. It is `#[ignore]`d so
`cargo test` stays portable, and `cargo xtask host-test` runs it — the same command CI
runs in the one job that installs Node.

One of its tests asserts the environment of the **running process** rather than of
the spec: the extension reports every variable it can see, and anything but deco's own
two is a failure. An extension that could read `$GITHUB_TOKEN` would make every other
guard here moot, so it is worth checking against a process and not against a `BTreeMap`.

A third test does the same thing **inside the container**, against the pinned image.
It is the only check that the digest deco ships still resolves to a working Node, and
that everything else — the mounts, the translated paths, `--permission` with container
roots, the `vscode` shim — agrees once the filesystem is not the machine's own.
`cargo xtask host-test` selects it only when Podman or Docker is on the `PATH`, and
**prints which of the two it decided**: a test that quietly does not run looks exactly
like a test that passed.

That test is also where the claim above got more honest. It was written asserting the
container hands the extension nothing but deco's two variables, and it failed: an image
sets variables in its own layers, so `PATH`, `HOME`, `HOSTNAME`, `NODE_VERSION` and
`YARN_VERSION` are there too. None of them come from deco, which is the part that
matters — but the assertion now names the exact set rather than the set that would have
been tidier.

## Which extensions start, and when

`deco_ext::catalogue` is the decision: given what is installed and something that
happened, which extensions should be running. It is pure — the directory walk belongs
to the frontend, the same way finding themes does — and it holds three rules worth
stating.

**Only code extensions activate.** An extension with no `main` never starts a process
at all. A theme's `"activationEvents": ["*"]` fires for nothing, because there is
nothing to fire: the whole sandbox would be spent starting a process to read a JSON
file. That is why a marketplace theme works in deco today.

**A contributed command activates its extension, with or without `onCommand:`.** VS
Code stopped requiring the declaration in 1.74, and the reason to follow it is not
compatibility: a palette entry that does nothing is worse than either alternative, and
the trigger is the user naming the command. An empty `activationEvents` is still not a
wildcard — such an extension activates only through its own commands.

**An activation event deco does not understand fires for nothing.** Activation is a
security control before it is a performance one: an extension that has not activated
has no process, so no request it could make exists. Treating an unknown event as `*`
would turn every future VS Code event into a startup activation.

Collisions are reported rather than resolved silently. The same extension installed
twice keeps the first copy; a command contributed by two extensions stays with the
first, and the second is told so — otherwise the loser looks broken with the reason
visible nowhere.

## Running an extension's command

`commands.registerCommand` goes one way: the extension tells deco a name. Running it
goes the other, as `$/executeCommand`, and `Host::execute_command` is that call. The
reply carries whatever the extension's callback returned; a command the host does not
have is an error reply naming it, not a dropped connection.

This path existed in the `vscode` shim from the beginning and **nothing had ever asked
it to run**. It works — the round-trip test now activates the fixture, calls
`roundTrip.hello`, and asserts `"hello from the host"` comes back — and the shim's
command registry has tests of its own now, which it did not before: arguments in
order, async callbacks awaited, a `dispose()`d command no longer callable, a throwing
command reported without ending the session.

## What an extension can reach, today

Three things, all of which only touch state deco already owns and shows you:

| Call | What happens |
| --- | --- |
| `commands.registerCommand` | Recorded, so the command can be run |
| `window.showInformationMessage` and its warning, error and status-bar siblings | The message reaches the status bar |
| `log.append` | Kept in deco's own record of what extensions did |
| `fs.readFile` and `fs.writeFile` | The file, read or written where the session's files are — over the connection in a remote session |

**Everything else is refused by name.** An extension that asks to spawn a process
gets an error saying deco does not implement it yet — not a fake exit code, not an
empty list of open editors. An extension told "no" can cope; one told "here is your
empty answer" cannot, and neither can the person reading its behaviour.

A capability the manifest declared and nobody has ruled on **is asked about**. The
extension waits — its request is held rather than answered — and the question names
the extension and what it wants in words: *"Acme Tools wants to read files under
/home/u/project/notes.txt"*, not a Rust value. The answer is remembered for the
session, refusals included, because a refusal that is not remembered is a prompt
loop and a prompt loop is how someone ends up allowing something to make it stop.

Only one question is open at a time. A second extension asking while one is on
screen is refused, with that as the reason: a queue would mean answering about a
request that was abandoned long before anyone read it.

Decisions last for the session and are not written down yet, so restarting the
editor asks again.

`deco --print-config` prints which sandbox you would get, including the resolved
runtime and the pinned image — or the reason there is none, which is the state in
which extensions refuse to start.

## What is still not connected

The mediated surface is three calls wide, and the table above is the whole of it. No
consent prompt, so no capability that needs one can be granted. No activation on
opening a file or on startup. No `workspace.fs`, no editor state, no quick pick, no
tree views, no webviews, no debug adapters.

## Zero npm dependencies

The extension host has no `node_modules` at all, and a test in
`extension-host/test/dependencies.test.js` asserts it: `package.json` declares no
dependencies, and nothing under `src/` requires a non-builtin. A sandbox whose own
supply chain is unbounded is not a sandbox.
