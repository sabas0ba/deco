# Testing

deco uses unit tests for individual components and end-to-end scenarios for interactions between components.

**Unit tests** are next to the code in each crate, and there are about 1,700 of them. Each builds the struct under test, calls the function under test, and asserts on the result. Most of the confidence in this codebase comes from them: they check, for example, that `deco-keymap` resolves a chord, `deco-config` layers a `settings.json`, and `deco_tui::render` lays out a session correctly.

**End-to-end scenarios** are in [`crates/deco-e2e`](../crates/deco-e2e), and there are about a hundred and twenty. They start the editor on a machine that the test set up, press keys, and check the screen and the disk. Editor defects in practice are rarely one function returning a wrong value. More often, a `settings.json` is read from the wrong directory, a keybinding resolves but never reaches its command, a file is saved to an unintended path, or a frame is drawn taller than the terminal. In these cases each component is correct, but the editor as a whole is not.

```console
$ cargo test --workspace          # both, and what CI runs
$ cargo test -p deco-e2e          # just the scenarios
$ cargo xtask ci                  # fmt, clippy, rustdoc and all of it
```

## What a scenario is made of

```rust
use deco_e2e::Scenario;

#[test]
fn a_workspace_settings_file_beats_the_users_own() {
    let scenario = Scenario::new("workspace-layer")
        .user_settings(r#"{ "editor.tabSize": 8, "editor.insertSpaces": true }"#)
        .workspace_settings(r#"{ "editor.tabSize": 2 }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("tab");
    assert_eq!(editor.text(), "  x\n");
}
```

Scenarios exercise four parts of the editor:

- **The configuration directory.** `user_settings`, `vscode_settings`, `workspace_settings` and `user_keybindings` write JSON to a temporary home directory in the platform's actual layout, and the session is built by `deco::startup::session`, the same call the binary makes. No test code passes a pre-built `Settings`.
- **The workspace.** `file` writes files to disk. Quick open lists them, search-in-files searches them, saving overwrites them, and `editor.on_disk(…)` returns what `cat` would show.
- **The keystrokes.** `press("ctrl+shift+p")` builds the crossterm `KeyEvent` a terminal would send, passes it to `deco_tui::keys::chord_from_event`, and then to `deco_tui::Driver`, the editor's own event loop without the terminal. A scenario can run a command only by pressing keys bound to it.
- **The screen.** `editor.screen()` renders a frame at the terminal size the scenario requested and applies the same substitution that `paint` applies before output to a terminal. `assert_shows`, `assert_status` and `assert_fits` print the whole screen, framed, when they fail, because when an expected string is missing, the useful information is what is shown instead.

## What it deliberately does not do

- **No terminal.** These tests do not verify that crossterm writes what is queued. `paint` has its own unit tests for that.
- **No process environment.** The process environment is shared by all test threads, so it cannot be modified safely. The home directory, the platform's configuration layout, the platform whose keybindings apply and the working directory are all fields on `Scenario`, so a scenario can simulate a Mac while running on Linux.
- **No line-ending default.** With `files.eol` left at `auto`, a new file's line ending follows the platform, so a scenario that asserts the bytes of a file it created specifies the ending it expects. A scenario that depends on the ending sets `files.eol` itself.
- **No language servers, unless requested.** A machine with `rust-analyzer` installed behaves differently from one without it, and a scenario about saving a file should not fail because of something it does not mention. The default machine has none, configured the way a user would configure it: `"deco.lsp.enabled": false`.

  A scenario that tests a language server requests one with `Scenario::language_server("rust", "full")`, which writes a `deco.lsp.servers` definition pointing at `examples/language_server.rs`, a test server subprocess communicating over LSP pipes. The fake server in `deco-lsp` instead exercises protocol and process failure handling.

  Waiting for a server requires real time, so `Editor::settle_until` sleeps and polls the editor's idle path. `Editor::wait` does not sleep; it only advances the clock passed to the editor, which works for `files.autoSave` but not for a subprocess.

## Where the scenarios are

| File | What it covers |
| --- | --- |
| `tests/editing.rs` | Typing and saving: indentation from settings, undo grouping at a human typing rate, comments, CRLF and final newlines, Unicode, 200,000 lines |
| `tests/configuration.rs` | Which settings file wins, VS Code's read when deco has none, workspace layers, broken JSON, `--clean`, `--print-config` |
| `tests/keybindings.rs` | Rebinding, `-command` removals, chords, `when` clauses, per-platform keys, a broken `keybindings.json` |
| `tests/files.rs` | Save-as, untitled buffers, tabs, save-all, auto-save, revert after an external change |
| `tests/navigation.rs` | Quick open, go to line, find and replace, search in files, the command palette |
| `tests/appearance.rs` | Themes from an installed extension, the frame at every terminal size, escape sequences in a file name, wide characters |
| `tests/workflow.rs` | Long sessions: several hundred keystrokes at one editor, because a class of bug only exists after the fifth thing |
| `tests/language_servers.rs` | A real server on a real pipe: diagnostics, hover, definition, references, symbols, completion, formatting — and a server a cloned repository asked for, which is not run |

Remote sessions have scenarios of their own in
[`crates/deco/tests/remote_editor.rs`](../crates/deco/tests/remote_editor.rs),
where `CARGO_BIN_EXE_deco` names the binary to run as the remote environment.
`Scenario::remote_file` places files in a directory that the local side does not have, so a file that opens must have arrived over the connection.

A failing scenario keeps its directory on disk and prints its location, so you can inspect what the home directory actually contained.

## What this found

The suite was written for an editor whose components already had good unit test coverage, and it still found five defects that unit tests could not detect, because each was a mismatch between two components rather than a fault in one:

- **Workspace settings were never loaded for a relative path on the command line.** The search for a `.git` or `.vscode` directory above the file queried the filesystem with a relative path, so every lookup was resolved against the process's working directory. It found the correct result only when that directory was the one the path was relative to.
- **Save-as could open the same file twice.** A relative path entered in the save prompt was stored unresolved, while every other way of opening a file produces an absolute path. Saving an untitled buffer as `notes.txt` and then choosing `notes.txt` from quick open therefore opened it again, in a second buffer with a second undo history, and the tab saved last overwrote the other.
- **The frame could be taller than the terminal.** Eight palette choices, an input line and a status bar take ten rows, but a terminal can have five. Drawing ten rows into five scrolls a real terminal and moves the editor off the screen. The prompt's list now shrinks to the rows available.
- **A one-row terminal drew two rows.** A comment in the unit test stated that the status bar takes priority, but the test asserted that both were drawn.
- **A seeded prompt appended to its initial text instead of replacing it.** Save As and Find in Files open with initial text, and the one-line input had no selection, so the next key appended to it: `ctrl+shift+f` on `fn` followed by typing `println` searched for `fnprintln`. The unit tests for the input and for the seeding each passed; only pressing the keys in sequence showed the defect. Fixed by giving the field a selection that covers all of its text.

One more was found while making the suite run on Windows:

- **`files.eol` changed existing line endings but was ignored for new files.** VS Code applies it only to *new* files and keeps the existing ending of other files. deco applied it in `Document::from_file`, so opening a CRLF file with `"files.eol": "\n"` converted it and the next save rewrote every line. Meanwhile, `Document::untitled` built a `Buffer::new()` without reading the setting, so the setting was ignored in the one case it should control. Fixed: the setting now applies where a buffer has no existing ending to keep.

## What the second round found

The scenarios above deliberately disabled language servers and left remote sessions to their own protocol tests. The next defects were in those two areas:

- **The completion list was never drawn.** `overlay_suggest` renders the list beside the cursor and has seven unit tests, but the event loop requested frames containing only a *hover*, and that function had no way to receive a completion list. The list was fetched, filtered and navigable, but not visible. Fixed.
- **`ctrl+space` cannot reach Trigger Suggest in a terminal.** A terminal sends NUL for it, crossterm converts that to `Char(' ')` with Control, deco mapped only `KeyCode::Null` to `space`, and the binding parsed to `Named(Space)`, so the two never matched. The GUI mapped its space bar the other way, so the frontends interpreted a `keybindings.json` differently, and an unbound space typed nothing in the GUI, because typing is decided by matching `Key::Char`. Fixed: `space` is one key with one representation, the character.
- **A refused server is not mentioned when the user has one of their own.** `Lsp::attach` collected refusals and reported them after the loop, but the loop returns as soon as a trusted candidate starts, so refusals were reported only when no other candidate was available. Fixed: refusals are reported before any candidate is tried, and they go into the problem list, so `attach` running on every tab switch does not repeat them.
- **Save-as in a remote session renames the document to a local path**, and every later save then asks the server to write that path, outside the workspace it serves. Fixed: the entered name is interpreted on the remote environment and the write goes through the connection.
- **Revert in a remote session reads this machine**, at the remote environment's relative path. Fixed: it also reads through the connection.
