//! Long sessions.
//!
//! Every other file here presses a handful of keys at a fresh editor. These press
//! a few hundred at one editor without restarting it, because a class of bug only
//! exists after the fifth thing: a find bar that leaves the keyboard behind, a
//! prompt that comes back with the last query still in it, a tab whose selection
//! belongs to a document that has since closed. None of it is visible in a
//! scenario short enough to reason about.

use deco_e2e::Scenario;

/// A small project, of the shape somebody actually opens.
fn project(name: &str) -> Scenario {
    Scenario::new(name)
        .user_settings(
            r#"{
                "editor.tabSize": 4,
                "editor.insertSpaces": true,
                "files.autoSave": "off"
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

    // Off to the other file, through quick open, and add a line to it.
    editor.quick_open("greet");
    assert!(editor.path().is_some_and(|p| p.ends_with("greet.rs")));
    editor.press("ctrl+end");
    editor.type_text("\n// checked\n");
    editor.press("ctrl+s");
    assert!(editor.on_disk("src/greet.rs").contains("// checked"));

    // Back to the first tab, which still knows where it was.
    editor.press("ctrl+shift+tab");
    assert!(editor.path().is_some_and(|p| p.ends_with("main.rs")));

    // A project-wide search, and open what it found.
    editor.press("ctrl+shift+f");
    editor.press("ctrl+x");
    editor.type_text("checked");
    editor.press("enter");
    editor.screen().assert_shows("greet.rs");
    editor.press("enter");
    assert!(editor.path().is_some_and(|p| p.ends_with("greet.rs")));

    // Comment a line, think better of it, undo.
    editor.press("ctrl+/");
    let commented = editor.text();
    editor.press("ctrl+z");
    assert_ne!(editor.text(), commented);

    // Everything that is still dirty goes to disk, and the screen is intact.
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
    // Open every widget in turn and escape out of it. If any of them keeps the
    // keyboard, the typing at the end lands somewhere other than the file — which
    // is the failure mode of every modal interface ever written.
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
    // The palette over the find bar, quick open over the palette. Each one takes
    // the keyboard, and the screen has to agree about which one has it.
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
    // Not a fuzzer, and not pretending to be one: a fixed, ordinary-looking
    // sequence of the keys people actually hit by accident, asserting only that
    // the editor is still drawable and still writes what it says it has. Every
    // widget key is in here, in an order nobody designed for.
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
            // Drawable after every single one, which is the claim: a frame that
            // does not fit its terminal is a corrupted screen on a real one.
            editor.screen().assert_fits();
        }
    }

    // And it can still be used afterwards.
    editor.press("ctrl+n");
    editor.type_text("still here\n");
    editor.press("ctrl+s");
    editor.type_text("after.txt");
    editor.press("enter");
    // Next to the file deco was started with, which is what a relative path in
    // the save prompt means.
    assert_eq!(editor.on_disk("src/after.txt"), "still here\n");
}

#[test]
fn what_the_editor_shows_agrees_with_what_it_would_write() {
    // The invariant worth stating once: the status line's dirty marker, the
    // session's own flag and the bytes on disk are three answers to one question,
    // and they have to be the same answer at every step.
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
