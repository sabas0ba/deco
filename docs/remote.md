# Remote development

> **State of this: you can open a file on another machine, edit it and save it
> back.** What is missing is everything around that — no provisioning (the binary
> has to already be on the remote), no port forwarding, no language servers or
> extensions over there, and no project-wide search. This page is explicit about
> each.

## Using it

```console
$ deco --remote ssh-remote+myhost --workspace /home/u/project src/main.rs
```

That starts `deco --server --stdio` on the far end over SSH, reads `src/main.rs`
through the connection, and opens it. `ctrl+s` writes it back over the same
connection. `ctrl+p` lists the **remote** workspace, because that is where the
files are.

Paths are relative to the workspace the server was given. Without `--workspace`
the server serves wherever the transport lands, which for SSH is the account's
home directory.

### What is different in a remote session

Three things are turned off rather than left to do the wrong thing, and each says
so rather than failing silently:

- **Language servers do not start.** They would be reading a local checkout that
  does not exist. deco cannot yet start one on the remote, so `F12` and its
  neighbours are unavailable and the reason is in the problem list at startup.
- **Search in files is refused.** The walk is local; in a remote session it would
  search this machine and report matches in files the editor is not showing.
- **Extensions are local.** A host started by a remote session runs here, in the
  same container sandbox as always, and reaches the remote's files not at all.

Saving is the one place where a failure is *not* fatal: a connection can drop
while the editor is perfectly able to keep your text and try again. A failed
remote save says so and leaves the document dirty, rather than reporting a save
that did not happen.

## Authorities

VS Code addresses a remote with an authority inside a `vscode-remote://` URI. deco
parses the same spellings:

| Authority | Means |
| --- | --- |
| `ssh-remote+myhost` | A `~/.ssh/config` alias, `user@host`, or a bare hostname |
| `ssh-remote+myhost:2222` | …with an explicit port |
| `wsl+Ubuntu` | A named WSL distribution |
| `wsl+` | The default WSL distribution |
| `dev-container+<id>` | A dev container built from the workspace |
| `attached-container+<id>` | An already-running container |

An unknown kind before the `+` is an error naming the kind, rather than a silent
fall back to local — connecting to the wrong machine is worse than refusing to
connect.

## Transports

Each authority knows the command that would reach it: `ssh`, `wsl.exe`, or
`docker exec`. The argument vector is built as a list and never as a shell string,
so a hostname or container id containing shell metacharacters is an argument and
not an instruction. This is the same rule the language-server launcher follows,
for the same reason.

## The wire protocol

Both ends would speak a length-prefixed framing over the transport's stdio — the
same shape as the Language Server Protocol's, and for the same reason: a stream
carrying both a program's output and a protocol's messages needs an unambiguous
boundary between frames.

The framing, the authority parsing and the command construction are implemented
and tested, and so is the far end that answers them.

## The server

```console
$ ssh myhost deco --server --stdio --workspace /home/u/project
```

That command is not written by hand — `deco_remote::server_command` builds it and
`command_for` wraps it in the transport — but it is exactly what runs, and a test
asserts that what one half builds is what the other half parses.

The server answers four methods: a handshake naming the protocol version and the
workspace, `fs.read`, `fs.write`, and `fs.list`. That is what opening, listing and
saving a file needs. It reads no settings, loads no theme, and starts no language
server or extension: a server deciding how to answer `fs.read` by reading a
`settings.json` on the remote would have an authority nobody asked it to have.

### One directory, and no way out of it

Every path is resolved and confined to the `--workspace` directory. Anything
outside it is refused **by name**, in both directions — reading and writing.

This is stricter than VS Code, whose remote server will open any path the account
can reach. The reason to be stricter is what the client is: whatever is on the
other end of a connection deco did not itself authenticate. A bug in the frontend,
a hijacked session, or a `deco-remote://` link someone else wrote should not be
able to ask for `~/.ssh/id_ed25519`.

Confinement is checked on the **canonical** path, so a symlink inside the
workspace pointing outside it is refused too — checking the path as written would
make `project/link-to-etc/passwd` legal, which is the exact shape of the mistake
this exists to prevent. A sibling directory whose name merely starts with the
same text (`project-secrets` against `project`) is outside, because the comparison
is on path components rather than on strings.

A file that is not valid UTF-8 is refused rather than repaired: deco would write
the replacement characters back on save, turning "deco opened my binary" into
"deco corrupted my binary".

## What a working version needs

Named so that the remaining work is legible rather than open-ended:

1. ~~`deco --server`, a headless session that answers frames.~~ **Done.**
2. ~~The client: opening a file through a transport, saving it back, and listing
   the remote workspace with `ctrl+p`.~~ **Done.**
3. Provisioning: getting the binary onto the remote and starting it, which means a
   decision about how much deco is willing to install on a machine you pointed it
   at. Today the binary has to be there.
4. Settings scope wiring: the `Remote` layer already exists between `User` and
   `Workspace` in the settings stack, so a remote's settings have somewhere to go.
5. Port forwarding, which the transports do not model at all.

Item 3 is the one with a security decision in it, not just work.
