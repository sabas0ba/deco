# Extensions

A VS Code extension is arbitrary JavaScript running with your full privileges. It
can read `~/.ssh/id_ed25519`, open a socket and spawn a shell, and nothing in the
extension API makes that visible, let alone preventable. Installing one is
trusting its author — and every package in its `node_modules` — with everything you
can reach.

deco keeps the separate Node process, because extensions are JavaScript and there
is no way around that. It removes the ambient authority.

> **State of this:** the protocol, the capability broker, the sandbox and the
> `vscode` shim all exist and are tested against each other. The editor does not
> yet start a host or dispatch to one. Theme and grammar extensions, which have no
> `main` and never start a host process, work today.

## Three independent layers

**1. Node's permission model.** The host runs with `--permission` (Node 20+),
which blocks filesystem, child-process and worker access below JavaScript, where
an extension cannot argue with it. No `--allow-child-process`, no
`--allow-fs-write`.

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
[`build_spec`](#three-independent-layers), and one JSON object per line travels each
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

One of its two tests asserts the environment of the **running process** rather than of
the spec: the extension reports every variable it can see, and anything but deco's own
two is a failure. An extension that could read `$GITHUB_TOKEN` would make every other
guard here moot, so it is worth checking against a process and not against a `BTreeMap`.

## What is still not connected

The editor does not start a host yet. This is the wire, tested end to end; what comes
next is the editor side of it — deciding which extensions to activate from their
`activationEvents`, putting their commands in the palette, and answering the mediated
surface from the session.

## Zero npm dependencies

The extension host has no `node_modules` at all, and a test in
`extension-host/test/dependencies.test.js` asserts it: `package.json` declares no
dependencies, and nothing under `src/` requires a non-builtin. A sandbox whose own
supply chain is unbounded is not a sandbox.
