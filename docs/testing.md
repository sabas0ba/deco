# Testing

deco is tested at two levels, and they are testing different things.

**Unit tests** sit next to the code, one per crate, and there are about 1,700 of
them. They build the struct they are about, call the function they are about, and
assert on what came back. That is where most of the confidence in this codebase
comes from: `deco-keymap` resolves a chord correctly, `deco-config` layers a
`settings.json` correctly, `deco_tui::render` lays a session out correctly.

**End-to-end scenarios** live in [`crates/deco-e2e`](../crates/deco-e2e) and
there are about a hundred and twenty. They start the editor on a machine the test built, press
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
- **No line-ending default.** A new file's ending follows the platform, so a
  scenario asserting the bytes of a file it created names the ending it expects.
  The harness deliberately does not pin `files.eol` for everyone: in deco that
  setting also converts the ending of every *existing* file that is opened, so a
  harness-wide default would quietly change what half the scenarios were testing.
- **No language servers, unless asked.** A machine with `rust-analyzer` installed
  is a different machine from one without, and a scenario about saving a file
  should not start failing because of something it never mentioned. The default
  machine has none, said the way a user would say it —
  `"deco.lsp.enabled": false`.

  A scenario that *is* about a language server asks for one with
  `Scenario::language_server("rust", "full")`, which writes a `deco.lsp.servers`
  definition pointing at `examples/language_server.rs` — a real program on a real
  pipe, answering real LSP. `deco-lsp` has a fake server too and it is the
  opposite instrument: that one acts out failure modes and cannot tell you
  whether go-to-definition works.

  Waiting on one needs real time, so `Editor::settle_until` sleeps and polls the
  editor's own idle path. `Editor::wait` does not — it advances only the clock
  the editor is handed, which is right for `files.autoSave` and useless for a
  subprocess.

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
where `CARGO_BIN_EXE_deco` names the binary to run as the far end.
`Scenario::remote_file` puts files on a directory this machine does not have, so
that a file arriving over the connection is one the local disk could not have
supplied.

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

And one it found while the suite was being made to run on Windows, recorded but
not changed here because it is a question about what the setting means rather
than a fault in carrying it out:

- **`files.eol` converts existing files rather than only new ones.** VS Code
  treats it as the ending a *new* file gets; deco applies it in
  `Document::from_file`, so opening a CRLF file with `"files.eol": "\n"` in
  settings converts it, and the next save writes every line back changed. Pinned
  by `setting_files_eol_converts_every_existing_file_that_is_opened`.

## What the second round found

The scenarios above deliberately turned language servers off and left remote
sessions to their own protocol tests. Both gaps were where the next defects were:

- **The completion list was never drawn.** `overlay_suggest` renders one beside
  the cursor and has seven unit tests; the event loop asked the renderer for a
  frame with a *hover* in it, and there is no way to mention a completion list to
  that function. The list was fetched, filtered, navigable and invisible. Fixed.
- **`ctrl+space` cannot reach Trigger Suggest in a terminal.** A terminal sends
  NUL for it; crossterm turns that into `Char(' ')` with Control; deco maps only
  `KeyCode::Null` to `space`, and the binding is `Named(Space)`. The two never
  meet. The GUI maps its space bar the other way, so the frontends disagree.
- **A refused server is not mentioned when the user has one of their own.**
  `Lsp::attach` collects refusals and reports them after the loop, and the loop
  returns as soon as a trusted candidate starts — so the disclosure is reached
  only when there was nothing else to try.
- **Save-as in a remote session renames the document to a local path**, which
  every later save then asks the server to write, outside the workspace it
  serves. Fixed: the typed name is the far end's and the write goes through the
  connection.
- **Revert in a remote session reads this machine**, at the far end's relative
  path. Fixed: it reads through the connection too.
