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
        // `.txt` and `.md` deliberately: `rust` has a built-in server definition,
        // and a scenario using it would try to start `rust-analyzer` over a
        // transport that has no `docker` behind it.
        .file("notes.txt", "the needle is here\nand not here\n")
        .file("src/deep/more.txt", "another needle further down\n")
        .file("README.md", "# nothing to find\n")
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
