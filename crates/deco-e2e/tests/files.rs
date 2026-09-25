//! Files, tabs, and writes to disk.
//!
//! These scenarios test side effects: which path was written, whether an
//! unnamed buffer can overwrite a named file, and what auto-save does while the
//! user is idle. This code exists only in the frontend: the core decides that
//! a save should happen, and the frontend decides where. None of it was tested
//! before the event loop could run without a terminal.

use deco_e2e::Scenario;

/// `text` followed by the line ending an untitled buffer uses on this platform.
///
/// With `files.eol` set to `auto`, an untitled document uses the platform's
/// line ending, so a scenario that saves one must expect the runner's ending.
/// When the setting names an ending, that ending is used instead; see
/// `files_eol_gives_a_new_untitled_buffer_its_ending` below.
fn untitled_line(text: &str) -> String {
    let ending = if cfg!(windows) { "\r\n" } else { "\n" };
    format!("{text}{ending}")
}

#[test]
fn files_eol_gives_a_new_untitled_buffer_its_ending() {
    // VS Code documents `files.eol` as "the default end of line character",
    // which applies to new files. `Document::untitled` previously built a
    // `Buffer::new()`, which used the platform's ending and ignored the setting.
    // The key therefore affected existing files but not new buffers.
    let scenario = Scenario::new("eol-untitled").user_settings(r#"{ "files.eol": "\r\n" }"#);
    let mut editor = scenario.launch(&[]);

    editor.type_text("one\n");
    editor.press("ctrl+s");
    editor.type_text("new.txt");
    editor.press("enter");

    assert_eq!(
        editor.on_disk_bytes("new.txt"),
        b"one\r\n",
        "the buffer should take the ending `files.eol` asked for"
    );
}

#[test]
fn save_as_writes_the_file_the_prompt_was_given() {
    let scenario = Scenario::new("save-as").file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    // The prompt starts with the path of the current file, so it must be deleted
    // before typing a different name, using the same keys as a user.
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

    assert_eq!(editor.on_disk("copy.txt"), "hello\n");
    assert_eq!(
        editor.on_disk("a.txt"),
        "hello\n",
        "the original should be untouched"
    );
    assert!(
        editor.path().is_some_and(|p| p.ends_with("copy.txt")),
        "the editor should now be editing the new file, not the old one"
    );
}

#[test]
fn save_as_takes_a_relative_path_against_the_workspace() {
    let scenario = Scenario::new("save-as-relative").file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

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
    editor.type_text("sub/copy.txt");
    editor.press("enter");

    // Either the file was written or the status explains why not. The editor
    // must not report a save without writing the file.
    if editor.exists("sub/copy.txt") {
        assert_eq!(editor.on_disk("sub/copy.txt"), "hello\n");
    } else {
        let status = editor.status().unwrap_or_default().to_owned();
        assert!(
            status.contains("sub/copy.txt") || status.to_lowercase().contains("no such"),
            "nothing was written and nothing was said: {status:?}"
        );
    }
}

#[test]
fn save_as_expands_a_leading_tilde_to_the_home_directory() {
    let scenario = Scenario::new("save-as-tilde").file("a.txt", "hello\n");
    let home = scenario.home().to_path_buf();
    let mut editor = scenario.launch(&["a.txt"]);

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
    editor.type_text("~/notes.txt");
    editor.press("enter");

    let written = home.join("notes.txt");
    assert!(
        written.exists(),
        "`~/notes.txt` should have been written to {}, status was {:?}",
        written.display(),
        editor.status()
    );
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "hello\n");
}

#[test]
fn an_untitled_buffer_cannot_overwrite_the_file_deco_was_started_with() {
    // Regression test for an unreported overwrite, using the original steps:
    // open a file, press `ctrl+n`, type, press `ctrl+s`. The save prompt must
    // appear, and the original file must not change.
    let scenario = Scenario::new("untitled").file("important.txt", "do not lose me\n");
    let mut editor = scenario.launch(&["important.txt"]);

    editor.press("ctrl+n");
    editor.type_text("scratch");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("important.txt"), "do not lose me\n");
}

#[test]
fn saving_an_untitled_buffer_asks_for_a_name_and_then_writes_it() {
    let scenario = Scenario::new("untitled-save");
    let mut editor = scenario.launch(&[]);

    editor.type_text("scratch\n");
    editor.press("ctrl+s");
    // The prompt is open; type the file name.
    editor.type_text("scratch.txt");
    editor.press("enter");

    assert_eq!(editor.on_disk("scratch.txt"), untitled_line("scratch"));
}

#[test]
fn two_files_open_as_two_tabs_and_ctrl_tab_moves_between_them() {
    let scenario = Scenario::new("tabs")
        .file("one.txt", "first\n")
        .file("two.txt", "second\n");
    let mut editor = scenario.launch(&["one.txt", "two.txt"]);

    // The first file on the command line is the one showing.
    editor.screen().assert_row_shows(1, "first");

    editor.press("ctrl+tab");
    editor.screen().assert_shows("second");

    editor.press("ctrl+tab");
    editor.screen().assert_shows("first");
}

#[test]
fn each_tab_keeps_its_own_unsaved_changes() {
    let scenario = Scenario::new("tab-state")
        .file("one.txt", "first\n")
        .file("two.txt", "second\n");
    let mut editor = scenario.launch(&["one.txt", "two.txt"]);

    editor.press("ctrl+end");
    editor.type_text("A");
    editor.press("ctrl+tab");
    editor.press("ctrl+end");
    editor.type_text("B");
    editor.press("ctrl+tab");

    assert!(
        editor.text().contains('A'),
        "the first tab lost its edit: {:?}",
        editor.text()
    );
}

#[test]
fn save_all_writes_every_changed_tab() {
    let scenario = Scenario::new("save-all")
        .file("one.txt", "first\n")
        .file("two.txt", "second\n");
    let mut editor = scenario.launch(&["one.txt", "two.txt"]);

    editor.press("ctrl+end");
    editor.type_text("A");
    editor.press("ctrl+tab");
    editor.press("ctrl+end");
    editor.type_text("B");

    editor.press("ctrl+k");
    editor.press("s");

    assert_eq!(editor.on_disk("one.txt"), "first\nA");
    assert_eq!(editor.on_disk("two.txt"), "second\nB");
}

#[test]
fn opening_the_same_file_twice_does_not_make_two_tabs_of_it() {
    // Two buffers for one file would have two undo histories, and saving one
    // would discard the other's changes without warning.
    let scenario = Scenario::new("same-file").file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);
    let before = editor.session().tab_count();

    editor.quick_open("a.txt");

    assert_eq!(
        editor.session().tab_count(),
        before,
        "the file was already open"
    );
}

#[test]
fn auto_save_writes_the_file_once_the_delay_has_passed() {
    let scenario = Scenario::new("auto-save")
        .user_settings(r#"{ "files.autoSave": "afterDelay", "files.autoSaveDelay": 500 }"#)
        .file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+end");
    editor.type_text("!");
    // Not yet written, to avoid a write per keystroke.
    editor.wait(100);
    assert_eq!(editor.on_disk("a.txt"), "hello\n");

    editor.wait(600);
    assert_eq!(editor.on_disk("a.txt"), "hello\n!");
    assert!(!editor.is_dirty());
}

#[test]
fn auto_save_off_means_off_however_long_the_editor_sits_there() {
    let scenario = Scenario::new("auto-save-off").file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+end");
    editor.type_text("!");
    editor.wait(60_000);

    assert_eq!(editor.on_disk("a.txt"), "hello\n");
    assert!(editor.is_dirty());
}

#[test]
fn reverting_brings_back_what_is_on_the_disk() {
    let scenario = Scenario::new("revert").file("a.txt", "original\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+end");
    editor.type_text(" edited");
    // Another program changed the file in the meantime, which is the main use
    // case for revert.
    editor.change_on_disk("a.txt", "changed by someone else\n");

    editor.palette("Revert File");

    assert_eq!(editor.text(), "changed by someone else\n");
    assert!(!editor.is_dirty());
}

#[test]
fn closing_a_tab_leaves_the_other_one_showing() {
    let scenario = Scenario::new("close-tab")
        .file("one.txt", "first\n")
        .file("two.txt", "second\n");
    let mut editor = scenario.launch(&["one.txt", "two.txt"]);

    editor.press("ctrl+w");

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("second");
}

#[test]
fn a_directory_given_where_a_file_was_expected_is_refused_rather_than_opened() {
    // Users type `deco src` because VS Code is opened that way. deco has no
    // concept of an open folder, so it cannot do this. It must not show an empty
    // buffer named after the directory, which a later `ctrl+s` would try to
    // write over.
    let scenario = Scenario::new("directory").file("src/a.txt", "x\n");
    let error = scenario.startup_error(&["src"]);

    assert!(
        error.contains("src"),
        "the failure should name what could not be opened: {error}"
    );
}

#[test]
fn a_file_saved_under_a_new_name_is_not_then_opened_a_second_time() {
    // Regression test for a bug where one file was open in two tabs. Running
    // `deco` with no file, then `ctrl+s` and a name, stored the name as typed.
    // Quick open passes absolute paths, so the two paths for the same file did
    // not compare equal, and the file opened again in a second buffer with its
    // own undo history. The tab saved last overwrote the other without warning.
    let scenario = Scenario::new("save-then-reopen");
    let mut editor = scenario.launch(&[]);

    editor.type_text("scratch\n");
    editor.press("ctrl+s");
    editor.type_text("scratch.txt");
    editor.press("enter");

    assert_eq!(editor.on_disk("scratch.txt"), untitled_line("scratch"));
    assert!(
        editor.path().is_some_and(|path| path.is_absolute()),
        "the document should have kept a path that means one file: {:?}",
        editor.path()
    );

    let tabs = editor.session().tab_count();
    editor.quick_open("scratch.txt");
    assert_eq!(
        editor.session().tab_count(),
        tabs,
        "the file it had just saved was opened a second time"
    );
}

#[test]
fn quitting_with_unsaved_work_does_not_throw_it_away_silently() {
    let scenario = Scenario::new("quit-dirty").file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+end");
    editor.type_text(" edited");
    editor.press("ctrl+q");

    if editor.has_quit() {
        panic!(
            "the editor quit with unsaved changes and said {:?}",
            editor.status()
        );
    }
    // The editor did not quit, so it must show the reason.
    let screen = editor.screen();
    assert!(
        !screen.status_line().is_empty(),
        "the editor refused to quit and said nothing{}",
        screen.dump()
    );
}
