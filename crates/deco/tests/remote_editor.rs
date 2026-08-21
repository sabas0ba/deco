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
    // The other half of opening: `Outcome::Save` consults the connection, and so
    // now do save-as and revert below. This is the arm the other two were
    // measured against — what made their omission a surprise is that this one
    // was right all along.
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
fn save_as_in_a_remote_session_writes_the_far_end_and_keeps_its_names() {
    // `Outcome::Save` asks the connection; `Outcome::SaveAs` did not look at it
    // at all. It resolved the typed name against *this* machine and called the
    // local `write_file`, then renamed the open document to that local absolute
    // path — one the far end has never heard of.
    //
    // The damage was the rename: every later save asked the server to write a
    // path outside the workspace it serves, and the server refuses everything
    // outside it. So "save a copy under another name" quietly converted a
    // working remote session into one that could not save at all, while the
    // status line reported a successful save throughout.
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

    // The copy is on the far end, which is the only place `on_disk` looks in a
    // remote scenario — a local write would have gone to this process's working
    // directory and left nothing here.
    assert_eq!(
        editor.on_disk("copy.txt"),
        "the needle is here\nand not here\n"
    );
    // And the document is still named the way the far end spells it, so the
    // session's paths stay in one namespace.
    assert_eq!(
        editor.path().map(Path::to_path_buf),
        Some("copy.txt".into())
    );

    // Which is what keeps saving working afterwards.
    editor.press("ctrl+end");
    editor.type_text("more\n");
    editor.press("ctrl+s");
    assert!(
        editor.on_disk("copy.txt").contains("more"),
        "saving after a save-as should still reach the far end: {:?}",
        editor.status()
    );
    // The original is untouched.
    assert_eq!(
        editor.on_disk("notes.txt"),
        "the needle is here\nand not here\n"
    );
}

#[test]
fn save_as_onto_this_machine_is_refused_rather_than_splitting_the_workspace() {
    // The workspace is one place — `run_with` says so — and half of one would
    // make every path ambiguous. A name that points off the far end is the
    // server's to refuse, which it does for everything outside what it serves,
    // and the refusal is reported rather than quietly writing a file here.
    let scenario = scenario("remote-save-as-local");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

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
    editor.type_text("../escaped.txt");
    editor.press("enter");

    let status = editor.status().unwrap_or_default().to_owned();
    assert!(
        status.contains("outside the workspace"),
        "the refusal should say why: {status:?}"
    );
    // And the document is still the one it was, so nothing was renamed to a
    // path that cannot be saved.
    assert_eq!(
        editor.path().map(Path::to_path_buf),
        Some("notes.txt".into())
    );
}

#[test]
fn reverting_in_a_remote_session_reads_the_far_end() {
    // The more dangerous of the two: `Outcome::Revert` called
    // `std::fs::read_to_string` on the document's path, which in a remote
    // session is relative to the *far end's* workspace. On this machine that
    // resolves against the process's working directory — so reverting threw the
    // edits away, which is what revert is for, and then filled the buffer with
    // whatever this machine happened to have at that relative path, or reported
    // a read error for a file that exists perfectly well over there.
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
    // And the buffer is now that file, rather than a read error or something
    // local that happened to be at the same relative path.
    assert_eq!(
        editor.text(),
        "the needle is here\nand not here\n",
        "status: {:?}",
        editor.status()
    );
    assert!(!editor.is_dirty(), "a reverted document is not modified");
}

#[test]
fn reverting_reports_a_far_end_read_failure_without_losing_the_edits() {
    // The edits stay when the read fails: throwing them away because the file
    // could not be read would lose work to a failure that had nothing to do
    // with it.
    let scenario = scenario("remote-revert-missing");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+end");
    editor.type_text("unsaved work\n");
    std::fs::remove_file(editor.workspace().join("notes.txt"))
        .expect("removing the far end's copy");
    editor.palette("Revert File");

    let status = editor.status().unwrap_or_default().to_owned();
    assert!(status.contains("could not read"), "{status:?}");
    assert!(
        editor.text().contains("unsaved work"),
        "the edits should still be there: {:?}",
        editor.text()
    );
}

#[test]
fn the_far_ends_own_settings_reach_the_session() {
    // The wiring, end to end: a real `deco --server` reads its machine settings,
    // the client fetches them over the connection, and the editor resolves them
    // as a layer. Every part of this was unit-tested separately and none of
    // those tests could tell whether the layer was ever applied.
    //
    // `editor.tabSize` because it is unambiguous and visible from the session.
    let scenario =
        scenario("remote-machine-settings").remote_machine_settings(r#"{ "editor.tabSize": 7 }"#);
    let editor = scenario.launch_remote(&["notes.txt"], server());

    assert_eq!(
        editor.session().settings.get_u64("editor.tabSize", None),
        Some(7),
        "the remote's machine settings should have become the `remote` layer"
    );
}

#[test]
fn this_machines_settings_beat_the_far_ends_where_a_project_disagrees() {
    // The layer's position, which is the other half of getting it right: VS
    // Code puts `remote` above the user's own and below the workspace's, and a
    // layer applied in the wrong place is worse than one not applied at all —
    // it changes settings the user thought they had decided.
    let scenario = scenario("remote-machine-settings-order")
        .user_settings(r#"{ "editor.tabSize": 2, "editor.insertSpaces": false }"#)
        .remote_machine_settings(r#"{ "editor.tabSize": 7 }"#);
    let editor = scenario.launch_remote(&["notes.txt"], server());

    let settings = &editor.session().settings;
    // The remote is above the user, so it wins where both speak.
    assert_eq!(settings.get_u64("editor.tabSize", None), Some(7));
    // And says nothing about the rest, which the user's own file still decides.
    assert_eq!(settings.get_bool("editor.insertSpaces", None), Some(false));
}

#[test]
fn a_language_server_the_far_end_defines_is_not_launched_on_its_word() {
    // The reason the layer is untrusted. A machine-settings file sits where
    // anyone with an account on that machine can write it, and a server
    // definition is a program to run — so connecting must not be enough to
    // execute one, exactly as cloning a repository is not.
    let scenario = scenario("remote-machine-settings-lsp").remote_machine_settings(
        r#"{ "deco.lsp.servers": { "theirs": { "languages": ["plaintext"], "command": "./evil" } } }"#,
    );
    let editor = scenario.launch_remote(&["notes.txt"], server());

    let (registry, _) = deco_lsp::settings::registry(&editor.session().settings);
    let server = registry.get("theirs").expect("the definition is read");
    assert!(
        server.trust.needs_confirmation(),
        "a server defined by the remote must be confirmed, not trusted: {:?}",
        server.trust
    );
}

#[test]
fn a_far_end_with_no_settings_of_its_own_changes_nothing() {
    // The ordinary case, and the one that must not become an error: most
    // machines have no machine-settings.json at all.
    let scenario =
        scenario("remote-machine-settings-absent").user_settings(r#"{ "editor.tabSize": 3 }"#);
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    assert_eq!(
        editor.session().settings.get_u64("editor.tabSize", None),
        Some(3)
    );
    editor.screen().assert_shows("the needle is here");
}
