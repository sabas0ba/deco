//! A `keybindings.json` on disk, and the keys it changes.
//!
//! A keybinding is only real once pressing the key runs the command. Resolving
//! the rule correctly and then never reaching the command is the failure these
//! scenarios exist to catch, so every one of them presses a key and looks at
//! what happened to the document.

use deco_keymap::binding::Platform;

use deco_e2e::Scenario;

#[test]
fn a_rebound_key_runs_the_command_it_was_bound_to() {
    let scenario = Scenario::new("rebind")
        .user_keybindings(
            r#"[
                { "key": "ctrl+e", "command": "editor.action.commentLine", "when": "editorTextFocus" }
            ]"#,
        )
        .file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    editor.press("ctrl+e");
    assert_eq!(editor.text(), "// let x = 1;\n");
}

#[test]
fn a_removed_default_stops_doing_what_it_used_to() {
    // `-command` is how VS Code takes a default away, and somebody who has
    // removed `ctrl+/` expects `ctrl+/` to do nothing at all — not to fall back
    // to the default it was written to cancel.
    let scenario = Scenario::new("remove-default")
        .user_keybindings(r#"[{ "key": "ctrl+/", "command": "-editor.action.commentLine" }]"#)
        .file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    editor.press("ctrl+/");
    assert_eq!(
        editor.text(),
        "let x = 1;\n",
        "the line was commented anyway"
    );
}

#[test]
fn a_users_binding_wins_over_the_built_in_one_for_the_same_key() {
    let scenario = Scenario::new("override")
        .user_keybindings(
            r#"[
                { "key": "ctrl+/", "command": "editor.action.selectAll", "when": "editorTextFocus" }
            ]"#,
        )
        .file("a.rs", "let x = 1;\nlet y = 2;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    editor.press("ctrl+/");
    // Not commented — and something was selected instead.
    assert_eq!(editor.text(), "let x = 1;\nlet y = 2;\n");
    editor.type_text("z");
    assert_eq!(
        editor.text(),
        "z",
        "select-all should have replaced the file"
    );
}

#[test]
fn a_two_key_chord_needs_both_keys() {
    let scenario = Scenario::new("chord")
        .user_keybindings(
            r#"[
                { "key": "ctrl+k ctrl+w", "command": "editor.action.commentLine", "when": "editorTextFocus" }
            ]"#,
        )
        .file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    // The first key alone does nothing but wait for the second.
    editor.press("ctrl+k");
    assert_eq!(editor.text(), "let x = 1;\n");
    editor.press("ctrl+w");
    assert_eq!(editor.text(), "// let x = 1;\n");
}

#[test]
fn a_chord_that_is_abandoned_does_not_leave_the_keyboard_stuck() {
    // Pressing the first half of a chord and then something else is a thing
    // people do by accident constantly. The editor has to come back.
    let scenario = Scenario::new("chord-abandoned").file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+k");
    editor.press("escape");
    editor.type_text("hello");

    assert_eq!(editor.text(), "hellox\n");
}

#[test]
fn a_when_clause_decides_whether_the_binding_applies() {
    // The same key, two commands, chosen by context: `escape` cancels a
    // selection when there is one, and this binding only applies when there is
    // not.
    let scenario = Scenario::new("when")
        .user_keybindings(
            r#"[
                { "key": "ctrl+e", "command": "editor.action.commentLine", "when": "editorHasSelection" }
            ]"#,
        )
        .file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    editor.press("ctrl+e");
    assert_eq!(editor.text(), "let x = 1;\n", "no selection, no command");

    editor.press("ctrl+a");
    editor.press("ctrl+e");
    assert_eq!(editor.text(), "// let x = 1;\n");
}

#[test]
fn a_mac_keyboard_gets_the_mac_half_of_the_default_bindings() {
    // Every default that differs per platform carries a `mac` field, and a
    // machine that is a Mac has to get that half rather than the other one.
    let scenario = Scenario::new("mac-defaults")
        .platform(Platform::Mac)
        .file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    editor.press("cmd+/");
    assert_eq!(editor.text(), "// let x = 1;\n");

    // And the key the other platforms use is not also bound here.
    editor.press("ctrl+/");
    assert_eq!(editor.text(), "// let x = 1;\n");
}

#[test]
fn a_binding_written_for_a_mac_is_ignored_on_a_machine_that_is_not_one() {
    let scenario = Scenario::new("mac-only-binding")
        .platform(Platform::Linux)
        .user_keybindings(
            r#"[
                { "key": "ctrl+e", "mac": "cmd+e", "command": "editor.action.commentLine", "when": "editorTextFocus" }
            ]"#,
        )
        .file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    editor.press("cmd+e");
    assert_eq!(editor.text(), "let x = 1;\n");
    editor.press("ctrl+e");
    assert_eq!(editor.text(), "// let x = 1;\n");
}

#[test]
fn vs_codes_keybindings_are_read_when_deco_has_none() {
    let scenario = Scenario::new("vscode-keys")
        .vscode_keybindings(
            r#"[
                { "key": "ctrl+e", "command": "editor.action.commentLine", "when": "editorTextFocus" }
            ]"#,
        )
        .file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    editor.press("ctrl+e");
    assert_eq!(editor.text(), "// let x = 1;\n");
}

#[test]
fn a_broken_keybindings_file_leaves_the_defaults_working_and_says_so() {
    let scenario = Scenario::new("broken-keys")
        .user_keybindings(r#"[ { "key": "ctrl+e", ] "#)
        .file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    assert!(
        !editor.problems().is_empty(),
        "a keybindings file that does not parse should be reported"
    );
    // And the built-in bindings still work, because an editor whose keyboard
    // stopped answering is worse than one that ignored a broken file.
    editor.press("ctrl+/");
    assert_eq!(editor.text(), "// let x = 1;\n");
}

#[test]
fn a_binding_to_a_command_that_does_not_exist_says_so_when_it_is_pressed() {
    let scenario = Scenario::new("unknown-command")
        .user_keybindings(r#"[{ "key": "ctrl+e", "command": "acme.doesNotExist" }]"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+e");
    let screen = editor.screen();
    screen.assert_fits();
    assert!(
        screen.status_line().contains("acme.doesNotExist"),
        "the status line should name the command it could not run{}",
        screen.dump()
    );
}

#[test]
fn a_typed_letter_is_still_a_typed_letter_when_a_binding_uses_it_with_a_modifier() {
    // Rebinding `ctrl+e` must not make `e` stop typing an `e`.
    let scenario = Scenario::new("plain-letter")
        .user_keybindings(r#"[{ "key": "ctrl+e", "command": "editor.action.selectAll" }]"#)
        .file("a.txt", "\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.type_text("eee");
    assert_eq!(editor.text(), "eee\n");
}

#[test]
fn the_number_of_bindings_in_force_is_reported_by_print_config() {
    let scenario = Scenario::new("binding-count")
        .user_keybindings(r#"[{ "key": "ctrl+e", "command": "editor.action.commentLine" }]"#)
        .file("a.txt", "x\n");

    let report = scenario.print_config(&["a.txt"]);
    assert!(report.contains("keybindings"), "{report}");
}
