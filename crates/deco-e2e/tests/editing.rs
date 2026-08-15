//! Typing into a file and getting it back onto the disk.
//!
//! The claim each of these makes is the one a person would make about an editor:
//! not that a command returned the right `Outcome`, but that after these
//! keystrokes the bytes in the file are these bytes.

use deco_e2e::Scenario;

#[test]
fn a_file_opens_showing_its_first_line_and_its_name() {
    let scenario = Scenario::new("open").file("src/main.rs", "fn main() {\n    hello();\n}\n");
    let mut editor = scenario.launch(&["src/main.rs"]);

    let screen = editor.screen();
    screen.assert_fits();
    // The gutter, then the text. Line one is on the first row: an editor that
    // opens somewhere other than the top of the file is an editor nobody trusts.
    screen.assert_row_shows(0, "fn main() {");
    screen.assert_row_shows(1, "hello();");
    // The name and the position, which is what the status line is for.
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
    // How every editor is used to make a new file: name it on the command line
    // and start typing.
    // The ending is named rather than assumed: a new file follows the platform
    // unless something says otherwise, and this scenario is about creating the
    // file rather than about the runner it is created on.
    let scenario = Scenario::new("new-file").user_settings(r#"{ "files.eol": "\n" }"#);
    let mut editor = scenario.launch(&["fresh.md"]);
    assert!(!editor.exists("fresh.md"), "nothing on disk yet");

    editor.type_text("# Title\n");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("fresh.md"), "# Title\n");
}

#[test]
fn the_indentation_settings_say_what_tab_inserts() {
    // The whole point of reading VS Code's `settings.json`: a two-space project
    // indents by two, and the key that says so is the one VS Code uses.
    let scenario = Scenario::new("indent")
        .user_settings(r#"{ "editor.tabSize": 2, "editor.insertSpaces": true }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("tab");
    // Read before saving: the save message is the whole absolute path, which on a
    // deep directory is wider than the terminal and pushes everything else off
    // the status line.
    editor.screen().assert_status("Spaces: 2");

    editor.press("ctrl+s");
    assert_eq!(editor.on_disk("a.txt"), "  x\n");
}

#[test]
fn a_language_override_beats_the_general_setting_for_that_language() {
    // `"[markdown]": { … }` is how a VS Code user keeps four spaces everywhere
    // and two in Markdown, and it has to mean that here.
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
    // Undo granularity is a clock question, and the clock is per keystroke. At a
    // human typing rate the edits coalesce, so one `ctrl+z` takes back the word
    // rather than the letter — which is what every editor does and what a test
    // that pressed every key at the same millisecond could not tell apart.
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
    // Longer than the coalescing window, so the next word is its own step.
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
    // The single most common way an editor ruins a diff: opening a CRLF file,
    // changing one line, and writing the whole thing back with Unix endings.
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
fn setting_files_eol_converts_every_existing_file_that_is_opened() {
    // A finding, pinned rather than asserted as good.
    //
    // `files.eol` in VS Code is the ending a *new* file gets; an existing file
    // keeps the ending it already had. In deco the setting is applied in
    // `Document::from_file`, so it converts on open — and the conversion reaches
    // the disk on the next save of a file that was opened only to have a typo
    // fixed in it.
    //
    // `"files.eol": "\n"` is an ordinary thing to have in a settings file. With it,
    // editing one line of a CRLF file rewrites every line of it, which is a
    // whole-file diff nobody asked for and one that is invisible in the editor.
    //
    // The unit test beside this behaviour covers `auto`, where nothing is
    // converted. Nobody asked what the other two values do to a file that already
    // had an ending of its own.
    let scenario = Scenario::new("eol-converts")
        .user_settings(r#"{ "files.eol": "\n" }"#)
        .file("dos.txt", "one\r\ntwo\r\n");
    let mut editor = scenario.launch(&["dos.txt"]);

    editor.press("ctrl+end");
    editor.type_text("!");
    editor.press("ctrl+s");

    assert_eq!(
        String::from_utf8_lossy(&editor.on_disk_bytes("dos.txt")),
        "one\ntwo\n!",
        "every line ending in the file was rewritten, not just the edited line"
    );
}

#[test]
fn a_file_with_no_trailing_newline_does_not_grow_one_by_being_saved() {
    // Saving a file deco only read should be a no-op on the bytes. A newline
    // added here shows up in everybody's diff.
    let scenario = Scenario::new("no-eol").file("a.txt", "no newline at the end");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+end");
    editor.type_text("!");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("a.txt"), "no newline at the end!");
}

#[test]
fn multiple_cursors_edit_every_occurrence_at_once() {
    // `ctrl+d` is the keystroke people reach for most, and its whole value is
    // that the next thing typed lands in every selection.
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
    // A rope indexed by bytes instead of characters breaks here, and so does a
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
    // Two rights from the start is past `a` and past the whole emoji, so a
    // backspace here takes the emoji and leaves `ab`.
    editor.press("backspace");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("a.txt"), "ab\n");
}

#[test]
fn a_large_file_opens_and_edits_without_the_screen_losing_its_shape() {
    // 200,000 lines is the size the README's performance table is measured at,
    // and the claim it makes — that drawing is bounded by the window rather than
    // the document — is only worth anything if the window still looks right.
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
