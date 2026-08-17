//! The editor against a remote workspace, driven by keystrokes.
//!
//! Every other remote test in this repository calls a method: the client's
//! `search`, the server's handler, the installer's `ensure`. What none of them
//! can say is whether pressing the key that is bound to a command reaches the
//! remote at all — and that gap is where the two bugs in this feature's history
//! lived, both of them in the dispatch rather than in anything a unit test
//! covers.
//!
//! So these press keys. The far end is a real `deco --server` process serving a
//! directory the scenario built, and the only thing left out is `ssh host` in
//! front of it — an argument vector tested where it is constructed.

use std::path::Path;

use deco_e2e::Scenario;

/// The binary to run as the far end.
///
/// Available here and not inside the harness: `CARGO_BIN_EXE_*` is defined for
/// integration tests of the package that builds the binary, which is this one.
fn server() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_deco"))
}

/// A workspace with something to find in more than one file.
///
/// Returned rather than launched from, because a `Scenario` deletes its
/// directory when it is dropped: `scenario(name).launch_remote(…)` leaves the
/// editor talking to a server whose workspace has just been removed, and a
/// search that finds nothing looks exactly like a search that is broken.
fn scenario(name: &str) -> Scenario {
    Scenario::new(name)
        // On the far end's own directory rather than this machine's, so that a
        // file arriving over the connection is one this machine does not have —
        // which is the only way a scenario can tell the connection is being used
        // at all. `remote_file` rather than `file` for exactly that reason.
        //
        // `.txt` and `.md` deliberately: `rust` has a built-in server definition,
        // and a scenario using it would try to start `rust-analyzer` over a
        // transport that has no `docker` behind it.
        .remote_file("notes.txt", "the needle is here\nand not here\n")
        .remote_file("src/deep/more.txt", "another needle further down\n")
        .remote_file("README.md", "# nothing to find\n")
}

#[test]
fn a_file_opened_over_the_connection_is_the_file_on_the_far_end() {
    let scenario = scenario("remote-open");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.screen().assert_shows("the needle is here");
    // Named by the path the far end knows it by, which is what makes the rest of
    // the session's paths mean anything.
    editor.screen().assert_shows("notes.txt");
}

#[test]
fn find_in_files_searches_the_far_end_and_offers_what_it_found() {
    // The feature, through the keys that reach it. Before this, the same press
    // set a status line saying search was local and did nothing else.
    let scenario = scenario("remote-search");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+shift+f");
    // The prompt opens seeded with the word under the cursor and typing appends,
    // so a query has to be cleared first — same as the local scenario next door.
    editor.press("ctrl+x");
    editor.type_text("needle");
    editor.press("enter");

    let screen = editor.screen();
    // Both files, each named the way the server spells it — relative to the
    // workspace it serves, with `/` separators.
    screen.assert_shows("notes.txt:1");
    screen.assert_shows("src/deep/more.txt:1");
    // And the line, so a result is recognisable without opening it.
    screen.assert_shows("the needle is here");
}

#[test]
fn a_search_result_opens_the_file_it_named() {
    // The pair that matters: a result the same connection cannot then read is a
    // search whose results do not work, and neither half would look wrong alone.
    let scenario = scenario("remote-search-open");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+shift+f");
    // The prompt opens seeded with the word under the cursor and typing appends,
    // so a query has to be cleared first — same as the local scenario next door.
    editor.press("ctrl+x");
    editor.type_text("another needle");
    editor.press("enter");
    editor.press("enter");

    assert!(
        editor.path().is_some_and(|path| path.ends_with("more.txt")),
        "choosing a result should open the file it is in, not {:?}",
        editor.path()
    );
    editor.screen().assert_shows("another needle further down");
}

#[test]
fn a_term_that_is_in_no_file_says_so_rather_than_offering_nothing() {
    let scenario = scenario("remote-search-empty");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+shift+f");
    // The prompt opens seeded with the word under the cursor and typing appends,
    // so a query has to be cleared first — same as the local scenario next door.
    editor.press("ctrl+x");
    editor.type_text("haystack");
    editor.press("enter");

    let said = editor.status().unwrap_or_default().to_owned();
    assert!(said.contains("haystack"), "{said}");
}

#[test]
fn a_file_excluded_by_settings_is_not_offered_even_though_the_server_found_it() {
    // The server reads no settings — deliberately — so `files.exclude` can only
    // be applied by the end that has it. Which means this filtering is the
    // client's, and it either happens or the setting silently stops working in
    // remote sessions.
    let scenario = scenario("remote-search-excluded")
        .user_settings(r#"{ "files.exclude": { "**/deep/**": true } }"#);
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+shift+f");
    // The prompt opens seeded with the word under the cursor and typing appends,
    // so a query has to be cleared first — same as the local scenario next door.
    editor.press("ctrl+x");
    editor.type_text("needle");
    editor.press("enter");

    let screen = editor.screen();
    screen.assert_shows("notes.txt:1");
    assert!(
        !screen.text().contains("more.txt"),
        "the excluded file was offered:\n{}",
        screen.text()
    );
}

#[test]
fn saving_over_the_connection_puts_the_bytes_on_the_far_end() {
    // The other half of opening. `Outcome::Save` does consult the connection, so
    // this is the one that works — and it is worth pinning next to the two below
    // that do not, because what makes those a surprise is that this one does.
    let scenario = scenario("remote-save");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+end");
    editor.type_text("edited on the far end\n");
    editor.press("ctrl+s");

    editor.screen().assert_status("remote");
    assert!(
        editor
            .on_disk("notes.txt")
            .contains("edited on the far end"),
        "the edit should have reached the server's workspace: {:?}",
        editor.on_disk("notes.txt")
    );
}

#[test]
fn quick_open_lists_the_far_ends_files() {
    let scenario = scenario("remote-quick-open");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+p");
    let screen = editor.screen();
    screen.assert_fits();
    // A file the local machine does not have, listed because the server does.
    screen.assert_shows("more.txt");

    editor.type_text("more");
    editor.press("enter");
    editor.screen().assert_shows("another needle further down");
}

#[test]
fn save_as_in_a_remote_session_leaves_the_session_pointing_at_this_machine() {
    // A finding, pinned rather than asserted as good.
    //
    // `Outcome::Save` asks the connection; `Outcome::SaveAs` does not look at it
    // at all. It resolves the typed name against *this* machine and calls the
    // local `write_file`, then renames the open document to that local absolute
    // path — a path the far end has never heard of.
    //
    // The damage is the rename. Every later save asks the server to write a path
    // outside the workspace it serves, and the server refuses everything outside
    // it. So "save a copy under another name" quietly converts a working remote
    // session into one that cannot save at all.
    //
    // This scenario cannot show *which machine* the copy landed on, because the
    // harness serves the scenario's own workspace as the far end — the two are
    // one directory. What it can show is the rename and what the rename costs.
    let scenario = scenario("remote-save-as");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    // Before: the document is known by the name the far end knows it by.
    assert_eq!(
        editor.path().map(Path::to_path_buf),
        Some("notes.txt".into())
    );

    editor.press("ctrl+shift+s");
    let seeded = editor
        .session()
        .prompt
        .as_ref()
        .expect("a prompt")
        .text()
        .chars()
        .count();
    editor.press_times("backspace", seeded);
    editor.type_text("copy.txt");
    editor.press("enter");

    assert!(
        editor.path().is_some_and(Path::is_absolute),
        "the document was renamed to a local absolute path: {:?}",
        editor.path()
    );

    // And now saving is broken: the path is outside what the server serves.
    editor.type_text("more\n");
    editor.press("ctrl+s");
    let status = editor.status().unwrap_or_default().to_owned();
    assert!(
        status.contains("could not save") || status.contains("outside"),
        "saving after a save-as should have been refused by the server — if it \
         succeeded, this finding is fixed. status: {status:?}"
    );
}

#[test]
fn reverting_in_a_remote_session_reads_this_machine_instead() {
    // A finding, pinned rather than asserted as good, and the more dangerous of
    // the two: `Outcome::Revert` calls `std::fs::read_to_string` on the
    // document's path, which in a remote session is a path relative to the *far
    // end's* workspace. On this machine that resolves against the process's
    // working directory.
    //
    // So reverting throws the edits away — which is what revert is for — and
    // fills the buffer with whatever this machine happens to have at that
    // relative path, or reports a read error for a file that exists perfectly
    // well on the machine the session is connected to.
    let scenario = scenario("remote-revert");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+end");
    editor.type_text("unsaved work\n");
    editor.palette("Revert File");

    // The far end's copy is untouched and still says what it said.
    assert!(
        editor.on_disk("notes.txt").contains("the needle is here"),
        "the file on the far end should not have changed"
    );
    // What the buffer holds now is not the far end's file: either the read
    // failed, or it found something local. Either way it is not a revert.
    let status = editor.status().unwrap_or_default().to_owned();
    assert!(
        status.contains("could not read") || !editor.text().contains("the needle is here"),
        "revert reached the far end after all — this finding is fixed. status: {status:?}, \
         text: {:?}",
        editor.text()
    );
}
