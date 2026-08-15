//! Which settings file wins, and what happens when one of them is wrong.
//!
//! deco's headline claim is that an existing VS Code configuration means the
//! same thing here. That claim is about files in directories, so these scenarios
//! put files in directories rather than building a [`deco_config::Settings`] by
//! hand — the layering is only true if the reading is.

use deco_config::paths::Layout;
use deco_e2e::Scenario;

#[test]
fn a_vs_code_configuration_is_read_when_deco_has_none_of_its_own() {
    // Nothing copied, nothing migrated: a user who has never run deco before
    // gets their own editor's settings.
    let scenario = Scenario::new("vscode-import")
        .language_servers(true)
        .vscode_settings(r#"{ "editor.tabSize": 3, "editor.insertSpaces": true }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("tab");
    assert_eq!(editor.text(), "   x\n");
    editor.screen().assert_status("Spaces: 3");
}

#[test]
fn decos_own_settings_file_replaces_vs_codes_rather_than_merging_with_it() {
    // The rule is "deco's directory is preferred", not "the two are merged". A
    // key VS Code sets and deco's file does not is therefore *not* inherited, and
    // that is worth pinning: merging would be a defensible design, and silently
    // half-merging would not.
    let scenario = Scenario::new("shadowing")
        .vscode_settings(r#"{ "editor.tabSize": 3, "editor.insertSpaces": true }"#)
        .user_settings(r#"{ "editor.insertSpaces": true }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("tab");
    assert_eq!(
        editor.text(),
        "    x\n",
        "the tab size should be the built-in default, not VS Code's 3"
    );
}

#[test]
fn a_workspace_settings_file_beats_the_users_own() {
    // A repository that indents by two indents by two, whatever the person
    // cloning it prefers globally.
    let scenario = Scenario::new("workspace-layer")
        .user_settings(r#"{ "editor.tabSize": 8, "editor.insertSpaces": true }"#)
        .workspace_settings(r#"{ "editor.tabSize": 2 }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("tab");
    assert_eq!(editor.text(), "  x\n");
}

#[test]
fn a_deco_workspace_file_shadows_a_vs_code_one() {
    let scenario = Scenario::new("workspace-shadow")
        .user_settings(r#"{ "editor.insertSpaces": true }"#)
        .workspace_settings(r#"{ "editor.tabSize": 2 }"#)
        .deco_workspace_settings(r#"{ "editor.tabSize": 6 }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("tab");
    assert_eq!(editor.text(), "      x\n");
}

#[test]
fn the_workspace_is_found_by_walking_up_to_a_marker() {
    // The file is three directories down and the settings are at the root, which
    // is the shape of every real project.
    let scenario = Scenario::new("workspace-walk")
        .user_settings(r#"{ "editor.insertSpaces": true }"#)
        .workspace_settings(r#"{ "editor.tabSize": 2 }"#)
        .file("src/deep/nested/a.txt", "x\n");
    let mut editor = scenario.launch(&["src/deep/nested/a.txt"]);

    editor.press("tab");
    assert_eq!(editor.text(), "  x\n");
}

#[test]
fn comments_and_trailing_commas_in_a_settings_file_are_not_errors() {
    // Every real `settings.json` has them, because VS Code writes them.
    let scenario = Scenario::new("jsonc")
        .user_settings(
            r#"{
                // How wide a tab is
                "editor.tabSize": 2,
                /* and whether it is spaces */
                "editor.insertSpaces": true,
            }"#,
        )
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    assert!(editor.problems().is_empty(), "{:?}", editor.problems());
    editor.press("tab");
    assert_eq!(editor.text(), "  x\n");
}

#[test]
fn a_broken_settings_file_is_reported_and_the_editor_still_opens_the_file() {
    // The failure mode that matters: a typo in `settings.json` must not be the
    // difference between having an editor and not having one.
    let scenario = Scenario::new("broken-settings")
        .user_settings(r#"{ "editor.tabSize": }"#)
        .file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    assert!(
        editor
            .problems()
            .iter()
            .any(|problem| problem.contains("settings.json")),
        "the problem should name the file: {:?}",
        editor.problems()
    );
    editor.screen().assert_row_shows(0, "hello");
    // And the built-in defaults are still in force.
    editor.press("tab");
    assert_eq!(editor.text(), "    hello\n");
}

#[test]
fn a_setting_with_the_wrong_type_does_not_take_the_editor_down_with_it() {
    let scenario = Scenario::new("wrong-type")
        .user_settings(r#"{ "editor.tabSize": "two", "editor.insertSpaces": "yes" }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("tab");
    editor.press("ctrl+s");
    // Whatever it decides, it has to decide something and say so rather than
    // panicking or writing nothing.
    assert!(editor.exists("a.txt"));
    editor.screen().assert_fits();
}

#[test]
fn clean_ignores_every_settings_file_on_the_machine() {
    // `--clean` is the flag someone is told to try when deco misbehaves, so it
    // has to genuinely bypass the configuration rather than merely most of it.
    let scenario = Scenario::new("clean")
        .language_servers(true)
        .user_settings(r#"{ "editor.tabSize": 2, "editor.insertSpaces": true }"#)
        .workspace_settings(r#"{ "editor.tabSize": 3 }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["--clean", "a.txt"]);

    editor.press("tab");
    assert_eq!(editor.text(), "    x\n", "the built-in default");
}

#[test]
fn print_config_says_where_the_answer_came_from() {
    // The flag exists to answer "why is my setting not applying", so the answer
    // has to include the value that actually won.
    let scenario = Scenario::new("print-config")
        .user_settings(r#"{ "editor.tabSize": 8, "editor.insertSpaces": true }"#)
        .workspace_settings(r#"{ "editor.tabSize": 2 }"#)
        .file("a.rs", "fn main() {}\n");

    let report = scenario.print_config(&["a.rs"]);
    assert!(report.contains("editor.tabSize      2"), "{report}");
    assert!(report.contains("language            rust"), "{report}");
    assert!(report.contains("theme"), "{report}");
}

#[test]
fn a_macos_machine_reads_its_own_configuration_directory() {
    // The layouts differ per platform, and a scenario can be any of them —
    // otherwise this rule is only ever exercised on the runner that happens to
    // be that platform.
    let scenario = Scenario::new("macos-layout")
        .layout(Layout::MacOs)
        .user_settings(r#"{ "editor.tabSize": 2, "editor.insertSpaces": true }"#)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("tab");
    assert_eq!(editor.text(), "  x\n");
}

#[test]
fn a_machine_with_no_configuration_at_all_starts_on_the_defaults() {
    let scenario = Scenario::new("bare")
        .language_servers(true)
        .file("a.txt", "x\n");
    let mut editor = scenario.launch(&["a.txt"]);

    assert!(editor.problems().is_empty(), "{:?}", editor.problems());
    editor.press("tab");
    assert_eq!(editor.text(), "    x\n");
}

#[test]
fn a_new_file_gets_the_platforms_own_ending_when_the_setting_says_auto() {
    // `files.eol: "auto"` is the default, and what it means depends on the
    // machine — LF on Unix, CRLF on Windows. Every other scenario pins the key so
    // that it can assert bytes without asserting about the runner; this is the one
    // that is about the runner, and it says so.
    let scenario = Scenario::new("eol-auto").user_settings(r#"{ "files.eol": "auto" }"#);
    let mut editor = scenario.launch(&["new.txt"]);

    editor.type_text("one\ntwo\n");
    editor.press("ctrl+s");

    let expected: &[u8] = if cfg!(windows) {
        b"one\r\ntwo\r\n"
    } else {
        b"one\ntwo\n"
    };
    assert_eq!(editor.on_disk_bytes("new.txt"), expected);
}

#[test]
fn the_end_of_line_setting_decides_what_a_new_file_gets() {
    let scenario = Scenario::new("eol").user_settings(r#"{ "files.eol": "\r\n" }"#);
    let mut editor = scenario.launch(&["new.txt"]);

    editor.type_text("one\ntwo\n");
    editor.press("ctrl+s");

    assert_eq!(
        String::from_utf8_lossy(&editor.on_disk_bytes("new.txt")),
        "one\r\ntwo\r\n"
    );
}

#[test]
fn word_wrap_from_the_settings_file_wraps_a_long_line_on_screen() {
    let long = "word ".repeat(40);
    let scenario = Scenario::new("wrap")
        .size(40, 12)
        .user_settings(r#"{ "editor.wordWrap": "on" }"#)
        .file("a.txt", &format!("{long}\n"));
    let mut editor = scenario.launch(&["a.txt"]);

    let screen = editor.screen();
    screen.assert_fits();
    // Wrapped means the text occupies several rows rather than being cut off at
    // the right-hand edge.
    assert!(
        screen.line(1).contains("word"),
        "the line should continue onto the next row{}",
        screen.dump()
    );
}
