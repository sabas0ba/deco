# Remote development

> **Status: you can open, edit and save a file on another machine, install deco there if it is missing, forward a port from it, run language servers and Git there, and search it.** Extensions do not yet run on the remote. This page describes each of these.

## Using it

```console
$ deco --remote ssh-remote+myhost --workspace /home/u/project src/main.rs
```

This starts `deco --server --stdio` on the remote environment over SSH, reads `src/main.rs` through the connection, and opens it. `ctrl+s` writes it back over the same connection. `ctrl+p` lists files in the **remote** workspace.

Git status, committed text, diff comparisons, branch preflight, checkout,
stage, unstage and commit also run on the remote environment.
They use a second connection and a worker of their own, so a working-tree walk
or commit hook does not stop file reads or the editor's event loop.

Paths are relative to the workspace the server was given. Without `--workspace`, the server uses the transport's initial working directory, which for SSH is the account's home directory.

All paths in a remote session belong to the served workspace, including the path entered in **Save As**. That path is interpreted on the remote environment and the copy is written there, not on the local machine. `~` and the local working directory are not used. A path outside the served directory is rejected with an error naming it, rather than written elsewhere. Using a single workspace for every path keeps paths unambiguous. **Revert** also reads over the same connection.

### What is different in a remote session

Extension hosts behave differently in a remote session:

- **Extension hosts run locally, and their file access goes over the connection.** A host started by a remote session is a local Node process in the usual container sandbox. `vscode.workspace.fs.readFile` and related calls are answered through the session's connection, so an extension reads the files being edited rather than the files at the same path on the local machine. See below.

A failed save is the one remote failure that is *not* fatal, because the connection can drop while the editor still holds the text and can retry. A failed remote save is reported and the document stays dirty.

## Getting deco onto the remote

The remote environment needs a `deco` binary. If one is on its PATH, no further setup is needed. If it is installed in a directory that a login shell does not search, specify its path:

```console
$ deco --remote ssh-remote+myhost --remote-server-path ~/.deco/bin/deco src/main.rs
```

If the remote has no `deco`, deco can upload the local binary **when requested**:

```console
$ deco --remote ssh-remote+myhost --remote-install src/main.rs
```

The binary is installed to `$HOME/.deco/bin/deco` on the remote, in the account's home directory rather than on the system path. A per-user install needs no privileges and does not affect other users. `--remote-server-path` selects a different location.

### What it will not do

Connecting to a machine does not authorise deco to install software on it. Installation therefore requires an explicit flag and is never automatic:

- **Nothing is installed unless requested.** If no `deco` is found, the session fails and the error mentions `--remote-install`.
- **A different platform requires a separate flag.** deco queries the remote platform *before* sending anything. `--remote-install` only copies the binary that is running locally and does not use the network, so on a platform mismatch it reports both platforms instead of uploading a binary that cannot run. `--remote-install-download` downloads a matching build; see below.
- **Files that are not deco are not overwritten.** deco checks separately whether the path exists and whether it runs. If `--remote-server-path` points by mistake at a file such as `notes.txt`, which exists but does not answer `--version`, the install is refused.
- **A partial upload never replaces the destination.** The upload is written to `deco.incoming` in the same directory and renamed when complete, so an interrupted install leaves either the old deco or nothing.
- **The result is verified.** deco asks the installed binary for its version, so a `noexec` mount or a missing libc is reported as an error instead of causing a handshake that never completes.

If the destination already has deco of the same version, it is not replaced, so the binary is not uploaded on every start.

### When the remote is another platform

`--remote-install` sends the local binary, so both machines must use the same platform. This covers Linux to Linux and all WSL and container cases. A macOS machine provisioning a Linux server does not have a suitable binary locally.

`--remote-install-download` fetches one:

```console
$ deco --remote ssh-remote+buildbox --remote-install-download src/main.rs
```

This is a **separate flag on purpose**. Copying the running binary does not use the network. Downloading an executable does, so it requires its own explicit permission. `--remote-install` alone still refuses a platform mismatch.

It downloads the release for the local deco's *own* version: the same archive and `SHA256SUMS` that the [README's install section](../README.md#a-prebuilt-binary) tells users to download manually.

- **deco verifies the checksum locally.** deco hashes the archive in its own code and compares the result with the archive's entry in `SHA256SUMS`. On a mismatch the archive is discarded, the error names both hashes, and nothing is written.
- **An archive without a checksum entry is not downloaded.** The checksums are downloaded first. If they have no entry for the asset, the archive is not requested, so unverified data is never used.
- **The archive is not extracted to the filesystem.** Only the member containing the binary is read, with `tar xzO`, so entries containing `..` or symbolic links pointing elsewhere are never written.
- **Download and extraction use `curl` and `tar`.** Both are available on every supported platform, and a missing tool is reported by name. Only the checksum verification is implemented in deco; see [Dependencies](../README.md#dependencies) for the reasoning behind this split.

The downloaded binary is then uploaded with the same steps as above: staged beside the destination, made executable, renamed, and asked for its version.

**A platform without a published build is refused with an error naming it.** Releases include four POSIX targets: Linux and macOS on x86-64 and ARM64. deco does not substitute the closest build for other platforms.

`uname` cannot distinguish glibc from musl, so an Alpine remote receives the `-gnu` build. If that binary cannot run, the final version check fails and the install is refused, instead of producing a session that never connects.

The remote must provide a POSIX shell and `uname`, `mkdir`, `dd`, `chmod` and `mv`. Running `deco --server` over `ssh` already requires these. The download adds no requirements on the remote because it runs locally.

## Settings that belong to the machine

Some settings describe a *machine* rather than a user, for example the toolchain location on a build machine or the interpreter in a container. VS Code calls these machine settings and stores them on the remote. deco does the same, in `machine-settings.json` beside the remote's own `settings.json`:

```jsonc
// ~/.config/deco/machine-settings.json, on the remote
{
  "deco.lsp.servers": {
    "rust-analyzer": { "languages": ["rust"], "command": "/opt/rust/bin/rust-analyzer" }
  }
}
```

After connecting, this file becomes the **`remote` layer**: above your own `settings.json` and below the project's, the same position VS Code uses. `deco --print-config` shows the file it was read from.

**This file is deliberately separate from the remote machine's `settings.json`.** Using the remote account's editor configuration would apply another person's theme, font and keybindings on connection. It would also turn an ordinary local configuration into a file that connecting sessions must treat as untrusted. A machine-settings file is created specifically to be read by clients that connect.

### It is not trusted, and that is the point

`machine-settings.json` is stored where anyone with an account on that machine can write it. Connecting to a machine does not mean trusting every file on it. This layer is therefore treated like a cloned repository's `.vscode/settings.json`:

- **A language server defined there is refused, with a message naming it.** A server definition is a program to execute, and connecting alone must not execute it. The same rule applies to workspace settings; see [Language servers](language-servers.md).
- **It cannot select the extension sandbox.** `deco.extensions.sandbox` and related settings are read only from deco's defaults and your own settings file. Attempts to set them in this layer are reported.
- **`--clean` ignores it**, like all other settings files. `--clean` starts with no configuration, so it does not load the remote machine's settings either.

All other settings, such as tab size, rulers, word wrap and colour theme, apply normally. They affect display, not which programs run.

### How it gets here

The server provides this file through one method, `settings.read`, which **takes no path**. A client cannot request an arbitrary file. It requests this machine's settings and receives the contents of the single path that the server determines itself. The [confinement rule](#one-directory-and-no-way-out-of-it) therefore still holds: a client can direct reads into only one directory.

The server does not use the file for its own decisions. It sends the bytes, and the client parses them, adds them to the layer and applies the trust rules above. The server only passes the file along; it does not act on it.

A server too old to support the method reports this in its handshake, and the client does not call it. A missing `machine-settings.json` is normal and not an error. A file that exists but cannot be read is reported, so that a layer that failed to apply does not go unnoticed.

The server can be pointed at a different file with `--machine-settings <path>`. This is chosen by whoever launches the server on the remote; a client cannot choose it.

## Extensions

The extension host stays on the local machine. In a remote session, its file requests are served over the editor's connection: reading, writing, `stat`, `readDirectory`, and creating, deleting, renaming and copying. An extension therefore reads and changes the workspace being edited.

This prevents an extension from reading local files by mistake. The path an extension requests exists on the remote. A local read at the same path would return a different checkout, or nothing, and the result would look the same in either case. The server's rules therefore also apply to extensions: a path outside the workspace is rejected with an error naming it, as it is for the editor.

Extension hosts still run locally. Process execution, clipboard access, secrets and `openExternal` are not implemented by the host integration; permission declarations do not enable these operations. See [extension API limitations](extensions.md#what-is-still-not-connected).

Running the host *on* the remote is a different design, not an extension of this one. It needs Node on the remote, which deco does not install. It also moves the capability broker, which mediates between a cloned repository's extension and your machine, onto a machine that may be shared. The current design does not prevent adding it later. VS Code's remote support has both kinds, and its UI-side extensions access workspace files in the same way as here.

### Permissions

`extensions.permissions.default` decides what happens to a capability that the manifest declares and that has no stored decision. `prompt`, the default, asks the user, and the extension waits for the answer. `allow` grants it without asking, so the declaration becomes the only check. `deny` refuses without asking.

## Language servers

Language servers run on the machine that holds the files. A server started locally would index a checkout that does not exist.

No configuration is needed. The same `deco.lsp.servers` definitions are used, with each command wrapped in the transport: `rust-analyzer` in your settings becomes `ssh myhost rust-analyzer`. The server must therefore be installed on the remote. deco does not install language servers; `--remote-install` only sends deco itself.

Two things differ from a local session:

- **Paths.** The editor holds paths relative to the workspace served by the remote environment, and the server uses absolute paths on the remote. The prefix is added when a path is converted to a URI and removed when a URI comes back, in a single conversion function. A URI *outside* the workspace, such as the target of go-to-definition into an indexed dependency, keeps its absolute form. The file server then rejects reads outside the workspace with an error naming the path.
- **Environment.** A definition's `env` is added to the command as `env NAME=VALUE …` instead of being set on the spawned process, because that process is the local `ssh`. Variables set on it would not reach the server, and the setting would appear to be ignored. A name that cannot be passed in an argument vector is rejected with an error naming it.

In a remote session, servers get a considerably longer timeout for `initialize`. The wait includes the SSH handshake and the server reading the project from the remote disk.

## Searching the workspace

`ctrl+shift+f` searches the remote workspace. It was previously refused, because a local walk in a remote session searches the local machine and reports matches in files the editor is not showing.

Matching runs on the remote environment and uses the same function as the find bar and local project search; `deco-remote` depends on `deco-core` for this reason. Separate implementations could diverge, so that a term matched in one search and not in another.

The limits are enforced by the server, not the client: five hundred matches and one megabyte per file. They are enforced on the server because the server does not authenticate the client at the other end of the connection. A search that stops early reports this.

Two details:

- **`files.exclude` is applied locally, not on the server.** The server does not act on settings; consulting a remote file to answer `fs.search` would give it authority it was not given. Only the client can therefore apply your excludes. (The server does send the machine's settings on request, as described in [a separate section](#settings-that-belong-to-the-machine), but it does not act on them.) The server still skips `.git`, `node_modules` and `target` itself, which accounts for most of the cost of a walk.
- **The displayed count is taken after filtering**, so it can be lower than the number the server found.

## Reaching a port on the remote

A dev server on the remote's `:3000` is not reachable from the local machine. `--forward` makes it reachable:

```console
$ deco --remote ssh-remote+myhost --forward 3000 src/main.rs
$ deco --remote ssh-remote+myhost --forward 8080:3000 src/main.rs
```

The first command makes the remote's port `3000` available on local port `3000`. The second uses local port `8080`, for when a local process already uses the port. The forward lasts as long as the editor session, and the port is released when the session ends.

### deco is its own tunnel

deco does not use `ssh -L`, because only one of the three transports supports it. `docker exec` cannot forward ports, and a WSL distribution has no equivalent of `-L`.

Instead, the remote's deco acts as the tunnel. Each connection runs:

```console
$ ssh myhost deco --forward-to 127.0.0.1:3000 --stdio
```

This command connects to the port and relays its data over its own stdin and stdout. Every transport can carry a program's stdio, as the file server already requires, so forwarding works over all three transports without `socat`, `nc` or any other tool on the remote that deco did not install. It uses the same binary that `--remote-install` installs, located the same way.

Each connection starts a process. Over SSH, each process would need an authentication round trip, which can be twenty round trips for one page load. deco therefore multiplexes SSH connections: the first connection creates a control socket, and later connections reuse it without authenticating again.

Multiplexing requires a `ControlPath`. `ControlMaster=auto` alone has *no effect*, because OpenSSH's `ControlPath` has no default. An earlier version of deco passed only `ControlMaster` and did not actually multiplex.

### Loopback at both ends

- **On the remote**, `--forward-to` accepts only loopback addresses. `--forward-to 10.0.0.5:5432` is rejected with an error naming it. A deco that could connect to any address its host can reach would act as a proxy into that network, which is the same kind of authority the file server refuses for paths. A host *name* is resolved first and every resulting address is checked, because `localhost` is loopback only by convention and the remote's `/etc/hosts` can map it elsewhere.
- **On the local machine**, the listener binds `127.0.0.1`, never `0.0.0.0`. Forwarding a port does not expose the remote service to your network.

### Who can use a forward

Forwarded ports restrict network access but do not authenticate local clients.

**From the network: no.** The listener binds `127.0.0.1`, so packets from other machines are not routed to it, and no port is opened on the local machine's network interfaces. Forwarded traffic is not sent over a network unencrypted: over SSH it is carried inside the SSH connection, and over `docker exec` or WSL it stays on the local machine. Network equipment on the path sees only SSH ciphertext. A test asserts that the listener binds to loopback, so that a change to `0.0.0.0` is detected.

**Another user on the same machine: yes.** This is the main exposure. Loopback is not per-user, so any local account can connect to a forwarded port and reach the remote service while the session runs. `ssh -L` and other port forwarders have the same property. Consider this before forwarding a database on a shared machine.

deco limits the exposure as follows: forwards are opt-in per port, last only as long as the session, and reach only the remote's loopback. deco does **not** authenticate the connecting process. Applications exposed through a forwarded port must provide their own authentication if local clients should be restricted.

The SSH control socket needs stricter protection, because it *is* an authenticated connection to the remote and anyone who can reach the socket can use it. It is created in `$XDG_RUNTIME_DIR/deco` or `~/.ssh/deco`, never in the shared temporary directory. The directory is created with mode `0700`, refused if it is a symbolic link, and refused if deco cannot `chmod` it. The `chmod` check also confirms that the directory belongs to the current account.

**A program running as you: yes, and this layer cannot prevent it.** Such a program can use the forward. It can also read your SSH keys, run `ssh` itself, or attach a debugger to the editor. It already has access to everything a forward provides, so protecting the forward against it would not improve security.

Nothing new listens on the remote. Tunnel processes are started per connection through the transport's stdio, so there is no daemon on the remote.

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

An unknown kind before the `+` is an error naming the kind. deco does not fall back to a local session, because connecting to the wrong machine is worse than not connecting.

## Transports

Each authority maps to the command that reaches it: `ssh`, `wsl.exe`, or `docker exec`. The argument vector is built as a list, never as a shell string, so a hostname or container id containing shell metacharacters is passed as an argument and not interpreted. The language-server launcher follows the same rule. OpenSSH joins the arguments after the host into one string that the remote login shell parses, so deco single-quotes each remote argument; a workspace path with spaces or a `;` arrives as one argument. This requires a POSIX-compatible login shell on the remote, such as `sh`, `bash`, `dash`, `zsh` or `ksh`; `csh`, `tcsh` and `fish` are not supported. `docker exec` passes the arguments exactly. `wsl.exe` currently passes them unquoted, and the distribution's default shell may split or interpret a WSL workspace path containing spaces or shell metacharacters; this has not been verified on a real machine.

## The wire protocol

Both ends use length-prefixed framing over the transport's stdio, the same format as the Language Server Protocol. A stream that carries both a program's output and protocol messages needs unambiguous boundaries between frames.

The framing, the authority parsing and the command construction are implemented
and tested, and so is the remote environment that answers them.

## The server

```console
$ ssh myhost deco --server --stdio --workspace /home/u/project
```

This command is not written by hand: `deco_remote::server_command` builds it and `command_for` wraps it in the transport. It is the exact command that runs, and a test asserts that the command the client builds is the one the server parses.

The server answers a handshake naming the protocol version and the workspace.
The client refuses a server whose version differs, so a server installed by an
older deco must be replaced, for example with `--remote-install`. Version 2
added regular-expression search to `fs.search`. The server implements
the `fs.*` and `scm.*` families, and `settings.read`. That is what opening,
listing, searching and saving a file needs, plus source control and the
machine's own settings.

It loads no theme or settings and starts no language server or extension. The
explicit exception is `scm.*`: those methods run the remote's `git`, and
`scm.comparison` reads the HEAD/index/working-tree pair needed by the selected
source-control row; the branch methods list and preview local branches; and
`scm.apply` changes the index, creates a commit or switches the working tree.
Commit hooks run as they do locally; they are arbitrary programs with the
remote account's authority. The client cannot choose another executable
through the protocol.

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

`settings.read` is the only method that returns a file outside the workspace, and it keeps the rule intact because it takes **no path**. A client cannot name a file; it can only request this machine's settings, and it receives the file at the one path the server computes itself. Reads that a client can direct are still limited to one directory.

This is stricter than VS Code, whose remote server opens any path the account can reach. deco is stricter because the client is whatever is at the other end of a connection that deco did not authenticate itself. A frontend bug, a hijacked session, or a `deco-remote://` link written by someone else must not be able to request `~/.ssh/id_ed25519`.

Confinement is checked on the **canonical** path, so a symlink inside the workspace that points outside it is also rejected. Checking the path as written would allow `project/link-to-etc/passwd`. A sibling directory whose name starts with the same text (`project-secrets` against `project`) is outside, because paths are compared by component, not as strings.

A file that is not valid UTF-8 is rejected rather than repaired. Repairing it would insert replacement characters that deco would write back on save, corrupting the file.

## What a working version needs

The remaining work:

1. ~~`deco --server`, a headless session that answers frames.~~ **Done.**
2. ~~The client: opening a file through a transport, saving it back, and listing
   the remote workspace with `ctrl+p`.~~ **Done.**
3. ~~Provisioning: getting the binary onto the remote, which means a decision about how much deco is willing to install on a machine you pointed it at.~~ **Done** — `--remote-install` for same-platform remotes, and `--remote-install-download` for a *different* platform, verified against the release's `SHA256SUMS`; see [When the remote is another platform](#when-the-remote-is-another-platform).
4. ~~Settings scope wiring: the `Remote` layer already exists between `User` and `Workspace` in the settings stack, so a remote's settings have somewhere to go.~~ **Done** — the remote's `machine-settings.json`, fetched with `settings.read`, fills the `Remote` layer; see [Settings that belong to the machine](#settings-that-belong-to-the-machine).
5. ~~Port forwarding, which the transports do not model at all.~~ **Done**, with deco as the tunnel instead of `ssh -L`; see above.
6. ~~Language servers on the remote.~~ **Done** — the same definitions, wrapped
   in the transport, with the remote environment's paths on the wire.
7. Extensions on the remote. A host started by a remote session still runs locally. Moving it requires deciding what a remote extension may access: the question the capability sandbox answers locally, applied across a machine boundary.
8. ~~Project-wide search, which needs the server to walk the workspace rather
   than this machine walking one it does not have.~~ **Done** — `fs.search`, with
   the remote environment matching.
9. ~~Git status, committed text and writes on the machine holding the
   repository.~~ **Done** — `scm.*` on a dedicated connection, confined to the
   served workspace.
