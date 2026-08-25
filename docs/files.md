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

**A rename moves the tabs with the file.** Renaming a file that is open
retargets its tab — the buffer, its unsaved changes and its undo history all
stay, because the file moved and the document did not. Renaming a *directory*
retargets every tab inside it, since that is what the rename did on disk.
Renaming `notes.txt` to `notes.md` starts highlighting it as Markdown too,
whether or not that tab is the one on screen, and unless the language was chosen
by hand — the same rule save-as follows.

### The tree has its own undo

`ctrl+z` in the tree takes back the last file operation. `ctrl+z` in the text
takes back characters. They are separate stacks, told apart by focus — as they
are in VS Code, and for the reason that an undo which sometimes moved files
because that was the last thing you did would be unpredictable in both places.
That holds even when the document has a [workspace edit](find-and-replace.md)
waiting to be undone: in the tree, `ctrl+z` is the tree's.

**Undoing a create only removes what the create made.** If you made a file and
have since typed in it and saved, `ctrl+z` in the tree refuses rather than taking
the file and its contents with it:

```text
could not deleted parse.rs: parse.rs has been written to since it was
created — delete it yourself if that is what you meant
```

The same for a folder that has gained anything. Undoing a create at that point is
not undoing anything; it is deleting work, and it would do it without the
confirmation an ordinary delete asks for.

**Deleting cannot be undone**, so it asks first:

```text
delete lexer.rs? this cannot be undone
```

Only a typed `y` goes through; enter on an empty box does not, because that is
what happens when somebody dismisses a prompt they did not read. Undoing a
delete would need the file's bytes and deco has nowhere to keep them, and there
is no trash to move it to either — `files.enableTrash` is one of the settings
deco does not honour. A delete also clears the stack rather than sitting on top
of it, so `ctrl+z` afterwards cannot quietly undo the operation *before* it —
and it clears it only once the delete has actually happened, so a delete the
filesystem refuses costs you nothing.

### What can still go wrong

The tree checks what it can before anything touches the disk — that the name is
a name and not a path, that the target is inside the workspace, that nothing is
already called that. The rest can only be found out by trying, and between the
check and the attempt is a window another program can use. So the frontend
refuses to create over a file that appeared in the meantime, and refuses a
rename onto a name that got taken, rather than truncating or replacing. When the
disk says no, the operation comes back off the undo stack — `ctrl+z` must never
offer to undo something that did not happen, and an undo that failed stays there
to be tried again.

Creating is safe against that window: `create_new` is one operation, and the
filesystem is what refuses. **Renaming is not.** The check that the target is
free and the rename itself are two calls, and a file appearing between them is
replaced rather than refused. Closing it needs a no-replace rename, which the
standard library does not offer on any platform — it means `renameat2` on Linux,
`renamex_np` on macOS and `MoveFileEx` on Windows, three pieces of unsafe
platform code with a runtime fallback each, in a codebase with one `unsafe` in
it. That is worth doing as its own change rather than being smuggled in here, so
for now the window is named rather than closed.

**A delete removes what the tree was showing**, not what is on disk when the
frontend gets there. The confirmation names a file or a folder, and that is what
is carried out — so a file replaced by a directory since the tree last read it is
refused rather than recursively deleted, which is what asking the disk instead
would have done. The tree has no watcher, so its picture really can be stale.

Deleting a file that is open lets that tab go rather than closing it: the buffer
stays, its path is dropped, and the status line says so. The text is still
yours — where it should live is a question only you can answer. Its diagnostics
go with the file, and the language server is told the document is closed.

That holds when a delete only *partly* works, too. Removing a directory can take
some of it and then stop — on an entry that is locked, or one another program
made underneath. The half that went is as gone as if it had all worked, so the
tree re-reads the directory **and everything below it**, and each tab is checked
against the disk one file at a time rather than the whole subtree being let go or
none of it. The tree's undo history goes as well, for the same reason a
completed delete clears it: something irreversible happened, and the entries
below it describe a state that never existed.

Only a *recursive* delete can half happen. Removing one file, or one empty
directory, either worked or did not — so a refusal there costs no undo history.
Nor does every recursive failure: a directory that was already gone, or that
turned out not to be a directory, failed before touching anything, and the
history survives. Any other failure could have taken part of the tree, and the
barrier goes up — an undo over a state that never existed is worse than an undo
you no longer have.

A tab is let go only when the disk says its file is **definitely** gone. The
permission problem that stopped a delete can also stop the check, and a file that
merely cannot be looked at is not a file that has been removed.

A rename can take a file's language away — `main.rs` to `main.txt`. What the
language server said goes with it, since nothing would ever replace it: no
server runs for a file with no language, so the old squiggles and highlighting
would otherwise sit there for good.

A rename that only changes capitalisation works on a case-insensitive
filesystem, where `Foo.rs` and `foo.rs` are the same file: the check asks whether
the target is *a different file*, not whether the name is taken. It asks about
the directory entry rather than what it leads to, so a symlink pointing at
nothing still counts as something in the way — it would otherwise be invisible
from both sides, since the tree lists only real files and directories.

Accepting the rename prompt without editing it does nothing at all, compared
before any tidying of the text: on a filesystem that allows a name like
`" report "`, trimming first would make pressing enter a rename nobody asked
for.

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
