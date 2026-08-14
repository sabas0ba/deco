# Testing

deco is tested at two levels, and they are testing different things.

**Unit tests** sit next to the code, one per crate, and there are about 1,700 of
them. They build the struct they are about, call the function they are about, and
assert on what came back. That is where most of the confidence in this codebase
comes from: `deco-keymap` resolves a chord correctly, `deco-config` layers a
`settings.json` correctly, `deco_tui::render` lays a session out correctly.

**End-to-end scenarios** live in [`crates/deco-e2e`](../crates/deco-e2e) and
there are about ninety. They start the editor on a machine the test built, press
keys, and look at the screen and the disk. They exist because the way an editor
breaks in practice is rarely one function returning the wrong value. It is a
`settings.json` read from the wrong directory, a keybinding that resolved but
never reached the command, a file saved to a path nobody meant, a frame drawn
taller than the terminal it was painted into. Each of the parts is right and the
editor is wrong.

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

Four things are deliberately real:

- **The configuration directory.** `user_settings`, `vscode_settings`,
  `workspace_settings` and `user_keybindings` write JSON to a temporary home in
  the layout the platform really uses, and the session is built by
  `deco::startup::session` — the same call the binary makes. Nothing is handed a
  pre-built `Settings`.
- **The workspace.** `file` writes files to disk. Quick open walks them,
  search-in-files greps them, saving overwrites them, and `editor.on_disk(…)` is
  what a `cat` would show.
- **The keystrokes.** `press("ctrl+shift+p")` builds the crossterm `KeyEvent` a
  terminal would send, hands it to `deco_tui::keys::chord_from_event`, and then
  to `deco_tui::Driver` — the editor's own event loop with the terminal taken out
  of it. A scenario cannot reach a command except by pressing keys bound to it.
- **The screen.** `editor.screen()` renders a frame at the terminal size the
  scenario asked for and applies the same substitution `paint` applies on the way
  to a terminal. `assert_shows`, `assert_status` and `assert_fits` print the whole
  screen when they fail, framed, because the useful question about a missing
  string is never "is it missing" but "what is there instead".

## What it deliberately does not do

- **No terminal.** Nothing here proves crossterm writes what it is queued.
  `paint` has its own unit tests for that.
- **No process environment.** It is shared by every test thread, so mutating it
  cannot be done safely. Home, the platform's configuration layout, which
  platform's keybindings win and the working directory are all fields on
  `Scenario` — which is also why a scenario can be a Mac while running on Linux.
- **No language servers, unless asked.** A machine with `rust-analyzer` installed
  is a different machine from one without, and a scenario about saving a file
  should not start failing because of something it never mentioned. The default
  machine has none, said the way a user would say it —
  `"deco.lsp.enabled": false`. `Scenario::language_servers(true)` turns them back
  on.

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

A scenario that fails leaves its directory on disk and prints where it is, so the
first question — what was actually in that home directory — can be answered by
looking.

## What this found

The suite was written against an editor whose parts were already well tested, and
it still turned up four defects that no unit test could have seen, because each
one is a disagreement between two components rather than a fault in either:

- **Workspace settings were never loaded for a relative path on the command
  line.** The walk that looks for a `.git` or a `.vscode` above the file asked the
  filesystem about a relative path, so every question it asked was a question
  about the process's working directory. It reached the right answer only for as
  long as that directory and the one the path was relative to were the same.
- **Save-as could open the same file twice.** A relative path typed into the save
  prompt was stored unresolved, and every other way of opening a file produces an
  absolute one — so saving an untitled buffer as `notes.txt` and then choosing
  `notes.txt` from quick open opened it again, in a second buffer with a second
  undo history, and whichever tab was saved last won.
- **The frame could be taller than the terminal.** Eight palette choices, an input
  line and a status bar are ten rows; a terminal can be five. Painting ten rows
  into five scrolls a real terminal and walks the editor off the screen. The
  prompt's list now gives up the rows it does not have.
- **A one-row terminal drew two rows.** The unit test for it said in a comment
  that the status bar wins, and then asserted that both were drawn.
