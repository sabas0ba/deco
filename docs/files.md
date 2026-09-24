# The file tree

![Opening the side bar, walking into src/parse, and opening a file with enter](img/file-tree.svg)

The workspace tree is shown in the [side bar](chrome.md). `ctrl+b` shows it, `ctrl+shift+e` moves keyboard focus to it, and the arrow keys navigate it.

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

`→` and `←` each have two actions, as in VS Code's explorer. Right expands the selected folder, or moves into it if it is already expanded. Left collapses the folder, or moves to the parent when the selection is a file or a collapsed folder.

**Enter moves focus to the editor.** Opening a file moves keyboard focus into the editor, so you can type without another keystroke. The animation types `// ` immediately after `enter` to show this. Enter on a *folder* opens it and keeps focus in the tree.

## What it costs to open a big workspace

Opening the workspace costs one `read_dir`. A directory is read only when it is first expanded, so the cost depends on the tree's **visible rows**, not on the workspace size. The lexer, wrapping and drawing follow the same window-bounded rule. A folder with ten thousand files is one row until you expand it.

For the same reason, the tree reads one level at a time when revealing a file. `revealInExplorer` on `src/parse/lexer.rs` reads `src`, then `src/parse`, and selects the row when it exists. No other directories are read.

`files.exclude` hides the same entries here as in `ctrl+p`, and the same fixed skips apply, so `.git`, `node_modules`, `target` and similar directories never appear. The setting has one meaning in both places.

## Where the reading happens

The tree does not read the filesystem. `deco-editor`'s `Explorer` stores the directory contents it has received and requests missing ones; the frontend provides them using `std::fs`. The core contains no `read_dir`, so the editing code, including this tree, can be tested without a filesystem.

The same design lets the tree work on a **remote** workspace: the request is answered over the connection instead, and the model is unchanged. The first version derives a remote directory's contents from the whole-workspace listing that the protocol already provides, which is the same listing `ctrl+p` requests on every press. A per-directory request would be cheaper, but requires a protocol change.

## Changing the files themselves

![Creating a file, typing into it, renaming it, and undoing that](img/file-mutations.svg)

`ctrl+n` creates a file, `ctrl+shift+n` creates a folder, `F2` renames and `delete` deletes. The same keys have other meanings in the text: `F2` renames the *symbol* under the cursor, and `delete` deletes a character. Keyboard focus, exposed as `sideBarFocus`, selects the meaning.

**A new file is created at the selection and opened.** It is created in the selected folder, or in the selected file's parent directory. The new file then opens and receives keyboard focus, as with `enter`. The animation types into the new file immediately after creating it.

**The new entry is selected.** After a create or rename, the selection moves to the new name, so a following `F2` or `delete` acts on that entry rather than on the previously selected one.

**A rename moves the tabs with the file.** Renaming an open file retargets its tab. The buffer, its unsaved changes and its undo history are kept. Renaming a *directory* retargets every tab inside it. Renaming `notes.txt` to `notes.md` also switches highlighting to Markdown, whether or not that tab is visible, unless the language was chosen manually. Save-as follows the same rule.

### The tree has its own undo

`ctrl+z` in the tree undoes the last file operation. `ctrl+z` in the text undoes text edits. The two undo stacks are separate and selected by focus, as in VS Code, so undo in the text never moves files. This also applies when the document has a [workspace edit](find-and-replace.md) to undo: in the tree, `ctrl+z` uses the tree's stack.

**Undoing a rename checks that the file is still the one that was moved.** Another program can move the renamed file away and put a different file at that path. Undoing by path alone would then move that other file back and point your buffer at it. Size and modification time are recorded at the rename and checked at undo. This is a heuristic rather than proof of identity: a real identity is an inode on Unix and a file index on Windows, and the standard library provides the latter only behind an unstable feature. On a mismatch, the undo is refused.

**Undoing a create removes only the created entry**, and only if it is unchanged. An empty file or folder cannot be identified by being empty, because another program can replace it with a *different* empty entry at the same path. The same size and time check used for rename undo is recorded when the create succeeds, and a mismatch is refused. If you created a file and have since typed in it and saved, `ctrl+z` in the tree refuses rather than deleting the file and its contents:

```text
could not deleted parse.rs: parse.rs has been written to since it was
created — delete it yourself if that is what you meant
```

The same applies to a folder that has gained any entries. Undoing the create at that point would delete work without the confirmation that an ordinary delete requires.

**Deleting cannot be undone**, so it asks for confirmation:

```text
delete lexer.rs? this cannot be undone
```

Only a typed `y` confirms the delete. Enter on an empty prompt does not, so dismissing the prompt without reading it deletes nothing. Undoing a delete would require storing the file's bytes, which deco does not do, and there is no trash support because deco does not honour `files.enableTrash`. A delete clears the tree's undo stack, so a later `ctrl+z` cannot undo the operation *before* the delete. The stack is cleared only after the delete succeeds, so a delete refused by the filesystem leaves the stack unchanged.

### What can still go wrong

Before touching the disk, the tree checks that the name is a name and not a path, that the target is inside the workspace, and that no entry with that name exists. Other conditions can only be detected by attempting the operation, and another program can change the filesystem between the check and the attempt. The frontend therefore refuses to create over a file that appeared in the meantime and refuses to rename onto a name that has been taken, rather than truncating or replacing. When the filesystem returns an error, the operation is removed from the undo stack so that `ctrl+z` never offers to undo an operation that did not happen. A failed undo stays on the stack so it can be retried.

**The resolved location of a path is checked, not only its spelling.** The tree has no filesystem and can only compare path strings. A directory replaced by a symlink after it was listed resolves elsewhere, and filesystem calls follow the link, so `New File` in a `src` that is now a link would write to the link target. Before anything is created, renamed or removed, the frontend resolves the containing directory and requires it to be inside the workspace.

Creating is not affected by the race between check and operation, because `create_new` is a single operation and the filesystem refuses an existing target. **Renaming is affected.** The check that the target is free and the rename are two calls, and a file created between them is replaced rather than refused. Preventing this requires a no-replace rename, which the standard library does not offer on any platform. It would require `renameat2` on Linux, `renamex_np` on macOS and `MoveFileEx` on Windows: three pieces of unsafe platform code, each with a runtime fallback, in a codebase that currently has one `unsafe` block. A no-replace rename is not implemented, so this race remains possible.

The symlink check has a similar race: a directory can be replaced between the check and the call. Preventing that requires `openat` with `O_NOFOLLOW` and a handle per path component, which the standard library also does not provide on any platform. The check covers the common case of an existing link in the workspace, for example one created by a build or a package manager. It does not prevent an attacker who replaces a directory between the two calls.

**Rename and delete act on the entry type the tree was showing**, not on what is on disk when the frontend performs the operation. A folder replaced by a file since it was read is refused rather than moved as a folder, which would retarget every tab below the old path to a regular file.

Delete follows the same rule. The confirmation names a file or a folder, and only that type is deleted. A file replaced by a directory since the tree last read it is refused rather than deleted recursively. The tree has no filesystem watcher, so its view can be out of date.

Deleting an open file detaches its tab rather than closing it. The buffer is kept, its path is cleared, and the status line reports this, so you can save the text elsewhere. The file's diagnostics are removed, and the language server is told that the document is closed.

The same applies when a delete *partly* succeeds. Removing a directory can delete some entries and then stop on an entry that is locked or was created by another program. The deleted entries are gone, so the tree re-reads the directory **and everything below it**, and each tab is checked against the disk individually rather than detaching all or none of the tabs in the subtree. The tree's undo history is also cleared, as after a completed delete, because an irreversible change occurred and the older entries no longer match the filesystem.

Only a *recursive* delete can partly succeed. Removing one file or one empty directory either succeeds or fails, so a refusal there keeps the undo history. A failed recursive delete clears the history only if something **was actually removed**: the tree checks the entries it knew about, and the files held by tabs, against the disk. A permission error on the directory itself stops the delete before any entry is removed, and the history is kept.

The check covers only directories that have been read, which are the directories that were expanded. A file removed from a collapsed directory is not detected, because detecting it would require walking the whole workspace.

When a directory is deleted, the tree's stored state for it is also removed. Otherwise a new directory created later with the same name would show the deleted directory's contents, already expanded. A **rename** keeps this state because only the name changed, so a renamed folder stays expanded with its rows intact.

A tab is detached only when the disk reports that its file is **definitely** gone. The permission problem that stopped a delete can also stop the check, and a file that cannot be inspected has not necessarily been removed.

A rename can remove a file's language, for example `main.rs` to `main.txt`. The language server's results for the file are then removed, because no server runs for a file without a language and the old diagnostics and highlighting would otherwise never be replaced.

A rename that only changes capitalisation works on a case-insensitive filesystem, where `Foo.rs` and `foo.rs` are the same file. The check tests whether the target is *a different file*, not whether the name exists. It inspects the directory entry rather than its target, so a dangling symlink still counts as an existing entry. Otherwise it would be visible in neither check, because the tree lists only regular files and directories.

Accepting the rename prompt without editing it does nothing. The comparison is made before the text is trimmed, so on a filesystem that allows a name such as `" report "`, pressing enter does not rename the file.

Over a [remote connection](remote.md), these operations are refused with a message naming the operation. The protocol supports reading, writing and listing, but not create, rename or delete yet. Performing the operation locally would change a file on the local machine while reporting success for the remote one.

## Two kinds of empty

An empty side bar can have two causes, so it shows which one applies:

```text
reading the workspace…
this workspace is empty
```

## Not built yet

**The tree does not detect changes on disk.** A file created by another program appears when the directory is read again. There is no filesystem watcher. A watcher has platform-specific failure modes and should be implemented as a separate change.

**No mouse, and no drag.** The tree is keyboard-only in both frontends, because mouse support has not been implemented in the GPU frontend. Moving a file to another folder is therefore not possible: rename accepts a name, not a path.

**Creating, renaming and deleting are not `WorkspaceEdit`s.** They use the same division of responsibility, where the core decides and the frontend accesses the disk, but not the same type. A `WorkspaceEdit` describes edits *within* files, and a file that does not exist yet has no text to edit. Like a `WorkspaceEdit`, a rename updates the file and its tab together.

There is one tree with one root: the root deco was started in. Opening a second workspace and switching between workspaces is covered in [the roadmap](roadmap.md#several-workspaces-switched-between).
