//! Typing into a file and saving it to disk.
//!
//! Each scenario checks the result a user sees: not that a command returned the
//! right `Outcome`, but that after the keystrokes the file contains the
//! expected bytes.

use deco_e2e::Scenario;

#[test]
fn a_file_opens_showing_its_first_line_and_its_name() {
    let scenario = Scenario::new("open").file("src/main.rs", "fn main() {\n    hello();\n}\n");
    let mut editor = scenario.launch(&["src/main.rs"]);

    let screen = editor.screen();
    screen.assert_fits();
    // The gutter, then the text. Line one is on the first row, so the file
    // opens at the top.
    screen.assert_row_shows(0, "fn main() {");
    screen.assert_row_shows(1, "hello();");
    // The status line shows the name and the position.
    screen.assert_status("main.rs");
    screen.assert_status("Ln 1, Col 1");
}

#[test]
fn typing_a_line_and_saving_it_puts_it_in_the_file() {
    let scenario = Scenario::new("type-save").file("notes.txt", "one\ntwo\n");
    let mut editor = scenario.launch(&["notes.txt"]);

    editor.press("ctrl+end");
    editor.type_text("three\n");
    assert!(editor.is_dirty(), "the document should be unsaved");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("notes.txt"), "one\ntwo\nthree\n");
    assert!(!editor.is_dirty(), "saving should clear the dirty flag");
    editor.screen().assert_status("Saved");
}

#[test]
fn a_file_that_does_not_exist_yet_is_created_by_saving_it() {
    // The usual way to create a new file: name it on the command line and start
    // typing.
    // The line ending is set explicitly. A new file uses the platform's ending
    // unless configured otherwise, and this scenario tests file creation, not
    // the runner's platform.
    let scenario = Scenario::new("new-file").user_settings(r#"{ "files.eol": "\n" }"#);
    let mut editor = scenario.launch(&["fresh.md"]);
    assert!(!editor.exists("fresh.md"), "nothing on disk yet");

    editor.type_text("# Title\n");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("fresh.md"), "# Title\n");
}

#[test]
fn the_indentation_settings_say_what_tab_inserts() {
    // A project configured for two-space indentation indents by two, using
    // VS Code's setting key.
    let scenario = Scenario::new("indent")
        .user_settings(r#"{ "editor.tabSize": 2, "editor.insertSpaces": true }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("tab");
    // Checked before saving. The save message contains the absolute path, which
    // in a deep directory is wider than the terminal and hides the rest of the
    // status line.
    editor.screen().assert_status("Spaces: 2");

    editor.press("ctrl+s");
    assert_eq!(editor.on_disk("a.txt"), "  x\n");
}

#[test]
fn a_language_override_beats_the_general_setting_for_that_language() {
    // A VS Code user uses `"[markdown]": { … }` to set a different indentation
    // for Markdown only. deco must apply it the same way.
    let scenario = Scenario::new("language-override")
        .user_settings(
            r#"{
                "editor.tabSize": 8,
                "editor.insertSpaces": true,
                "[markdown]": { "editor.tabSize": 2 }
            }"#,
        )
        .file("notes.md", "x\n")
        .file("code.py", "y\n");

    let mut markdown = scenario.launch(&["notes.md"]);
    markdown.press("tab");
    markdown.press("ctrl+s");
    assert_eq!(markdown.on_disk("notes.md"), "  x\n");

    let mut python = scenario.launch(&["code.py"]);
    python.press("tab");
    python.press("ctrl+s");
    assert_eq!(python.on_disk("code.py"), "        y\n");
}

#[test]
fn typing_a_word_undoes_as_a_word() {
    // Undo granularity depends on the time between keystrokes. At a normal
    // typing rate the edits are merged, so one `ctrl+z` undoes the word rather
    // than the last letter. A test that pressed every key at the same
    // millisecond could not detect a regression here.
    let scenario = Scenario::new("undo").file("a.txt", "\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.type_text("hello");
    editor.press("ctrl+z");

    assert_eq!(editor.text(), "\n", "one undo should take back the word");
}

#[test]
fn a_pause_between_words_makes_them_separate_undo_steps() {
    let scenario = Scenario::new("undo-pause").file("a.txt", "\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.type_text("hello");
    // Longer than the merge window, so the next word is a separate step.
    editor.wait(1_000);
    editor.type_text(" world");
    editor.press("ctrl+z");

    assert_eq!(editor.text(), "hello\n");
}

#[test]
fn commenting_a_line_uses_the_languages_own_comment() {
    let scenario = Scenario::new("comment")
        .file("a.rs", "let x = 1;\n")
        .file("a.py", "x = 1\n");

    let mut rust = scenario.launch(&["a.rs"]);
    rust.press("ctrl+/");
    rust.press("ctrl+s");
    assert_eq!(rust.on_disk("a.rs"), "// let x = 1;\n");

    let mut python = scenario.launch(&["a.py"]);
    python.press("ctrl+/");
    python.press("ctrl+s");
    assert_eq!(python.on_disk("a.py"), "# x = 1\n");
}

#[test]
fn commenting_twice_leaves_the_line_as_it_was() {
    let scenario = Scenario::new("uncomment").file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    editor.press("ctrl+/");
    editor.press("ctrl+/");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("a.rs"), "let x = 1;\n");
}

#[test]
fn a_windows_file_stays_a_windows_file_through_an_edit() {
    // A common cause of whole-file diffs: opening a CRLF file, changing one
    // line, and writing the file back with Unix line endings.
    let scenario = Scenario::new("crlf").file("dos.txt", "one\r\ntwo\r\n");
    let mut editor = scenario.launch(&["dos.txt"]);

    editor.press("ctrl+end");
    editor.type_text("three\n");
    editor.press("ctrl+s");

    assert_eq!(
        String::from_utf8_lossy(&editor.on_disk_bytes("dos.txt")),
        "one\r\ntwo\r\nthree\r\n"
    );
}

#[test]
fn setting_files_eol_leaves_an_existing_files_own_ending_alone() {
    // In VS Code, `files.eol` sets the line ending for new files. An existing
    // file keeps its line ending until the user changes it. deco previously
    // applied the setting on open, so editing one line of a CRLF file with the
    // common setting `"files.eol": "\n"` rewrote every line. The resulting
    // whole-file diff was not visible in the editor.
    let scenario = Scenario::new("eol-converts")
        .user_settings(r#"{ "files.eol": "\n" }"#)
        .file("dos.txt", "one\r\ntwo\r\n");
    let mut editor = scenario.launch(&["dos.txt"]);

    editor.press("ctrl+end");
    editor.type_text("!");
    editor.press("ctrl+s");

    assert_eq!(
        String::from_utf8_lossy(&editor.on_disk_bytes("dos.txt")),
        "one\r\ntwo\r\n!",
        "only the edited line should have changed"
    );
}

#[test]
fn setting_files_eol_decides_for_a_file_that_has_no_ending_of_its_own() {
    // The same rule, other case: a file without a line terminator has no ending
    // to keep, so the setting applies.
    let scenario = Scenario::new("eol-no-ending")
        .user_settings(r#"{ "files.eol": "\r\n" }"#)
        .file("one.txt", "just one line");
    let mut editor = scenario.launch(&["one.txt"]);

    editor.press("ctrl+end");
    editor.type_text("\nand another");
    editor.press("ctrl+s");

    assert_eq!(
        String::from_utf8_lossy(&editor.on_disk_bytes("one.txt")),
        "just one line\r\nand another"
    );
}

#[test]
fn a_file_with_no_trailing_newline_does_not_grow_one_by_being_saved() {
    // Saving must not add bytes the user did not type. An added final newline
    // would appear in the diff.
    let scenario = Scenario::new("no-eol").file("a.txt", "no newline at the end");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+end");
    editor.type_text("!");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("a.txt"), "no newline at the end!");
}

#[test]
fn multiple_cursors_edit_every_occurrence_at_once() {
    // `ctrl+d` is heavily used, and the next text typed must be inserted in
    // every selection.
    let scenario = Scenario::new("multi-cursor").file("a.txt", "cat\ncat\ncat\n");
    let mut editor = scenario.launch(&["a.txt"]);

    // Select the first `cat`, then add the next two occurrences.
    editor.press("ctrl+d");
    editor.press("ctrl+d");
    editor.press("ctrl+d");
    editor.type_text("dog");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("a.txt"), "dog\ndog\ndog\n");
}

#[test]
fn moving_a_line_up_swaps_it_with_the_one_above() {
    let scenario = Scenario::new("move-line").file("a.txt", "one\ntwo\nthree\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("down");
    editor.press("alt+up");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("a.txt"), "two\none\nthree\n");
}

#[test]
fn a_non_ascii_file_survives_being_edited() {
    // This fails with a rope indexed by bytes instead of characters, or with a
    // renderer that counts a wide character as one column.
    let scenario = Scenario::new("unicode").file("hello.txt", "こんにちは\nсвіт\n");
    let mut editor = scenario.launch(&["hello.txt"]);

    editor.press("ctrl+end");
    editor.type_text("🎉 done\n");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("hello.txt"), "こんにちは\nсвіт\n🎉 done\n");
    editor.screen().assert_fits();
}

#[test]
fn the_caret_lands_between_characters_of_an_emoji_never_inside_one() {
    let scenario = Scenario::new("grapheme").file("a.txt", "a👍b\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("right");
    editor.press("right");
    // Two `right` presses from the start move past `a` and the whole emoji, so
    // backspace deletes the emoji and leaves `ab`.
    editor.press("backspace");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("a.txt"), "ab\n");
}

#[test]
fn a_large_file_opens_and_edits_without_the_screen_losing_its_shape() {
    // The README's performance table is measured at 200,000 lines. It states
    // that drawing cost depends on the window size rather than the document
    // size, and this checks that the window is still drawn correctly.
    let text: String = (1..=200_000).map(|n| format!("line {n}\n")).collect();
    let scenario = Scenario::new("large").file("big.txt", &text);
    let mut editor = scenario.launch(&["big.txt"]);

    editor.screen().assert_row_shows(0, "line 1");
    editor.press("ctrl+end");
    editor.type_text("last\n");

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_status("Ln 200002");
}
