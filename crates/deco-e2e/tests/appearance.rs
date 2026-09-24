//! Appearance: themes loaded from disk, and a frame that matches the terminal
//! size.
//!
//! The renderer's unit tests cover layout for a given session. These scenarios
//! test that a theme file in an extension directory is found, read and applied,
//! and that the frame still matches the terminal size after operations that
//! change the layout.

use deco_e2e::Scenario;

/// A theme file with a distinctive background colour.
const MAGENTA: &str = r##"{
    "name": "Acme Magenta",
    "type": "dark",
    "colors": {
        "editor.background": "#ff00ff",
        "editor.foreground": "#00ff00"
    },
    "tokenColors": []
}"##;

#[test]
fn a_theme_from_an_installed_extension_is_offered_and_applied() {
    // Marketplace theme compatibility, end to end: a directory in the layout of
    // an installed VS Code theme appears in `ctrl+k ctrl+t`, and choosing it
    // repaints the screen.
    let scenario = Scenario::new("theme")
        .theme_extension("acme.magenta-1.0.0", "Acme Magenta", MAGENTA)
        .file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+k");
    editor.press("ctrl+t");
    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("Acme Magenta");

    editor.type_text("Magenta");
    editor.press("enter");

    let screen = editor.screen();
    let (_, background) = screen
        .colours_at(0, 0)
        .expect("the first cell should have colours");
    assert_eq!(
        (background.r, background.g, background.b),
        (0xff, 0x00, 0xff),
        "the theme's own background should be painted{}",
        screen.dump()
    );
}

#[test]
fn the_theme_named_in_settings_is_the_one_the_editor_starts_in() {
    let scenario = Scenario::new("theme-setting")
        .theme_extension("acme.magenta-1.0.0", "Acme Magenta", MAGENTA)
        .user_settings(r#"{ "workbench.colorTheme": "Acme Magenta" }"#)
        .file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    let report = scenario.print_config(&["a.txt"]);
    assert!(
        report.contains("Acme Magenta"),
        "the resolved theme should be the one named in settings: {report}"
    );
    editor.screen().assert_fits();
}

#[test]
fn a_theme_that_is_named_but_not_installed_is_reported_rather_than_ignored() {
    let scenario = Scenario::new("theme-missing")
        .user_settings(r#"{ "workbench.colorTheme": "Nothing Like This" }"#)
        .file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    assert!(
        editor
            .problems()
            .iter()
            .any(|problem| problem.contains("Nothing Like This")),
        "the problem should name the theme: {:?}",
        editor.problems()
    );
    // The editor is still usable with the fallback theme.
    editor.screen().assert_row_shows(0, "hello");
}

#[test]
fn the_built_in_themes_are_offered_even_with_nothing_installed() {
    let scenario = Scenario::new("theme-builtin").file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+k");
    editor.press("ctrl+t");

    let screen = editor.screen();
    screen.assert_fits();
    assert!(
        screen.lines().iter().any(|line| line.contains("Dark")),
        "the picker should list the themes deco ships with{}",
        screen.dump()
    );
}

#[test]
fn the_frame_is_exactly_the_size_of_the_terminal_at_every_size() {
    // Too few rows leave old content on screen, and a row that is too wide
    // wraps and scrolls the whole frame up. Both errors often appear only at
    // terminal sizes the developer did not test.
    for (width, height) in [(80, 24), (40, 10), (200, 60), (20, 5), (120, 3)] {
        let scenario = Scenario::new(&format!("size-{width}x{height}"))
            .size(width, height)
            .file("a.txt", "one\ntwo\nthree\nfour\nfive\n");
        let mut editor = scenario.launch(&["a.txt"]);
        editor.screen().assert_fits();

        // Also with UI elements that take rows.
        editor.press("ctrl+f");
        editor.screen().assert_fits();
        editor.press("escape");
        editor.press("ctrl+shift+p");
        editor.screen().assert_fits();
    }
}

#[test]
fn resizing_the_terminal_reflows_without_losing_the_caret() {
    let scenario = Scenario::new("resize").file("a.txt", "one\ntwo\nthree\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+end");
    editor.resize(30, 8);
    let screen = editor.screen();
    screen.assert_fits();
    assert!(
        screen.cursor().is_some(),
        "the caret should still be on screen{}",
        screen.dump()
    );

    editor.resize(160, 50);
    editor.screen().assert_fits();
}

#[test]
fn a_terminal_too_small_to_draw_in_does_not_panic() {
    // Users can resize a terminal to almost nothing, and a panic at that size
    // would lose unsaved changes.
    let scenario = Scenario::new("tiny")
        .size(1, 1)
        .file("a.txt", "hello\nworld\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.screen().assert_fits();
    editor.type_text("x");
    editor.press("ctrl+f");
    editor.screen().assert_fits();
}

/// Fails if the frame contains bytes that a terminal would interpret as control
/// sequences.
#[track_caller]
fn assert_nothing_executable(screen: &deco_e2e::Screen) {
    assert!(
        !screen.text().contains('\u{1b}'),
        "an escape byte reached the screen{}",
        screen.dump()
    );
    assert!(
        !screen.text().contains('\u{7}'),
        "a bell reached the screen{}",
        screen.dump()
    );
}

#[test]
fn a_settings_file_cannot_put_an_escape_sequence_on_the_screen() {
    // A cloned repository's `.vscode/settings.json` is untrusted text, and deco
    // includes parts of it in error messages, here the name of a theme that is
    // not installed. That message reaches the status line without the
    // renderer's sanitisation of document text, so `paint` must make it
    // printable.
    //
    // The bytes are written as JSON escapes rather than raw bytes. A raw control
    // character is invalid JSON and the parser rejects the whole file, so a
    // malicious file must use escapes such as `\u001b`. That is valid JSON and
    // decodes to the byte a terminal interprets.
    let scenario = Scenario::new("escape-setting")
        .file("a.txt", "hello\n")
        .workspace_settings(
            r#"{ "workbench.colorTheme": "Nothing\u001b]52;c;aGk=\u0007Like This" }"#,
        );
    let mut editor = scenario.launch(&["a.txt"]);

    assert!(
        editor
            .problems()
            .iter()
            .any(|problem| problem.contains('\u{1b}')),
        "the theme name should have reached a problem message: {:?}",
        editor.problems()
    );
    assert_nothing_executable(&editor.screen());
}

// A file name is also untrusted text, and it reaches the tab bar and the status
// line without the renderer's sanitisation.
//
// Unix only, because the problem exists only there. Windows rejects creating a
// name that contains a control byte (`ERROR_INVALID_NAME`), so such a file
// cannot exist, and the scenario would test the operating system rather than
// deco. The scenario above covers the same sanitisation on every platform.
#[cfg(unix)]
#[test]
fn a_file_whose_name_carries_an_escape_sequence_cannot_reach_the_terminal() {
    // OSC 52 sets the clipboard on terminals that support it, and BEL rings the
    // bell. Defined here rather than shared with the scenario above, which writes
    // the same bytes as JSON escapes. A shared constant would be unused on
    // platforms where this scenario is not compiled.
    let name = "evil\u{1b}]52;c;aGk=\u{7}.txt".to_owned();
    let scenario = Scenario::new("escape-name").file(&name, "hello\n");
    let mut editor = scenario.launch(&[&name]);

    assert_nothing_executable(&editor.screen());
}

#[test]
fn a_line_of_wide_characters_does_not_paint_past_the_right_hand_edge() {
    // Each character is two columns wide. A renderer that counts characters
    // instead of columns would draw twice the available width.
    let scenario = Scenario::new("wide")
        .size(20, 6)
        .file("a.txt", "漢字漢字漢字漢字漢字漢字漢字漢字\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.screen().assert_fits();
}

#[test]
fn syntax_highlighting_colours_a_keyword_differently_from_a_name() {
    // Highlighting, checked on screen: two tokens on one line are drawn in
    // different colours.
    let scenario = Scenario::new("highlight").file("a.rs", "fn greet() {}\n");
    let mut editor = scenario.launch(&["a.rs"]);

    let screen = editor.screen();
    let row = screen
        .row_of("fn greet")
        .expect("the line should be on screen");
    let line = screen.line(row);
    let keyword = line.find("fn").expect("the keyword");
    let name = line.find("greet").expect("the name");

    let (keyword_colour, _) = screen.colours_at(row, keyword).expect("a colour");
    let (name_colour, _) = screen.colours_at(row, name).expect("a colour");
    assert_ne!(
        (keyword_colour.r, keyword_colour.g, keyword_colour.b),
        (name_colour.r, name_colour.g, name_colour.b),
        "`fn` and `greet` should not be the same colour{}",
        screen.dump()
    );
}
