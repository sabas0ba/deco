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
| `ctrl+n` | `explorer.newFile` |
| `ctrl+shift+n` | `explorer.newFolder` |
| `F2` | `renameFile` |
| `delete` | `deleteFile` |
| `ctrl+z` | `undo` — the tree's own, not the text's |
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

## Changing the files themselves

![Creating a file, typing into it, renaming it, and undoing that](img/file-mutations.svg)

`ctrl+n` makes a file, `ctrl+shift+n` a folder, `F2` renames and `delete`
deletes. The same keys mean other things in the text — `F2` renames the *symbol*
under the cursor, `delete` deletes a character — and they are told apart by what
has the keyboard, which is what `sideBarFocus` is for.

**A new file is created where the selection is, and opened.** The directory is
the selected row when that row is a folder, and its parent when it is a file, so
"new file" means "next to this one" without a second question. It then opens and
the keyboard goes with it, the same way `enter` does — the animation types into
the new file immediately after making it.

**What you just made is what is selected.** After a create or a rename the
selection lands on the new name. It matters more than it sounds: the next key
might be `F2` or `delete`, and having those act on whatever happened to be
highlighted before is a destructive kind of surprising.

**A rename moves the tab with the file.** Renaming a file that is open retargets
its tab — the buffer, its unsaved changes and its undo history all stay, because
the file moved and the document did not. Renaming `notes.txt` to `notes.md`
starts highlighting it as Markdown too, unless the language was chosen by hand,
which is the same rule save-as follows.

### The tree has its own undo

`ctrl+z` in the tree takes back the last file operation. `ctrl+z` in the text
takes back characters. They are separate stacks, told apart by focus — as they
are in VS Code, and for the reason that an undo which sometimes moved files
because that was the last thing you did would be unpredictable in both places.

**Deleting cannot be undone**, so it asks first:

```text
delete lexer.rs? this cannot be undone
```

Only a typed `y` goes through; enter on an empty box does not, because that is
what happens when somebody dismisses a prompt they did not read. Undoing a
delete would need the file's bytes and deco has nowhere to keep them, and there
is no trash to move it to either — `files.enableTrash` is one of the settings
deco does not honour. A delete also clears the stack rather than sitting on top
of it, so `ctrl+z` afterwards cannot quietly undo the operation *before* it.

### What can still go wrong

The tree checks what it can before anything touches the disk — that the name is
a name and not a path, that the target is inside the workspace, that nothing is
already called that. The rest can only be found out by trying, and between the
check and the attempt is a window another program can use. So the frontend
refuses to create over a file that appeared in the meantime, and refuses a
rename onto a name that got taken, rather than truncating or replacing. When the
disk says no, the operation comes back off the undo stack — `ctrl+z` must never
offer to undo something that did not happen.

Over a [remote connection](remote.md) these are refused by name: the protocol
reads, writes and lists, and has no create, rename or delete yet. Refusing is
the point — doing it locally would change a file on this machine and report
success about the other one.

## Two kinds of empty

A blank side bar could mean either of two things, so it says which:

```text
reading the workspace…
this workspace is empty
```

## Not built yet

**The tree does not notice changes on disk.** A file created by another program
appears when the directory is read again. There is no watcher; adding one is its
own piece of work with its own failure modes on every platform, and it is worth
doing as itself rather than smuggled in here.

**No mouse, and no drag.** Keyboard only, in both frontends — the GPU frontend's
mouse work has not been done, and a tree that could only be clicked in one of
them would be worse than one that is arrowed in both. Moving a file to another
folder therefore has no gesture: renaming takes a name, not a path, so it cannot
be used to move one either.

**Creating, renaming and deleting are not `WorkspaceEdit`s.** They go through
the same division of labour — the core decides, the frontend touches the disk —
but not the same type: a `WorkspaceEdit` resolves edits *within* files, and a
file that does not exist yet has no text to edit. What the two share is the
thing that mattered, which is that a rename and its tab retarget happen
together.

There is one tree, on one root — the root deco was started in. Opening a second
workspace and switching between them is
[its own chapter](roadmap.md#several-workspaces-switched-between).
