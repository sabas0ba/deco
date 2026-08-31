# Remote development

> **State of this: you can open a file on another machine, edit it and save it
> back, put deco there if it has none, reach a port on it, and run language
> servers and Git over there, and search it.** What is missing is extensions on the
> remote. This page is explicit about each.

## Using it

```console
$ deco --remote ssh-remote+myhost --workspace /home/u/project src/main.rs
```

That starts `deco --server --stdio` on the far end over SSH, reads `src/main.rs`
through the connection, and opens it. `ctrl+s` writes it back over the same
connection. `ctrl+p` lists the **remote** workspace, because that is where the
files are.

Git status, committed text, stage, unstage and commit also run on the far end.
They use a second connection and a worker of their own, so a working-tree walk
or commit hook does not stop file reads or the editor's event loop.

Paths are relative to the workspace the server was given. Without `--workspace`
the server serves wherever the transport lands, which for SSH is the account's
home directory.

Every path in a remote session belongs to that one workspace, including the one
you type into **Save As**: it is taken as the far end spells it and the copy is
written there, not onto this machine. `~` and this machine's working directory
do not come into it, and a name that points outside what the server serves is
refused by name rather than written somewhere else. The workspace is one place;
half of one would make every path ambiguous. **Revert** reads over the same
connection, for the same reason.

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
- **A different platform needs a second, larger yes.** The remote is asked what
  it is *before* anything is sent. `--remote-install` on its own reaches
  nothing — it copies the file already running here — so a mismatch names both
  sides rather than uploading a binary that cannot run there. Downloading one
  that can is `--remote-install-download`, below.
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

### When the remote is another platform

`--remote-install` sends *this* machine's binary, so both ends must match: Linux
to Linux, and every WSL and container case. A macOS laptop provisioning a Linux
server needs a binary this machine does not have.

`--remote-install-download` fetches one:

```console
$ deco --remote ssh-remote+buildbox --remote-install-download src/main.rs
```

It is a **separate flag on purpose**. Copying the file you are already running
reaches nothing; fetching an executable over the network is a different thing to
be allowed to do, and a flag that quietly grew that power would be the kind of
convenience this project keeps refusing. `--remote-install` alone still refuses
a mismatch exactly as it did.

What it fetches is the release for this deco's *own* version — the same archive
and the same `SHA256SUMS` the [README's install
section](../README.md#a-prebuilt-binary) tells a person to download by hand.

- **The checksum is checked here, by deco.** The archive is hashed in deco's own
  code and compared with the line `SHA256SUMS` carries for it. A mismatch is
  discarded, naming both hashes, and nothing is written.
- **An archive nothing vouches for is refused before it is downloaded.** The
  checksums come first; if they list no line for the asset, the archive is never
  asked for. There is no path where unverified bytes are used because they had
  already arrived.
- **Nothing from the archive touches the filesystem as a tree.** The one member
  holding the binary is read out with `tar xzO`, so a path with `..` in it or a
  symlink pointing somewhere it should not has nowhere to land.
- **The transfer and the unpacking are `curl` and `tar`.** Both ship with every
  platform this runs on, and both are missing-by-name failures rather than
  silent ones. Only the check is deco's own — see
  [Dependencies](../README.md#dependencies) for why that line is drawn there and
  not somewhere cheaper.

Once the binary is in hand it is uploaded by exactly the path above: staged
beside the destination, made executable, renamed, and then asked for its version.

**A platform with no published build is still refused by name.** The releases
carry four POSIX targets — Linux and macOS, x86-64 and ARM64 — and anything else
is told so rather than given the nearest thing.

`uname` cannot tell glibc from musl, so an Alpine remote is sent the `-gnu`
build. That is not silently wrong: the installed binary is asked for its version
and one that cannot run does not answer, so it surfaces as a refusal at the end
rather than as a session that mysteriously never connects.

The remote is assumed to have a POSIX shell and `uname`, `mkdir`, `dd`, `chmod`
and `mv` — the same assumption already made by running `deco --server` over
`ssh`. The download adds nothing to that list: it all happens on this end.

## Settings that belong to the machine

Some settings are facts about a *machine* rather than about a person: where the
toolchain is on the build box, which interpreter that container has. VS Code
calls these machine settings and keeps them on the remote; deco does the same,
in `machine-settings.json` beside the remote's own `settings.json`:

```jsonc
// ~/.config/deco/machine-settings.json, on the remote
{
  "deco.lsp.servers": {
    "rust-analyzer": { "languages": ["rust"], "command": "/opt/rust/bin/rust-analyzer" }
  }
}
```

Connect, and it becomes the **`remote` layer**: above your own `settings.json`,
below the project's, exactly where VS Code puts it. `deco --print-config` names
the file it came from.

**It is a separate file from that machine's `settings.json`, deliberately.**
Serving the account's own editor configuration would mean connecting quietly
adopted somebody else's theme, font and keybindings — and would turn an ordinary
local setup into something a visitor's session has to treat as suspect. A
machine-settings file is written on purpose, by someone who meant it to be seen
by whoever connects.

### It is not trusted, and that is the point

`machine-settings.json` sits where anyone with an account on that machine can
write it. Choosing to connect to a machine is a decision about the machine; it
is not a signature on every file that happens to be on it. So this layer is
treated like a cloned repository's `.vscode/settings.json`:

- **A language server defined there is confirmed before it runs.** A definition
  is a program to execute, and connecting must not be enough to execute one.
  This is the same rule that already applies to a workspace's, for the same
  reason — see [Language servers](language-servers.md).
- **It cannot choose the extension sandbox.** `deco.extensions.sandbox` and its
  neighbours are read from deco's defaults and your own file only, and an
  attempt from here is reported rather than dropped in silence.
- **`--clean` ignores it**, along with everything else. A flag meaning "start
  with nothing" that still adopted another machine's settings would not be the
  flag it says it is.

Everything else — tab size, rulers, word wrap, a colour theme — applies
normally. Those change what you see, not what runs.

### How it gets here

The server has one method for it, `settings.read`, and that method **takes no
path**. A client cannot ask for a file of its choosing; it asks for "this
machine's settings" and receives whatever is at the one path the server works
out for itself. So the [confinement rule](#one-directory-and-no-way-out-of-it)
is untouched: there is still exactly one directory a client can steer a read
into.

The server does not read the file to *decide* anything either. It hands over
bytes; this end parses them, places them in the layer, and applies the trust
rules above. A server that obeyed a settings file would be taking an authority
nobody gave it — reading one to pass along is not that.

A server too old to have the method says so in its handshake, and this end does
not ask. A machine with no `machine-settings.json` is the ordinary case and is
not an error; a file that cannot be read *is* reported, because a layer that
silently did not apply is how "why is my setting being ignored" starts.

Whoever starts the server can point it elsewhere with `--machine-settings
<path>`. That is a decision made where the server is launched — on the remote,
by whoever runs it — and not one a client can make.

## Extensions

The host stays on this machine. What changes in a remote session is where its
file requests are served from: reading, writing, `stat`, `readDirectory`, and
creating, deleting, renaming and copying all go through the same connection the
editor uses, so an extension sees — and changes — the workspace being edited.

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
manifest declared and nobody has ruled on. `prompt` — the default — asks, and the
extension waits for the answer. `allow` serves it without asking, which is the
deliberate downgrade the setting describes: declaration becomes the only check.
`deny` refuses without asking.

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

- **`files.exclude` is applied here, not there.** The server does not *act* on
  settings — answering `fs.read` by consulting a file on the remote would be an
  authority nobody gave it — so the only end that can apply your excludes is
  this one. (It will hand over the machine's settings when asked; that is
  [a different thing](#settings-that-belong-to-the-machine), and it still does
  not obey them.) The server still skips `.git`, `node_modules` and `target` on
  its own, which is where most of the cost of a walk is.
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

The server answers a handshake naming the protocol version and the workspace,
the `fs.*` and `scm.*` families, and `settings.read`. That is what opening,
listing, searching and saving a file needs, plus source control and the
machine's own settings.

It loads no theme or settings and starts no language server or extension. The
explicit exception is `scm.*`: those methods run the remote's `git`, and
`scm.apply` changes its index or creates a commit. Commit hooks run as they do
locally; they are arbitrary programs with the remote account's authority. The
client cannot choose another executable through the protocol.

### One directory, and no way out of it

Every path is resolved and confined to the `--workspace` directory. Anything
outside it is refused **by name**, in both directions — reading and writing.

The repository root is confined too. A workspace that is only a subdirectory
of a repository does not make its parent newly reachable; `scm.*` is refused
until the session serves the repository root itself.

Git's per-worktree administrative directory and shared common directory must
also be inside the workspace. A linked worktree whose `.git` file points back
to metadata in another checkout is refused even though its visible repository
root is inside the workspace; otherwise staging or committing there would
change an index, refs, and object store the server was not given authority over.

`settings.read` is the one answer about a file outside it, and it is shaped so
the rule still holds: it takes **no path**. A client cannot name a file, only ask
for "this machine's settings", and gets whatever is at the one path the server
computes for itself. What a client can *steer* a read into is still exactly one
directory.

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
9. ~~Git status, committed text and writes on the machine holding the
   repository.~~ **Done** — `scm.*` on a dedicated connection, confined to the
   served workspace.
