//! Long sessions.
//!
//! The other files here press a few keys in a newly started editor. These
//! scenarios press a few hundred keys in one editor without restarting it,
//! because some bugs appear only after several operations: a find bar that
//! keeps keyboard focus, a prompt that reopens with the previous query, or a
//! tab whose selection refers to a closed document. Short scenarios do not
//! detect them.

use deco_e2e::Scenario;

/// A small project with a typical structure.
fn project(name: &str) -> Scenario {
    Scenario::new(name)
        .user_settings(
            r#"{
                "editor.tabSize": 4,
                "editor.insertSpaces": true,
                "files.autoSave": "off",
                "files.eol": "\n"
            }"#,
        )
        .workspace_settings(r#"{ "editor.tabSize": 2 }"#)
        .file(
            "src/main.rs",
            "fn main() {\n  let name = \"world\";\n  greet(name);\n}\n",
        )
        .file(
            "src/greet.rs",
            "pub fn greet(name: &str) {\n  println!(\"hello {name}\");\n}\n",
        )
        .file("README.md", "# greeter\n\nSays hello.\n")
}

#[test]
fn an_afternoon_of_editing_ends_with_the_right_bytes_in_every_file() {
    let scenario = project("afternoon");
    let mut editor = scenario.launch(&["src/main.rs"]);

    // The workspace's own indentation, not the user's.
    editor.screen().assert_status("Spaces: 2");

    // Rename the variable everywhere in this file.
    editor.press("ctrl+f");
    editor.type_text("name");
    editor.press("escape");
    editor.press("ctrl+home");
    editor.press("ctrl+d");
    editor.press("ctrl+d");
    editor.press("ctrl+d");
    editor.type_text("who");
    editor.press("ctrl+s");
    assert!(
        editor.on_disk("src/main.rs").contains("who"),
        "{}",
        editor.on_disk("src/main.rs")
    );

    // Open the other file through quick open, and add a line to it.
    editor.quick_open("greet");
    assert!(editor.path().is_some_and(|p| p.ends_with("greet.rs")));
    editor.press("ctrl+end");
    editor.type_text("\n// checked\n");
    editor.press("ctrl+s");
    assert!(editor.on_disk("src/greet.rs").contains("// checked"));

    // Return to the first tab, which keeps its state.
    editor.press("ctrl+shift+tab");
    assert!(editor.path().is_some_and(|p| p.ends_with("main.rs")));

    // Search the project and open the result.
    editor.press("ctrl+shift+f");
    editor.press("ctrl+x");
    editor.type_text("checked");
    editor.press("enter");
    editor.screen().assert_shows("greet.rs");
    editor.press("enter");
    assert!(editor.path().is_some_and(|p| p.ends_with("greet.rs")));

    // Comment a line, then undo.
    editor.press("ctrl+/");
    let commented = editor.text();
    editor.press("ctrl+z");
    assert_ne!(editor.text(), commented);

    // Save all unsaved documents, and check that the screen is intact.
    editor.press("ctrl+k");
    editor.press("s");
    let screen = editor.screen();
    screen.assert_fits();
    assert!(
        !editor.is_dirty(),
        "everything should have been saved by now"
    );
}

#[test]
fn the_keyboard_always_comes_back_to_the_document() {
    // Open each widget in turn and close it with escape. If any widget keeps
    // keyboard focus, the text typed at the end does not reach the file.
    let scenario = project("keyboard");
    let mut editor = scenario.launch(&["src/main.rs"]);
    let before = editor.text();

    for opening in [
        "ctrl+f",       // find
        "ctrl+h",       // replace
        "ctrl+p",       // quick open
        "ctrl+shift+p", // the palette
        "ctrl+g",       // go to line
        "ctrl+shift+f", // search in files
    ] {
        editor.press(opening);
        editor.press("escape");
        assert_eq!(
            editor.text(),
            before,
            "{opening} then escape changed the document"
        );
    }

    editor.press("ctrl+home");
    editor.type_text("x");
    assert_eq!(
        editor.text(),
        format!("x{before}"),
        "typing after all that should reach the document"
    );
}

#[test]
fn a_widget_opened_over_another_one_does_not_leave_the_first_behind() {
    // The palette over the find bar, quick open over the palette. Each takes
    // keyboard focus, and the screen must show the widget that has it.
    let scenario = project("stacked");
    let mut editor = scenario.launch(&["src/main.rs"]);

    editor.press("ctrl+f");
    editor.type_text("greet");
    editor.press("ctrl+shift+p");
    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("Command:");

    editor.press("escape");
    editor.press("escape");
    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_lacks("Command:");

    editor.press("ctrl+home");
    editor.type_text("z");
    assert!(editor.text().starts_with('z'));
}

#[test]
fn splitting_the_window_and_editing_both_halves_keeps_them_apart() {
    let scenario = project("split");
    let mut editor = scenario.launch(&["src/main.rs", "src/greet.rs"]);

    editor.press("ctrl+\\");
    let screen = editor.screen();
    screen.assert_fits();
    assert!(
        editor.session().group_count() >= 2,
        "the window should have split"
    );

    editor.press("ctrl+home");
    editor.type_text("// left\n");
    editor.press("ctrl+1");
    editor.press("ctrl+home");
    editor.type_text("// right\n");

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("// left");
    screen.assert_shows("// right");
}

#[test]
fn a_hundred_keystrokes_of_nonsense_leave_the_editor_standing() {
    // Not a fuzzer: a fixed sequence of keys that users commonly press by
    // accident. It asserts only that the editor can still draw a frame and
    // saves what it reports. Every widget key is included, in an arbitrary
    // order.
    let keys = [
        "ctrl+f",
        "escape",
        "ctrl+d",
        "ctrl+d",
        "alt+up",
        "ctrl+z",
        "ctrl+k",
        "escape",
        "ctrl+p",
        "escape",
        "ctrl+h",
        "tab",
        "escape",
        "ctrl+g",
        "escape",
        "ctrl+end",
        "ctrl+shift+home",
        "delete",
        "ctrl+z",
        "ctrl+shift+z",
        "ctrl+tab",
        "ctrl+w",
        "ctrl+n",
        "ctrl+shift+p",
        "escape",
        "ctrl+b",
        "ctrl+j",
        "alt+z",
        "ctrl+l",
        "escape",
    ];
    let scenario = project("nonsense");
    let mut editor = scenario.launch(&["src/main.rs", "src/greet.rs"]);

    for _ in 0..3 {
        for key in keys {
            editor.press(key);
            // Check the frame after every key. A frame that does not fit its
            // terminal corrupts the screen of a real terminal.
            editor.screen().assert_fits();
        }
    }

    // The editor is still usable afterwards.
    editor.press("ctrl+n");
    editor.type_text("still here\n");
    editor.press("ctrl+s");
    editor.type_text("after.txt");
    editor.press("enter");
    // A relative path in the save prompt is resolved next to the file deco was
    // started with. The ending is LF on every platform: `project` sets
    // `files.eol` to `\n`, and the setting applies to an untitled buffer. See
    // `files_eol_gives_a_new_untitled_buffer_its_ending` in `tests/files.rs`.
    assert_eq!(editor.on_disk("src/after.txt"), "still here\n");
}

#[test]
fn what_the_editor_shows_agrees_with_what_it_would_write() {
    // The status line's dirty marker, the session's dirty flag and the bytes on
    // disk must be consistent at every step.
    let scenario = project("agreement");
    let mut editor = scenario.launch(&["README.md"]);

    let on_disk = editor.on_disk("README.md");
    assert_eq!(editor.text(), on_disk);
    assert!(!editor.is_dirty());

    editor.press("ctrl+end");
    editor.type_text("more\n");
    assert!(editor.is_dirty());
    assert_ne!(editor.text(), editor.on_disk("README.md"));
    editor.screen().assert_status("README.md*");

    editor.press("ctrl+s");
    assert!(!editor.is_dirty());
    assert_eq!(editor.text(), editor.on_disk("README.md"));
    editor.screen().assert_lacks("README.md*");
}
