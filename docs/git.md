# Git

![The branch in the status bar, unmoved by typing and refreshed by a save and a new file](img/git-status.svg)

deco reads git the same way VS Code does: it runs the `git` binary and parses its output. No library is linked. All three stages are built: **the branch and the changes relative to it, in the status bar**, **marks beside changed lines**, and **a source-control view** that stages, unstages and commits. Local branches can also be listed and switched after a preflight confirmation.

```
main ±2 ↑2
```

Each marker is omitted when its count is zero, as with the problem counts. A permanent `0 changed` would add noise; the absence of a marker means zero.

| | |
| --- | --- |
| `main` | the branch, or a seven-character commit id when `HEAD` is detached |
| `±2` | two files differ from `HEAD` — staged, unstaged or untracked |
| `↑2 ↓1` | two commits to push, one to pull; only when the branch tracks another |
| `!1` | one file a merge left conflicted, which has to be dealt with first |

**One per file, not one per side.** A file that is staged *and* modified since counts once, because the count is the number of files that differ from `HEAD`. The [source-control view](#the-source-control-view) shows such a file as two rows, one per side; see below.

## Marks beside the changed lines

![Editing a committed file, and the three marks appearing as it changes](img/git-gutter.svg)

| | |
| --- | --- |
| `┃` | the line is not in the committed file at all |
| `│` | it is there, and says something else |
| `▔` | lines were removed just above this one |

**Shape as well as colour.** VS Code distinguishes added from modified lines only by colour, so users who cannot tell its green from its blue lose the distinction. A heavy bar and a light bar carry the same distinction without relying on colour. The colours are VS Code's: `editorGutter.addedBackground`, `editorGutter.modifiedBackground` and `editorGutter.deletedBackground`, so a theme that sets them is applied.

A deletion has no remaining line to mark, so its mark is drawn on the **top edge** of the line that now follows the removed lines. A run of *replaced* lines is one modified hunk, not an addition next to a deletion. This matches git's output and what the user changed.

**The marks follow the buffer, not the file on disk.** Editing a line shows its mark immediately; restoring the line to the committed text removes the mark. This is why the diff runs in process: a gutter updated only on save would describe the file, not the screen.

The committed text and the comparison are therefore handled separately. Fetching the committed text requires a process, and the text changes only on commit. deco watches the commit id reported by `git status` and discards the cached text when it changes, so a `git commit` in another terminal clears the gutter. The comparison is pure, so it is recomputed as you type. This means one `git` process per commit and none per keystroke.

The GPU frontend computes the marks but does not draw them yet, as with its selection and current-line rectangles. When the remaining drawing is implemented, it will show the same marks as the terminal frontend.

## The source-control view

![Opening the source-control view, staging both files and committing them](img/git-view.svg)

`ctrl+shift+g` opens and focuses the source-control view in the side bar. `ctrl+shift+e` switches focus to the [file tree](files.md).

Rows are grouped by the action they need, in the order they must be handled: **Merge Changes** first because a conflict blocks other operations, then **Staged Changes**, **Changes** and **Untracked**. The letter beside each name is git's status code: `M`, `A`, `D`, `R`, `U` for a conflict, and `?` for an untracked file.

**A file can appear twice.** A file that is staged and modified again since appears as two rows under two headings, because unstaging the first and staging the second act on the same file in opposite directions. The status bar's `±` count deliberately counts the file once: it reports how many files need attention, and counting a file twice would contradict that meaning.

**The selection follows the file, not the row number.** Staging reorders the list. If the selection kept its row index, it would move to a different file, and the next command would act on that file.

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

VS Code has no default key for most of these commands; its view uses buttons on each row. deco has no buttons and does not add key bindings that VS Code does not have. The commands are therefore available in the command palette (`ctrl+shift+p`), as used in the animation above.

| Command | What it does |
| --- | --- |
| `workbench.view.scm` | `ctrl+shift+g` — show the view and focus it |
| `git.stage` | add the selected file's working-tree state to the index |
| `git.stageAll` | add everything git reported |
| `git.unstage` | take the selected file back out of the index |
| `git.commit` | `ctrl+enter` — ask for a message, then record what is staged |
| `git.checkout` | list local branches, preview the switch, then ask for confirmation |
| `git.refresh` | ask git again |

**Every refusal happens before a process starts.** Staging a file that is already staged would succeed without changing anything, so deco reports that it could not stage the file instead of reporting a change. Committing with nothing staged does not open the message box, so you do not write a commit message only to learn that there was nothing to commit.

**The commit runs your hooks.** A `pre-commit` hook that reformats or rejects a commit is part of your setup, and running it is the main reason to use the git binary instead of a library. Their stdin is closed and `GIT_TERMINAL_PROMPT` is `0`, so
Git's own terminal prompt is disabled and a hook reading stdin gets EOF. A hook
is still an arbitrary program: it can open `/dev/tty`, show a graphical prompt
or run for a long time, and deco does not sandbox or bypass that behaviour.

### Switching branches

![Choosing a local branch, reviewing the preflight and switching without discarding an untracked file](img/git-checkout.svg)

`git.checkout` first lists existing **local** branches. Remote-tracking names are not included, because choosing one would also create a local branch, which is a separate decision. After a branch is selected, deco asks Git how
many committed paths differ and counts the staged, unstaged and untracked work
that would have to come along.

The confirmation selects **Cancel** by default and says plainly that no local
work will be discarded. The actual command has no force flag; if a tracked or
untracked file would be overwritten, Git refuses the switch and the current
branch remains in place. Merge conflicts are refused before confirmation.

Unsaved editor buffers are a separate boundary because Git cannot see them.
Checkout is refused while any tab has unsaved text. After a successful switch,
every clean open file is re-read and its old undo history is dropped so a later
save or undo cannot put the previous branch's contents back. If a file is absent
on the target branch, its old text is detached into an unsaved tab rather than
silently closed.

### What it deliberately will not do

**Discard.** `git clean` and `git checkout --` discard work with no undo and no trash. For the same reason, the [tree's delete](files.md) does not delete without confirmation. Discard is not built, rather than built without a way to recover.

**Reach the network.** No push, pull or fetch. These require credentials, and deco would have to be trusted to handle credential prompts. Reading and staging need neither.

## When it runs

Scheduling needs care, because a naive implementation would start a process per keystroke.

`git status` runs when its output may have changed: after a **save**, after a file is **created, renamed or deleted** in the tree, and once at startup. Typing does not run it, as the first animation shows: the file is edited and the status bar does not change until `ctrl+s`. The *marks* are pure computations and update on every keystroke.

It runs **on a separate thread**. On deco's own checkout `git status` takes a few milliseconds; on a working tree with a million files it takes much longer. A briefly outdated branch name is preferable to an editor that stops drawing while git runs.

In a **remote session**, status, committed text, diff comparisons, branch
preflight, checkout, stage, unstage and commit run through a second server connection on the machine
holding the repository. That connection has one worker and one request in
flight: a slow status or commit hook neither blocks the terminal loop nor races
another repository write. The ordinary connection remains available for file
reads and extension requests.

The remote server does not expand its authority to find a repository. If the
served workspace is only a subdirectory and the repository begins above it,
source control is refused; restart with the repository root as `--workspace`.

**One run at a time, without losing requests.** The pending-request flag is cleared when a run *starts*, not when it returns. A save made while git is still running sets the flag again, the earlier result does not clear it, and another run follows. Clearing the flag when the result arrived would lose that save, and the status bar would stay wrong until the next one.

## When there is nothing to show

Three situations look identical on screen. All three show nothing: no branch, and no empty space where it would be:

- **No git on this machine.** The feature is *absent*, not broken.
- **The folder is not a repository.** This is normal and does not need a status bar entry.
- **No result yet**, before the first run completes.

The first two are remembered for the session and not checked again, because they will still be true after the next save and spawning a process to re-check them has no benefit. Other failures, such as git refusing because a rebase is in progress or an index lock held by a command in a terminal, are transient, so the next save retries.

The reason is stored even though there is nowhere to display it yet. When the [panel](chrome.md) has an output view, the reason will be shown there. Until then, the frontend can read it and a test asserts it.

## Settings

| Setting | What deco does with it |
| --- | --- |
| `git.enabled` | VS Code's meaning and VS Code's default of `true`. Turning it off stops the process being spawned *and* takes the segment off the bar — a setting that only hid the result would still be paying for it |
| `git.path` | Where `git` is for a local session. Empty or unset means whatever `PATH` finds. A remote session uses `git` from the server's `PATH`; allowing an untrusted remote setting to choose a program requires the same consent model as a language server and is not implemented |
| `git.decorations.enabled` | The gutter marks. Turning it off stops the committed text being fetched as well, not just the marks being drawn — the branch stays in the status bar |

## Why the binary and not a library

The same three reasons VS Code has:

- **It uses the user's git configuration.** This includes their `includeIf` config, `credential.helper`, hooks and `core.fsmonitor`. A library implements only a subset of this and can then disagree with the command line the user checks their work with.
- **It needs no Git implementation dependency.** `deco-scm` uses `thiserror` for its errors and `serde` to carry status and operations over deco's own remote protocol. Anyone opening a repository already has the binary, libgit2 would add its own dependency subtree, and the [README](https://github.com/sabas0ba/deco#readme) publishes deco's crate count.
- **A missing binary is a supported state.** If git is not installed, the feature is unavailable, and deco can report that directly.

## How it is read

`git status --porcelain=v2 --branch -z --untracked-files=all` for the bar, and `git show HEAD:<path>` for the committed text behind the marks. Both run as an argument vector without a shell, which matters because `$(rm -rf ~)` is a legal branch name.

**Every path is relative to the repository root**, which is how git reports and accepts paths. Paths are not relative to the folder deco was started in: opening a subdirectory of a repository is common, and the two coordinate systems differ in that case. `git rev-parse --show-toplevel` is run once so that they stay consistent. (`HEAD:./a` would instead resolve against the working directory, and a gutter computed from the wrong blob looks the same as a correct one.)

`git show` is deliberately run **without** `--textconv`. A repository can configure a filter that runs an arbitrary program to render a file, and the gutter does not justify running programs configured in `.gitattributes`.

`--untracked-files=all` is used instead of git's default `normal`, which reports a new directory as one `? newdir/` record. The count above is per *file*, so with the default a newly added folder containing a dozen files would count as `±1`, which undercounts a common operation. This requires walking untracked directories, but ignored files are still skipped, so directories such as `target/` and `node_modules/` are not walked.

`-z` is used for correctness, not performance. Without it, git C-quotes any path containing a space, a quote or a non-ASCII byte, and separates a rename's two paths with a tab, which a path may also contain. A parser would have to reverse git's quoting exactly, and any mistake would affect exactly the unusual paths that expose it. With `-z`, every field ends with a NUL and there is no quoting.

The diff uses Myers' algorithm, as `git diff` does. The common prefix and suffix are removed before the search starts, so the cost of an edit in a thousand-line file depends on the size of the edit rather than the size of the file. The search stops after two thousand edits, because a file that has been replaced entirely has no useful gutter. Beyond that limit, the middle section becomes one modified block and this is reported, instead of showing approximate marks.

The status parser is pure: it takes bytes and returns a status, without a process, filesystem or clock. A detached head, an unborn branch, a rename, a merge conflict and a path with spaces are each tested with a string literal instead of a repository built in CI.

Two environment variables are set for the child process, each preventing a specific failure:

- `GIT_OPTIONAL_LOCKS=0`: reading status must never take the index lock, so a status refresh on save cannot make a `git commit` in another terminal fail.
- `GIT_TERMINAL_PROMPT=0`: deco cannot answer prompts, so an operation that would prompt fails instead of waiting indefinitely.

## Not built yet

**No watcher.** A commit made in a terminal appears on the next save, not immediately. The [file tree](files.md#not-built-yet) has the same limitation for the same reason, and one watcher will address both.
