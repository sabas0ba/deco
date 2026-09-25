//! The editor against a remote workspace, driven by keystrokes.
//!
//! Every other remote test in this repository calls a method: the client's
//! `search`, the server's handler, the installer's `ensure`. None of them checks
//! whether pressing the key bound to a command reaches the remote. Both earlier
//! bugs in this feature were in that dispatch path, which unit tests do not
//! cover.
//!
//! These tests therefore press keys. The remote side is a real `deco --server`
//! process serving a directory the scenario built. Only the `ssh host` prefix is
//! omitted; that argument vector is tested where it is constructed.

use std::path::Path;

use deco_e2e::Scenario;

/// The binary to run as the remote side.
///
/// Defined here rather than in the harness because `CARGO_BIN_EXE_*` is only
/// available to integration tests of the package that builds the binary, which
/// is this one.
fn server() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_deco"))
}

/// A workspace with something to find in more than one file.
///
/// Returned rather than launched from, because a `Scenario` deletes its
/// directory when it is dropped. `scenario(name).launch_remote(…)` would leave
/// the editor connected to a server whose workspace has been removed, and a
/// search that finds nothing would look the same as a broken search.
fn scenario(name: &str) -> Scenario {
    Scenario::new(name)
        // In the remote side's directory rather than this machine's, so that a
        // file received over the connection does not exist on this machine.
        // That is the only way a scenario can confirm the connection is used,
        // and the reason for `remote_file` rather than `file`.
        //
        // `.txt` and `.md` are used intentionally: `rust` has a built-in server
        // definition, and a scenario using it would try to start
        // `rust-analyzer` over a transport that has no `docker` behind it.
        .remote_file("notes.txt", "the needle is here\nand not here\n")
        .remote_file("src/deep/more.txt", "another needle further down\n")
        .remote_file("README.md", "# nothing to find\n")
}

#[test]
fn a_file_opened_over_the_connection_is_the_file_on_the_far_end() {
    let scenario = scenario("remote-open");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.screen().assert_shows("the needle is here");
    // Named by its path on the remote side, which the rest of the session's
    // paths depend on.
    editor.screen().assert_shows("notes.txt");
}

#[test]
fn find_in_files_searches_the_far_end_and_offers_what_it_found() {
    // The feature, through its key bindings. Previously the same key only set a
    // status line saying search was local.
    let scenario = scenario("remote-search");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+shift+f");
    // The prompt opens with the word under the cursor and typing appends, so
    // the query must be cleared first, as in the local scenario.
    editor.press("ctrl+x");
    editor.type_text("needle");
    editor.press("enter");

    let screen = editor.screen();
    // Both files, each named as the server reports it: relative to the
    // workspace it serves, with `/` separators.
    screen.assert_shows("notes.txt:1");
    screen.assert_shows("src/deep/more.txt:1");
    // And the line, so a result is recognisable without opening it.
    screen.assert_shows("the needle is here");
}

#[test]
fn a_search_result_opens_the_file_it_named() {
    // Search and open must work together: if the connection cannot read a
    // result, the search results are unusable, and neither step alone would
    // show the problem.
    let scenario = scenario("remote-search-open");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+shift+f");
    // The prompt opens with the word under the cursor and typing appends, so
    // the query must be cleared first, as in the local scenario.
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
fn replace_in_files_reaches_the_far_end_rather_than_this_machine() {
    // These paths do not exist on this machine, so a replacement that reads
    // `std::fs` finds nothing and reports a file it could not read. The
    // replacement must read the remote side through the same connection that
    // everything else uses.
    let scenario = scenario("remote-replace");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+shift+h");
    editor.type_text("needle");
    editor.press("enter");
    editor.type_text("pin");
    editor.press("enter");

    let status = editor.status().unwrap_or_default().to_owned();
    assert!(
        status.starts_with("Replaced `needle`"),
        "the replacement should have reached the far end: {status}"
    );

    // The open document, and the file that had to be fetched to be changed.
    assert_eq!(editor.text(), "the pin is here\nand not here\n");
    let unsaved = editor.session().unsaved();
    let deep = unsaved
        .iter()
        .find(|(path, _)| path.to_string_lossy().contains("more.txt"))
        .map(|(_, text)| text.clone())
        .expect("the file on the far end should have been opened and changed");
    assert_eq!(deep, "another pin further down\n");

    // One undo reverts all of it, as it does locally.
    editor.press("ctrl+z");
    assert_eq!(editor.text(), "the needle is here\nand not here\n");
}

#[test]
fn a_term_that_is_in_no_file_says_so_rather_than_offering_nothing() {
    let scenario = scenario("remote-search-empty");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+shift+f");
    // The prompt opens with the word under the cursor and typing appends, so
    // the query must be cleared first, as in the local scenario.
    editor.press("ctrl+x");
    editor.type_text("haystack");
    editor.press("enter");

    let said = editor.status().unwrap_or_default().to_owned();
    assert!(said.contains("haystack"), "{said}");
}

#[test]
fn a_file_excluded_by_settings_is_not_offered_even_though_the_server_found_it() {
    // The server intentionally reads no settings, so `files.exclude` can only be
    // applied by the client, which has the setting. If the client does not
    // filter, the setting has no effect in remote sessions.
    let scenario = scenario("remote-search-excluded")
        .user_settings(r#"{ "files.exclude": { "**/deep/**": true } }"#);
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+shift+f");
    // The prompt opens with the word under the cursor and typing appends, so
    // the query must be cleared first, as in the local scenario.
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
    // The counterpart of opening: `Outcome::Save` uses the connection, and
    // save-as and revert below now do too. Save was already correct and is the
    // reference for the other two.
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
    // `Outcome::Save` uses the connection; `Outcome::SaveAs` previously did not.
    // It resolved the typed name against *this* machine, called the local
    // `write_file`, and renamed the open document to that local absolute path,
    // which does not exist on the remote side.
    //
    // The rename caused the failure: every later save asked the server to write
    // a path outside the workspace it serves, and the server rejects all such
    // paths. After a save-as, the remote session could no longer save at all,
    // while the status line still reported successful saves.
    let scenario = scenario("remote-save-as");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    // Before: the document has its name on the remote side.
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

    // The copy is on the remote side, the only place `on_disk` looks in a
    // remote scenario. A local write would have gone to this process's working
    // directory and left nothing here.
    assert_eq!(
        editor.on_disk("copy.txt"),
        "the needle is here\nand not here\n"
    );
    // The document is still named by its remote path, so the session's paths
    // stay in one namespace.
    assert_eq!(
        editor.path().map(Path::to_path_buf),
        Some("copy.txt".into())
    );

    // Therefore saving still works afterwards.
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
    // The workspace is in one place, as documented on `run_with`; splitting it
    // would make every path ambiguous. The server rejects a name outside the
    // workspace it serves, and the rejection is reported instead of writing a
    // file on this machine.
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
    // The document keeps its original path, so it was not renamed to a path
    // that cannot be saved.
    assert_eq!(
        editor.path().map(Path::to_path_buf),
        Some("notes.txt".into())
    );
}

#[test]
fn reverting_in_a_remote_session_reads_the_far_end() {
    // The more dangerous of the two: `Outcome::Revert` called
    // `std::fs::read_to_string` on the document's path, which in a remote
    // session is relative to the *remote* workspace. On this machine that path
    // resolves against the process's working directory. Reverting discarded the
    // edits, as intended, and then filled the buffer with whatever this machine
    // had at that relative path, or reported a read error for a file that
    // exists on the remote.
    let scenario = scenario("remote-revert");
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    editor.press("ctrl+end");
    editor.type_text("unsaved work\n");
    editor.palette("Revert File");

    // The remote copy is unchanged.
    assert!(
        editor.on_disk("notes.txt").contains("the needle is here"),
        "the file on the far end should not have changed"
    );
    // The buffer now contains that file, not a read error or a local file at
    // the same relative path.
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
    // The edits are kept when the read fails. Discarding them would lose work
    // because of an unrelated failure.
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
    // End-to-end check: a real `deco --server` reads its machine settings, the
    // client fetches them over the connection, and the editor resolves them as a
    // layer. Each part has unit tests, but none of them verifies that the layer
    // is applied.
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
    // The layer's position: VS Code puts `remote` above the user's layer and
    // below the workspace's. A layer applied in the wrong position is worse than
    // one not applied, because it overrides settings the user chose.
    let scenario = scenario("remote-machine-settings-order")
        .user_settings(r#"{ "editor.tabSize": 2, "editor.insertSpaces": false }"#)
        .remote_machine_settings(r#"{ "editor.tabSize": 7 }"#);
    let editor = scenario.launch_remote(&["notes.txt"], server());

    let settings = &editor.session().settings;
    // The remote layer is above the user's, so it wins where both set a value.
    assert_eq!(settings.get_u64("editor.tabSize", None), Some(7));
    // Settings the remote does not set still come from the user's file.
    assert_eq!(settings.get_bool("editor.insertSpaces", None), Some(false));
}

#[test]
fn a_language_server_the_far_end_defines_is_not_launched_on_its_word() {
    // This is why the layer is untrusted. Anyone with an account on that
    // machine may be able to write the machine-settings file, and a server
    // definition names a program to run. Connecting must not be enough to run
    // it, just as cloning a repository is not.
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
    // The common case, which must not be an error: most machines have no
    // machine-settings.json.
    let scenario =
        scenario("remote-machine-settings-absent").user_settings(r#"{ "editor.tabSize": 3 }"#);
    let mut editor = scenario.launch_remote(&["notes.txt"], server());

    assert_eq!(
        editor.session().settings.get_u64("editor.tabSize", None),
        Some(3)
    );
    editor.screen().assert_shows("the needle is here");
}
