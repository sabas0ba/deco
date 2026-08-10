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

## Zero npm dependencies

The extension host has no `node_modules` at all, and a test in
`extension-host/test/dependencies.test.js` asserts it: `package.json` declares no
dependencies, and nothing under `src/` requires a non-builtin. A sandbox whose own
supply chain is unbounded is not a sandbox.
