# Git

![The branch in the status bar, unmoved by typing and refreshed by a save and a new file](img/git-status.svg)

deco reads git the way VS Code does: by running the `git` binary and parsing
what it says. No library is linked. All three stages are built: **the branch and
what differs from it, in the status bar**, **marks beside the lines that
changed**, and **a source-control view** that stages, unstages and commits.

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

## Marks beside the changed lines

![Editing a committed file, and the three marks appearing as it changes](img/git-gutter.svg)

| | |
| --- | --- |
| `┃` | the line is not in the committed file at all |
| `│` | it is there, and says something else |
| `▔` | lines were removed just above this one |

**Shape as well as colour.** VS Code separates added from modified by colour
alone, which is a distinction anyone who cannot tell its green from its blue
does not get. A heavy bar against a light one carries the same thing without
it, and costs nothing. The colours are VS Code's own —
`editorGutter.addedBackground`, `editorGutter.modifiedBackground`,
`editorGutter.deletedBackground` — so a theme that sets them is honoured.

A deletion has nothing left to draw beside, so its mark sits on the **top edge**
of the line that took the removed lines' place. And a run of lines *replaced* is
one modified hunk, not an addition sitting on a deletion — which is what git
reports and what a person means by "I changed this".

**The marks follow the buffer, not the file on disk.** Type into a line and the
mark appears as you type; type it back to what git has and the mark goes. That
is the point of diffing in process: a gutter that waited for a save would be
describing the file rather than the screen.

Which is also why the two halves are fetched and computed separately. The
committed text costs a process and only changes when someone commits — so
`git status`'s commit id is watched, and when it moves the cached text is thrown
away, which is what makes a `git commit` in another terminal clear the gutter.
The comparison itself is pure, so it is redone as the file is typed into. One
`git` per commit; none per keystroke.

The GPU frontend works the marks out but does not paint them yet — the same
state its selection and current-line rectangles are in. When it grows the rest
of its drawing they are already the same answer the terminal shows.

## The source-control view

![Opening the source-control view, staging both files and committing them](img/git-view.svg)

`ctrl+shift+g` opens the side bar's other tenant, switches to it and gives it
the keyboard — one key to reach a thing you mean to act on, which is what VS
Code's `workbench.view.*` do. `ctrl+shift+e` goes back to the
[file tree](files.md).

Rows are grouped by what you would *do* about them, in the order they have to
be dealt with: **Merge Changes** first because a conflict blocks everything
else, then **Staged Changes**, **Changes**, **Untracked**. The letter beside
each name is git's own — `M`, `A`, `D`, `R`, `U` for a conflict, `?` for
something git has never been told about.

**A file can appear twice.** Staged, and modified again since — two rows, under
two headings, because unstaging the first and staging the second do opposite
things to the same file. The status bar's `±` count deliberately does not do
this: it answers "how many files need thinking about", and counting one twice
there would make the bar disagree with itself.

**The selection follows the file, not the row number.** Staging something
reorders the list; an index that stayed put would leave the selection on a
different file than the one you were looking at, and the next command would act
on it.

`enter` opens a read-only, side-by-side diff and the keyboard goes with it.
Staged rows compare **HEAD ↔ Index**; Changes and Untracked compare
**Index ↔ Working Tree**. That boundary matters when the same file appears
twice: the staged row shows only what the next commit records, while the other
row shows only what staging again would add. `ctrl+w` closes the diff and
returns to the tab underneath; `ctrl+1` and `ctrl+2` move between its sides.

![Opening a modified row, moving between the aligned diff panes, and closing the comparison](img/git-diff.svg)

Added, removed and modified rows have distinct gutter marks, and alignment gaps
keep the two sides on the same screen row. Changed rows also carry a thin tint
across their full width: removed content uses
`diffEditor.removedLineBackground`, and inserted content uses
`diffEditor.insertedLineBackground`. Translucent theme colours are composited
over the editor background for terminals. The comparison is fetched on a worker,
so opening one does not stop the editor from painting. Merge-conflict rows are
refused for now: presenting their unresolved stages as an ordinary two-way diff
would hide the part that must be resolved.

### The commands

VS Code has no default key for most of these: its view is driven by the buttons
on each row, and deco does not have buttons and will not invent keys VS Code has
not. So they live in the command palette (`ctrl+shift+p`), which is how the
animation above reaches them.

| Command | What it does |
| --- | --- |
| `workbench.view.scm` | `ctrl+shift+g` — show the view and focus it |
| `git.stage` | add the selected file's working-tree state to the index |
| `git.stageAll` | add everything git reported |
| `git.unstage` | take the selected file back out of the index |
| `git.commit` | `ctrl+enter` — ask for a message, then record what is staged |
| `git.refresh` | ask git again |

**Every refusal happens before a process starts.** Staging something already
staged would succeed and change nothing, and a message claiming otherwise is
worse than one saying it could not. Committing with nothing staged never opens
the message box at all — asking someone to write a commit message and *then*
telling them there was nothing to commit is how a message gets lost.

**The commit runs your hooks.** A `pre-commit` that reformats or refuses is
yours, and inheriting it is the whole argument for shelling out rather than
linking a library. Their stdin is closed and `GIT_TERMINAL_PROMPT` is `0`, so
Git's own terminal prompt is disabled and a hook reading stdin gets EOF. A hook
is still an arbitrary program: it can open `/dev/tty`, show a graphical prompt
or run for a long time, and deco does not sandbox or bypass that behaviour.

### What it deliberately will not do

**Discard.** `git clean` and `git checkout --` throw away work with no undo and
no trash, which is the same thing the [tree's delete](files.md) refuses to do
quietly. Not built, rather than built without a way back.

**Reach the network.** No push, pull or fetch. Those need credentials, and a
credential prompt is a thing an editor has to be trusted with; reading and
staging need neither.

## When it runs

This is the part worth being careful about, because the alternative is a process
per keystroke.

`git status` runs when something has happened that it would report differently:
a **save**, a file **created, renamed or deleted** from the tree, and once at
startup. Typing does not run it, which is what the first animation shows — the
file is edited and the bar does not move until `ctrl+s`. The *marks*, being
pure, are a different matter: they keep up with every keystroke.

It runs **on a thread**. On deco's own checkout `git status` is a few
milliseconds; on a working tree with a million files it is not, and an editor
that stopped painting while git thought would be worse than one whose branch
name is a moment stale.

In a **remote session**, status, committed text, diff comparisons, stage,
unstage and commit run through a second server connection on the machine
holding the repository. That connection has one worker and one request in
flight: a slow status or commit hook neither blocks the terminal loop nor races
another repository write. The ordinary connection remains available for file
reads and extension requests.

The remote server does not expand its authority to find a repository. If the
served workspace is only a subdirectory and the repository begins above it,
source control is refused; restart with the repository root as `--workspace`.

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
| `git.path` | Where `git` is for a local session. Empty or unset means whatever `PATH` finds. A remote session uses `git` from the server's `PATH`; allowing an untrusted remote setting to choose a program requires the same consent model as a language server and is not implemented |
| `git.decorations.enabled` | The gutter marks. Turning it off stops the committed text being fetched as well, not just the marks being drawn — the branch stays in the status bar |

## Why the binary and not a library

The same three reasons VS Code has:

- **It inherits the user's git.** Their `includeIf` config, their
  `credential.helper`, their hooks, their `core.fsmonitor`. A library
  reimplements a subset of that and then disagrees with the command line the
  user checks their work with.
- **It costs no Git implementation dependency.** `deco-scm` uses `thiserror`
  for its errors and `serde` to carry status and operations over deco's own
  remote protocol. Anyone with a repository to open already has the binary;
  nobody asked for libgit2's subtree, and the
  [README](https://github.com/sabas0ba/deco#readme) counts deco's crates in
  public.
- **Absent is a state it can be in.** A missing binary is a feature that is not
  there, which is a thing deco can say plainly.

## How it is read

`git status --porcelain=v2 --branch -z --untracked-files=all` for the bar, and
`git show HEAD:<path>` for the committed text behind the marks. Both run as an
argument vector with no shell anywhere near them — a branch called
`$(rm -rf ~)` is a legal branch name.

**Every path is relative to the repository**, which is what git reports in and
answers about. Not to the folder deco was started in: opening a subdirectory of
a repository is an ordinary thing to do, and the two coordinate systems
disagree the moment somebody does. `git rev-parse --show-toplevel` is asked once
so they cannot drift. (`HEAD:./a` would be the other thing — resolved against
the working directory — and a gutter drawn from the wrong blob looks exactly
like one drawn from the right blob.)

`git show` is deliberately run **without** `--textconv`: a repository can
configure a filter that runs an arbitrary program to render a file, and a
gutter is not worth executing someone's `.gitattributes` for.

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

The diff is Myers' — the algorithm `git diff` itself uses. Its common prefix and
suffix come off before the search starts, so an edit in a thousand-line file
costs what the edit is worth rather than what the file is; and the search gives
up after two thousand edits, because a file replaced wholesale has no gutter
worth drawing. Past that the middle becomes one modified block and says so,
rather than the marks quietly being approximate.

The status parser is pure: hand it the bytes, get back a status, with no
process, no filesystem and no clock involved. A detached head, an unborn branch, a rename, a
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

**No checkout, and no branch list.** Switching branches can lose uncommitted
work when it goes wrong, and doing it well needs to say what would be lost
before it happens. That is its own piece of work rather than a line in this
one.

**No watcher.** A commit made in a terminal shows up on the next save, not the
moment it happens. The [file tree](files.md#not-built-yet) has the same gap for
the same reason, and one watcher will close both.
