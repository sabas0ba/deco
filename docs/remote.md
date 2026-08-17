# Remote development

> **State of this: you can open a file on another machine, edit it and save it
> back, put deco there if it has none, reach a port on it, and run language
> servers over there, and search it.** What is missing is extensions on the
> remote. This page is explicit about each.

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

One thing is turned off rather than left to do the wrong thing, and it says so
rather than failing silently:

- **Extension hosts run here, and their file access goes over the connection.**
  A host started by a remote session is still a local Node process in the same
  container sandbox as always — but `vscode.workspace.fs.readFile` and its
  neighbours are answered through the session's connection, so an extension reads
  the files being edited rather than whatever is at that path on this machine.
  See below.

Saving is the one place where a failure is *not* fatal: a connection can drop
while the editor is perfectly able to keep your text and try again. A failed
remote save says so and leaves the document dirty, rather than reporting a save
that did not happen.

## Getting deco onto the remote

The far end needs a `deco` to run. If it has one on its PATH, nothing below is
needed; if it is installed somewhere a login shell does not look, name it:

```console
$ deco --remote ssh-remote+myhost --remote-server-path ~/.deco/bin/deco src/main.rs
```

And if it has none at all, deco can send this machine's own binary — **when you
ask it to**:

```console
$ deco --remote ssh-remote+myhost --remote-install src/main.rs
```

It lands in `$HOME/.deco/bin/deco` on the remote, under the account's own home
rather than anywhere on the system path: installing for one user needs no
privileges and affects nobody else, which is the least surprising thing an editor
can do to a machine. `--remote-server-path` chooses somewhere else.

### What it will not do

Pointing an editor at a machine is not the same as authorising it to install
software there. That is the whole design of this part, and it is why the flag
exists rather than the behaviour being automatic:

- **Nothing happens unless asked.** A session that finds no `deco` fails and
  mentions `--remote-install`. It does not helpfully fix it.
- **A different platform is refused.** The remote is asked what it is *before*
  anything is sent, and a mismatch names both sides rather than uploading a
  binary that cannot run there.
- **Anything that is not deco is left alone.** Existence and runnability are
  asked separately, so a `--remote-server-path` typo landing on a `notes.txt` —
  which exists but answers no `--version` — is a refusal, not an overwrite.
- **A half-written binary never reaches the destination.** The upload goes to
  `deco.incoming` beside it and is renamed once complete, so an interrupted
  install leaves the old deco, or nothing.
- **The result is checked.** The installed binary is asked for its version, so a
  `noexec` mount or a missing libc is an error here rather than a handshake that
  never answers.

A deco of the same version already at the destination is left where it is, so
this is not an upload on every start.

### What it cannot do yet

It sends *this* machine's binary, which means both ends must be the same
platform: Linux to Linux, and every WSL and container case. **A macOS laptop
provisioning a Linux server is refused**, because the fix is fetching a release
built for the remote rather than sending the wrong file — and where deco is
willing to download from is its own decision, not one to slip in here.

The remote is assumed to have a POSIX shell and `uname`, `mkdir`, `dd`, `chmod`
and `mv` — the same assumption already made by running `deco --server` over
`ssh`.

## Extensions

The host stays on this machine. What changes in a remote session is where its
file requests are served from: `readFile` and `writeFile` go through the same
connection the editor uses, so an extension sees the workspace being edited.

Reading around the connection is the failure this is shaped to prevent. The path
an extension asks for exists on the remote; a local read at the same path would
answer from a different checkout, or from nothing, and the reply would look
identical either way. So the server's rules apply to an extension too — a path
outside the workspace is refused by name, exactly as it is for the editor.

Two consequences worth stating:

- **`process` does not follow the files.** An extension granted permission to run
  `eslint` runs it *here*, where the project is not. Nothing else could be done
  without a host on the remote, and a linter pointed at files that are not there
  produces wrong answers rather than an error — so this is the one capability
  where a remote session is honestly worse than a local one.
- **Clipboard, secrets and `openExternal` stay local, and that is right**: they
  belong where the person is, not where the files are.

A host *on* the remote is the other design, and it is a different decision rather
than more of this one: it needs Node over there, which deco does not provision,
and it moves the capability broker — the thing standing between a cloned
repository's extension and your machine — onto a machine you may share. What is
here does not have to be undone to get there; VS Code's remote support has both
kinds, and its UI-side extensions reach workspace files exactly this way.

### Permissions

`extensions.permissions.default` decides what happens to a capability the
manifest declared and nobody has ruled on. It defaults to `prompt`, and there is
nowhere to prompt yet, so a declared capability is still refused — with the reason
said rather than looking like an extension that does not work. Setting it to
`allow` serves declared capabilities without asking, which is the deliberate
downgrade the setting describes: declaration becomes the only check.

Until this change deco passed `deny` regardless of that setting, so writing
`allow` did nothing and did not say so.

## Language servers

They run on the machine holding the files, which is the only place that could
work: a server started here would be indexing a checkout that does not exist.

Nothing needs configuring for this. The same `deco.lsp.servers` definitions are
used, with each command wrapped in the transport — `rust-analyzer` in your
settings becomes `ssh myhost rust-analyzer` — so the server has to be installed
on the remote rather than here. deco does not provision language servers; the
only binary `--remote-install` sends is its own.

Two things change on the way:

- **Paths.** The editor holds paths relative to the workspace the far end
  serves; the server knows them as absolute paths over there. The prefix is
  added when a path becomes a URI and taken off when one comes back, at the
  single place where that conversion happens. A URI *outside* the workspace —
  go-to-definition into an indexed dependency — keeps its absolute form, and
  what happens next is the file server's decision: it refuses to read outside
  the workspace, by name.
- **Environment.** A definition's `env` moves into the command as
  `env NAME=VALUE …` rather than being set on the process deco spawns, because
  that process is `ssh` on *this* machine. Left where it was it would have been
  set in the wrong place and never reached the server — the kind of failure that
  looks like the setting being ignored. A name deco cannot pass through an
  argument vector is refused by name rather than mangled.

Servers are given longer to answer `initialize` in a remote session, and not by
a little: the wait covers an SSH handshake and a language server reading a
project from a disk this machine never touches.

## Searching the workspace

`ctrl+shift+f` searches the remote, because that is where the files are. It used
to be refused: a local walk in a remote session searches *this* machine and
reports matches in files the editor is not showing.

The matching happens on the far end, with the same function the find bar and the
local project search use — `deco-remote` depends on `deco-core` for exactly that
reason. Two definitions of what counts as a match would drift, and a term that
matched in one place and not the other would be worse than no search at all.

The limits are the server's, not the client's: five hundred matches and a
megabyte per file, enforced over there because the client is whatever is on the
other end of a connection the server did not authenticate. A search that stopped
early says so.

Two things are worth knowing:

- **`files.exclude` is applied here, not there.** The server reads no settings —
  deliberately, since answering `fs.read` by consulting a `settings.json` on the
  remote would be an authority nobody gave it — so the only end that can apply
  your excludes is this one. The server still skips `.git`, `node_modules` and
  `target` on its own, which is where most of the cost of a walk is.
- **The count shown is the count after filtering**, which can be fewer than the
  server found.

## Reaching a port on the remote

A dev server on the remote's `:3000` has no route from here. `--forward` gives it
one:

```console
$ deco --remote ssh-remote+myhost --forward 3000 src/main.rs
$ deco --remote ssh-remote+myhost --forward 8080:3000 src/main.rs
```

The first makes the remote's `3000` answer on this machine's `3000`; the second
puts it on `8080` instead, for when something local already holds the port. The
forward lasts as long as the editor session and the port is released when it
ends.

### deco is its own tunnel

`ssh -L` exists and does this well, and nothing here uses it, because it is
available on exactly one of the three transports — `docker exec` cannot forward
a port at all, and a WSL distribution has no `-L` either.

So the remote's deco is the tunnel. Each connection runs:

```console
$ ssh myhost deco --forward-to 127.0.0.1:3000 --stdio
```

which connects to that port and pipes it to its own stdin and stdout. Every
transport can already carry a program's stdio — that is how the file server
works — so this works over all three, with no `socat`, no `nc`, and nothing on
the remote that deco did not put there. It is the same binary
`--remote-install` provisions, found the same way.

The cost is a process per connection, which over SSH would be an authentication
round-trip each time — twenty of them for one page load. So deco multiplexes: the
first connection sets up a control socket and the rest are local work.

That needs a `ControlPath`, and it is a path rather than a flag for a reason.
`ControlMaster=auto` on its own does *nothing*: OpenSSH's `ControlPath` has no
default, and without one the setting is silently inert. deco used to pass
`ControlMaster` alone and claim multiplexing it did not have.

### Loopback at both ends

- **On the remote**, `--forward-to` accepts loopback addresses only.
  `--forward-to 10.0.0.5:5432` is refused by name, because a deco that dials
  anywhere its host can reach is a proxy into that network — the same authority
  the file server refuses to have over paths. A *name* is resolved first and
  every address it resolves to is checked, since `localhost` is only loopback by
  convention and a remote's `/etc/hosts` can say otherwise.
- **On this machine**, the listener binds `127.0.0.1` and never `0.0.0.0`.
  Typing a port number is not a request to put someone else's database on your
  network.

### Who can use a forward

The honest version, threat by threat.

**From the network — no.** The listener binds `127.0.0.1`, so packets from
another machine are not routed to it at all; there is no port open on this
machine's network interfaces. And the forwarded traffic never crosses a network
in the clear: over SSH it rides inside the SSH connection, and over `docker exec`
or WSL it never leaves the machine. Network equipment on the path sees SSH
ciphertext and nothing else. A test asserts the listener is loopback, because
that one line is the whole of this paragraph and nothing else would catch it
being changed to `0.0.0.0` for convenience.

**Another user on the same machine — yes, and this is the real exposure.**
Loopback is not per-user: any local account can connect to a forwarded port and
reach the remote's service through it, for as long as the session runs. This is
not particular to deco — `ssh -L` and every other port forwarder have exactly
the same property — but it is true, and worth knowing before forwarding a
database on a shared machine.

What deco does about it: forwards are opt-in per port, they last only as long as
the session, and they reach only the remote's loopback. What deco does **not**
do is authenticate the connecting process — there is no portable way to identify
the peer of a TCP connection, and pretending otherwise with a check that works on
one platform would be worse than saying so.

The SSH control socket is a related and sharper case, because it *is* an
authenticated connection to the remote: anyone who can reach the socket can ride
it. It goes in `$XDG_RUNTIME_DIR/deco`, or `~/.ssh/deco` — never the shared
temporary directory — created `0700`, refused if it is a symbolic link, and
refused if deco cannot `chmod` it, which is also how it knows the directory is
this account's own.

**A program running as you — yes, and there is nothing to be done at this
layer.** It can use the forward. It can also read your SSH keys, run `ssh`
itself, or attach a debugger to the editor. Code running as you already has
everything a forward would give it, so defending the forward against it would be
security theatre.

One thing that is *not* on the list: nothing new listens on the remote. The
tunnel processes are started per connection through the transport's stdio, so
there is no daemon over there for anyone to find.

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
3. ~~Provisioning: getting the binary onto the remote, which means a decision
   about how much deco is willing to install on a machine you pointed it at.~~
   **Done** for same-platform remotes, under the rules above. Fetching a build
   for a *different* platform is still open, and is the same decision again in a
   harder form: it needs somewhere deco is willing to download from.
4. Settings scope wiring: the `Remote` layer already exists between `User` and
   `Workspace` in the settings stack, so a remote's settings have somewhere to go.
5. ~~Port forwarding, which the transports do not model at all.~~ **Done**, by
   making deco the tunnel rather than reaching for `ssh -L` — see above.
6. ~~Language servers on the remote.~~ **Done** — the same definitions, wrapped
   in the transport, with the far end's paths on the wire.
7. Extensions on the remote. A host started by a remote session still runs here,
   and moving it means deciding what a remote extension is allowed to reach —
   the same question the capability sandbox answers locally, asked again across
   a machine boundary.
8. ~~Project-wide search, which needs the server to walk the workspace rather
   than this machine walking one it does not have.~~ **Done** — `fs.search`, with
   the far end matching.
