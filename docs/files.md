# The file tree

![Opening the side bar, walking into src/parse, and opening a file with enter](img/file-tree.svg)

The workspace tree lives in the [side bar](chrome.md). `ctrl+b` shows it,
`ctrl+shift+e` puts the keyboard in it, and the arrow keys walk it.

| Key | Command |
| --- | --- |
| `ctrl+b` | `workbench.action.toggleSidebarVisibility` |
| `ctrl+shift+e` | `workbench.files.action.focusFilesExplorer` |
| `↑` `↓` | `list.focusUp`, `list.focusDown` |
| `→` | `list.expand` — opens a folder, or steps into one already open |
| `←` | `list.collapse` — closes a folder, or goes up to the one above |
| `home` `end` | `list.focusFirst`, `list.focusLast` |
| `enter` | `list.select` — opens a file, toggles a folder |
| `escape` | `workbench.action.focusActiveEditorGroup` |
| | `revealInExplorer` — opens the tree onto the file being edited |

`→` and `←` each do two things, which is what VS Code's explorer does and what
makes arrowing through a tree feel like one gesture: right opens the folder you
are on, and if it is already open, moves into it. Left closes it, and on a file
or a closed folder goes up to the parent instead.

**Enter takes the keyboard with it.** Opening a file moves focus into the
editor, because opening something and leaving the caret in the tree would mean a
second keystroke before you could type in what you just asked for. The animation
types `// ` straight after `enter` to show where it lands. Enter on a *folder*
opens it and stays put — there is nothing to move to.

## What it costs to open a big workspace

One `read_dir`. A directory is read when it is first expanded and not before, so
the tree costs what its **visible rows** cost rather than what the workspace
contains — the same bounded-by-the-window rule the lexer, the wrap and the draw
already follow. A folder with ten thousand files in it is one row until you open
it.

That is also why the tree walks down a level at a time when it reveals
something: `revealInExplorer` on `src/parse/lexer.rs` reads `src`, then
`src/parse`, and lands the selection when the row finally exists. Nothing in
between is read.

`files.exclude` hides the same things here as it hides from `ctrl+p`, and the
same conventional skips apply — `.git`, `node_modules`, `target` and friends
never appear. One setting with two meanings would be worse than either meaning.

## Where the reading happens

Not in the tree. `deco-editor`'s `Explorer` holds what it has been *told* a
directory contains, and asks for what it lacks; the frontend answers with
`std::fs`. There is no `read_dir` anywhere in the core, which is what keeps the
whole editable surface — this tree included — testable with no filesystem
attached.

That is not tidiness for its own sake. It is also what makes the tree work on a
**remote** workspace: the same request is answered over the connection instead,
and the model does not know the difference. The first version derives a remote
directory's contents from the whole-workspace listing the protocol already has —
the same listing `ctrl+p` asks for on every press. A per-directory call on the
wire would be cheaper and is a protocol change rather than a local one.

## Two kinds of empty

A blank side bar could mean either of two things, so it says which:

```text
reading the workspace…
this workspace is empty
```

## Not built yet

**Nothing here changes a file.** New file, rename, delete and drag are all
absent. deco has the machinery for them — a `WorkspaceEdit` applies across files
as one undoable step, which is how [rename](language-servers.md) and
[replace across the workspace](find-and-replace.md) already work — and mutations
in the tree should land through it, so renaming an open file and retargeting its
tab are one thing that undoes together. That is the next piece of this chapter,
not a missing foundation.

**The tree does not notice changes on disk.** A file created by another program
appears when the directory is read again. There is no watcher; adding one is its
own piece of work with its own failure modes on every platform, and it is worth
doing as itself rather than smuggled in here.

**No mouse.** Keyboard only, in both frontends — the GPU frontend's mouse work
has not been done, and a tree that could only be clicked in one of them would be
worse than one that is arrowed in both.

There is one tree, on one root — the root deco was started in. Opening a second
workspace and switching between them is
[its own chapter](roadmap.md#several-workspaces-switched-between).
