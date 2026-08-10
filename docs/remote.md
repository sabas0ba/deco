# Remote development

> **State of this: half built, and the half that exists is the local half.**
> deco parses VS Code's remote authorities, builds the commands that would reach
> them, and speaks the framed protocol the two ends would use. What does not exist
> is the other end: there is no `deco --server`, no provisioning it onto a remote,
> and no port forwarding. This page describes what is there so that the gap is
> clear rather than implied.

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
and tested. Nothing sends a frame anywhere yet, because there is nothing at the
far end to answer.

## What a working version needs

Named so that the remaining work is legible rather than open-ended:

1. `deco --server`, a headless session that owns a document and answers frames.
2. Provisioning: getting that binary onto the remote and starting it, which means
   a decision about how much deco is willing to install on a machine you pointed it
   at.
3. Settings scope wiring: the `Remote` layer already exists between `User` and
   `Workspace` in the settings stack, so a remote's settings have somewhere to go.
4. Port forwarding, which the transports do not model at all.

Item 2 is the one with a security decision in it, not just work.
