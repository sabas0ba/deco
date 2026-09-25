//! Which settings file takes precedence, and what happens when one is invalid.
//!
//! deco reads an existing VS Code configuration with the same meaning. That
//! configuration consists of files in directories, so these scenarios write
//! files to directories rather than building a [`deco_config::Settings`] by
//! hand. Correct layering depends on correct reading of the files.

use deco_config::paths::Layout;
use deco_e2e::Scenario;

#[test]
fn a_vs_code_configuration_is_read_when_deco_has_none_of_its_own() {
    // Without copying or migration, a user who has never run deco gets their
    // VS Code settings.
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
    // deco's directory is preferred; the two files are not merged. A key set by
    // VS Code but not by deco's file is therefore not inherited. This test
    // records that behaviour: a full merge would be a valid design, but a
    // partial merge would not.
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
    // A repository's indentation setting applies regardless of the user's
    // global setting.
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
    // The file is three directories deep and the settings are at the root, as in
    // a typical project.
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
    // VS Code writes both, so real `settings.json` files contain them.
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
    // A typo in `settings.json` must not prevent the editor from starting.
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
    // The built-in defaults still apply.
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
    // The exact behaviour is not specified, but the editor must not panic and
    // must write the file.
    assert!(editor.exists("a.txt"));
    editor.screen().assert_fits();
}

#[test]
fn clean_ignores_every_settings_file_on_the_machine() {
    // `--clean` is used for troubleshooting, so it must bypass all
    // configuration, not only part of it.
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
    // The flag is for finding out why a setting is not applied, so the report
    // must include the effective value.
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
    // The layouts differ per platform, and a scenario can select any of them.
    // Otherwise this rule would only be tested on a runner of that platform.
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
    // `files.eol: "auto"` is the default, and its meaning depends on the
    // platform: LF on Unix, CRLF on Windows. Other scenarios set the key so that
    // their byte assertions do not depend on the runner. This scenario tests
    // the platform-dependent behaviour.
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
    // When wrapped, the text continues on the following rows instead of being
    // cut off at the right edge.
    assert!(
        screen.line(1).contains("word"),
        "the line should continue onto the next row{}",
        screen.dump()
    );
}
