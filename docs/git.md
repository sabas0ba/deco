# Git

![The branch in the status bar, unmoved by typing and refreshed by a save and a new file](img/git-status.svg)

deco reads git the way VS Code does: by running the `git` binary and parsing
what it says. No library is linked. The first of the three stages is built —
**the branch and what differs from it, in the status bar**. The gutter marks and
the source-control view are still [planned](roadmap.md#git).

```
main ±2 ↑2
```

Each marker is omitted at zero, which is the same bargain the problem tallies
make: a permanent `0 changed` is noise, and the absence of the marker is the
signal.

| | |
| --- | --- |
| `main` | the branch, or a seven-character commit id when `HEAD` is detached |
| `±2` | two files differ from `HEAD` — staged, unstaged or untracked |
| `↑2 ↓1` | two commits to push, one to pull; only when the branch tracks another |
| `!1` | one file a merge left conflicted, which has to be dealt with first |

**One per file, not one per side.** A file that is staged *and* modified since is
one thing to think about, so it counts once — otherwise the bar would disagree
with the list that the source-control view will show.

## When it runs

This is the part worth being careful about, because the alternative is a process
per keystroke.

`git status` runs when something has happened that it would report differently:
a **save**, a file **created, renamed or deleted** from the tree, and once at
startup. Typing does not run it, which is what the animation above shows — the
file is edited and the bar does not move until `ctrl+s`.

It runs **on a thread**. On deco's own checkout `git status` is a few
milliseconds; on a working tree with a million files it is not, and an editor
that stopped painting while git thought would be worse than one whose branch
name is a moment stale.

**One at a time, and nothing is lost.** The request is marked taken when a run
*starts*, not when it answers. So a save made while git is still thinking sets
the flag again, the earlier answer lands without clearing it, and a fresh run
follows. Clearing on the answer instead would swallow that save silently, and
the bar would sit there being wrong until the next one.

## When there is nothing to show

Three situations look identical on screen, and all three show nothing at all —
no branch, and no gap where one would be:

- **No git on this machine.** The feature is *absent* rather than broken.
- **The folder is not a repository.** Which is normal, and not worth a line of
  its own on everyone's status bar.
- **Nobody has asked yet**, in the moment before the first run answers.

The first two are remembered for the session and never asked about again: they
will still be true after the next save, and spawning a process to re-learn a
known fact is a cost with no benefit. Anything else — git refusing because a
rebase is in progress, an index lock held by a command in a terminal — is
transient, so the next save tries again.

The reason is kept even though there is nowhere to put it. When the
[panel](chrome.md) grows an output view, that is where it will go; until then it
is readable from the frontend and asserted by a test, so deco knowing *why* it
is showing nothing is a fact rather than an intention.

## Settings

| Setting | What deco does with it |
| --- | --- |
| `git.enabled` | VS Code's meaning and VS Code's default of `true`. Turning it off stops the process being spawned *and* takes the segment off the bar — a setting that only hid the result would still be paying for it |
| `git.path` | Where `git` is, for a machine that keeps it somewhere unusual. Empty or unset means whatever `PATH` finds |

## Why the binary and not a library

The same three reasons VS Code has:

- **It inherits the user's git.** Their `includeIf` config, their
  `credential.helper`, their hooks, their `core.fsmonitor`. A library
  reimplements a subset of that and then disagrees with the command line the
  user checks their work with.
- **It costs no dependency.** `deco-scm`'s only one is `thiserror`. Anyone with
  a repository to open already has the binary; nobody asked for libgit2's
  subtree, and the [README](https://github.com/sabas0ba/deco#readme) counts
  deco's crates in public.
- **Absent is a state it can be in.** A missing binary is a feature that is not
  there, which is a thing deco can say plainly.

## How it is read

`git status --porcelain=v2 --branch -z --untracked-files=all`, run as an
argument vector with no shell anywhere near it — a branch called `$(rm -rf ~)`
is a legal branch name.

`--untracked-files=all` rather than git's default of `normal`, which collapses a
new directory into one `? newdir/` record. The count above is one per *file*, and
under the default a folder someone has just added with a dozen files in it would
read as `±1` — an undercount, on one of the commonest things a person does. It
costs a walk into untracked directories; ignored files are still left out, so the
usual `target/` and `node_modules/` are not what is being walked.

`-z` is not a performance choice. Without it, git C-quotes any path containing a
space, a quote or a non-ASCII byte, and separates a rename's two paths with a tab
that a path may legally contain — so a parser would have to undo git's quoting
exactly, and would get it wrong for precisely the files most likely to expose the
mistake. With `-z` every field ends at a NUL and there is no quoting at all.

The parser is pure: hand it the bytes, get back a status, with no process, no
filesystem and no clock involved. A detached head, an unborn branch, a rename, a
merge conflict and a path with spaces in it are each a test with a string
literal in it rather than a repository CI has to build.

Two environment variables go to the child, and each prevents a specific failure:

- `GIT_OPTIONAL_LOCKS=0` — showing a status must never take the index lock. A
  status bar refreshing on save should not be the reason a `git commit` in
  another terminal fails.
- `GIT_TERMINAL_PROMPT=0` — nothing here can answer a question, so anything that
  would ask one has to fail instead of waiting forever for an answer that is not
  coming.

## Not built yet

**Nothing writes to a repository.** No staging, no committing, no checkout.
Reading a repository and changing one are different promises, and the writing
half arrives with the source-control view that gives you somewhere to see what
you are about to do.

**No gutter marks.** Which lines changed is the second stage: a diff of the open
buffer against `HEAD`, computed for the visible rows the way everything else
here is. It has to diff the *unsaved* text, not the file on disk pretending to be
the buffer.

**Nothing over a remote connection.** A remote workspace's root is a path on the
far machine, so running git here against it would either fail or — worse, if a
directory of that name happens to exist locally — report a different
repository's branch as though it were the one being edited. deco leaves it
alone and shows nothing.

**No watcher.** A commit made in a terminal shows up on the next save, not the
moment it happens. The [file tree](files.md#not-built-yet) has the same gap for
the same reason, and one watcher will close both.
