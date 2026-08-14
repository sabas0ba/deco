//! What the editor looks like: themes off the disk, and a screen that keeps its
//! shape whatever size the terminal is.
//!
//! The renderer has unit tests, and they are about layout given a session. These
//! are about the other half — that a theme file in an extension directory is
//! found, read and applied, and that the frame is still exactly as big as the
//! terminal after the things that resize it.

use deco_e2e::Scenario;

/// A theme file with an unmistakable background.
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
    // The whole marketplace-compatibility claim, end to end: a directory that
    // looks like an installed VS Code theme becomes an entry in `ctrl+k ctrl+t`,
    // and choosing it repaints the screen.
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
    // And the editor is still usable in whatever theme it fell back to.
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
    // A row short leaves whatever was underneath on screen; a row too wide wraps
    // and pushes the whole frame up. Both are the kind of thing that only shows
    // up on somebody else's terminal.
    for (width, height) in [(80, 24), (40, 10), (200, 60), (20, 5), (120, 3)] {
        let scenario = Scenario::new(&format!("size-{width}x{height}"))
            .size(width, height)
            .file("a.txt", "one\ntwo\nthree\nfour\nfive\n");
        let mut editor = scenario.launch(&["a.txt"]);
        editor.screen().assert_fits();

        // And with the chrome that costs rows.
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
    // People do drag a terminal down to nothing, and an editor that panics there
    // takes the unsaved file with it.
    let scenario = Scenario::new("tiny")
        .size(1, 1)
        .file("a.txt", "hello\nworld\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.screen().assert_fits();
    editor.type_text("x");
    editor.press("ctrl+f");
    editor.screen().assert_fits();
}

#[test]
fn a_file_whose_name_carries_an_escape_sequence_cannot_reach_the_terminal() {
    // A file name is somebody else's text — a cloned repository can contain
    // anything — and a terminal executes `\x1b]52;c;…` as a clipboard write. The
    // renderer substitutes it; this is the check that the substitution is on the
    // path a real file name takes.
    let scenario = Scenario::new("escape-name").file("evil\u{1b}]52;c;aGk=\u{7}.txt", "hello\n");
    let mut editor = scenario.launch(&["evil\u{1b}]52;c;aGk=\u{7}.txt"]);

    let screen = editor.screen();
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
fn a_line_of_wide_characters_does_not_paint_past_the_right_hand_edge() {
    // Two columns per character, and a renderer that counts characters instead of
    // columns paints twice the width it has.
    let scenario = Scenario::new("wide")
        .size(20, 6)
        .file("a.txt", "漢字漢字漢字漢字漢字漢字漢字漢字\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.screen().assert_fits();
}

#[test]
fn syntax_highlighting_colours_a_keyword_differently_from_a_name() {
    // The claim the highlighting makes, at the level anybody can see it: two
    // things on one line are painted in two colours.
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
