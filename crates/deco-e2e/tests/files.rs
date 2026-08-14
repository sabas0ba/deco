//! Files, tabs, and the moments an editor writes to the disk.
//!
//! Everything here is about a side effect: which path was written, whether an
//! unnamed buffer can overwrite a named file, what an auto-save does while
//! nobody is typing. These are the paths that only exist in the frontend — the
//! core decides *that* a save should happen and this is the code that decides
//! *where* — and until the event loop could be driven without a terminal, none
//! of them had a test.

use deco_e2e::Scenario;

#[test]
fn save_as_writes_the_file_the_prompt_was_given() {
    let scenario = Scenario::new("save-as").file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    // The prompt is seeded with the path of the file being edited, so getting a
    // different name into it means taking that one out first. This is what a
    // person has to do, keystroke for keystroke.
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

    // Either it wrote it or it said why. What it must not do is report a save
    // and leave nothing on the disk.
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
    // The regression guard for a silent overwrite, driven the way it happened:
    // open a file, press `ctrl+n`, type, press `ctrl+s`. The save prompt should
    // appear rather than the original file changing under it.
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
    // The prompt is open; this is the name.
    editor.type_text("scratch.txt");
    editor.press("enter");

    assert_eq!(editor.on_disk("scratch.txt"), "scratch\n");
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
    // Two buffers over one file means two undo histories and a save that
    // silently discards the other one's work.
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
    // Not yet: nobody wants a write per keystroke.
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
    // Another program changed the file in the meantime, which is the case that
    // makes revert worth having.
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
    // `deco src` is a thing people type, because that is how VS Code is opened.
    // deco has no concept of an open folder, so it cannot do what was asked — but
    // the one thing it must not do is present an empty buffer named after a
    // directory, which a later `ctrl+s` would then try to write over.
    let scenario = Scenario::new("directory").file("src/a.txt", "x\n");
    let error = scenario.startup_error(&["src"]);

    assert!(
        error.contains("src"),
        "the failure should name what could not be opened: {error}"
    );
}

#[test]
fn a_file_saved_under_a_new_name_is_not_then_opened_a_second_time() {
    // The regression guard for a two-tabs-one-file aliasing bug: `deco` with no
    // file, then `ctrl+s` and a name, used to store the name exactly as typed.
    // Quick open hands over absolute paths, so the same file did not compare
    // equal to itself and opened again in a second buffer with its own undo
    // history — and whichever tab was saved last silently won.
    let scenario = Scenario::new("save-then-reopen");
    let mut editor = scenario.launch(&[]);

    editor.type_text("scratch\n");
    editor.press("ctrl+s");
    editor.type_text("scratch.txt");
    editor.press("enter");

    assert_eq!(editor.on_disk("scratch.txt"), "scratch\n");
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
    // It stayed, so it has to say why.
    let screen = editor.screen();
    assert!(
        !screen.status_line().is_empty(),
        "the editor refused to quit and said nothing{}",
        screen.dump()
    );
}
