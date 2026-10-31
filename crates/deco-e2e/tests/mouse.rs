//! The mouse, as a terminal reports it: clicks, double clicks, drags and the
//! wheel, on what the screen shows.
//!
//! Each scenario finds what it clicks on in the drawn screen, so it tests
//! that a click lands on what is drawn there, which is the part that would
//! break if the hit testing and the renderer disagreed.

use deco_e2e::Scenario;

fn caret(editor: &deco_e2e::Editor) -> (u32, u32) {
    let at = editor.session().view.cursor();
    (at.line, at.character)
}

fn selected(editor: &deco_e2e::Editor) -> String {
    let session = editor.session();
    let primary = session.view.selections.primary();
    session
        .document
        .buffer
        .text_in_range(deco_core::Range::new(primary.start(), primary.end()))
}

#[test]
fn a_click_moves_the_caret_and_a_double_click_selects_a_word() {
    let scenario = Scenario::new("mouse-click").file("a.txt", "alpha beta\ngamma delta\n");
    let mut editor = scenario.launch(&["a.txt"]);

    let (col, row) = editor.screen().cell_of("delta").expect("on screen");
    editor.click(col + 2, row);
    assert_eq!(caret(&editor), (1, 8));

    editor.pause().click(col + 1, row).click(col + 1, row);
    assert_eq!(selected(&editor), "delta");
    editor.type_text("omega");
    assert_eq!(editor.text(), "alpha beta\ngamma omega\n");
}

#[test]
fn a_drag_selects_and_shift_click_extends() {
    let scenario = Scenario::new("mouse-drag").file("a.txt", "alpha beta\ngamma delta\n");
    let mut editor = scenario.launch(&["a.txt"]);
    let (col, row) = editor.screen().cell_of("beta").expect("on screen");
    editor.drag((col, row), (col + 2, row + 1));
    // To two columns past where `beta` starts, on the next line.
    assert_eq!(selected(&editor), "beta\ngamma de");
    editor.pause().click(col, row).shift_click(col + 4, row);
    assert_eq!(selected(&editor), "beta");
}

#[test]
fn the_wheel_scrolls_the_text_and_leaves_the_caret() {
    let text: String = (0..200).map(|n| format!("line {n}\n")).collect();
    let scenario = Scenario::new("mouse-wheel").file("a.txt", &text);
    let mut editor = scenario.launch(&["a.txt"]);
    editor.wheel(5, 2, 4);
    editor
        .screen()
        .assert_lacks("line 0\n")
        .assert_shows("line 12");
    assert!(!editor
        .screen()
        .lines()
        .iter()
        .any(|l| l.contains("line 11 ")));
    assert_eq!(caret(&editor), (0, 0));
}

#[test]
fn a_tab_click_shows_its_file_and_a_tree_click_opens_one() {
    let scenario = Scenario::new("mouse-tabs")
        .file("a.txt", "first\n")
        .file("b.txt", "second\n");
    let mut editor = scenario.launch(&["a.txt", "b.txt"]);
    let (col, row) = editor.screen().cell_of(" a.txt").expect("a tab bar");
    editor.click(col + 1, row);
    assert_eq!(editor.text(), "first\n");

    editor.press("ctrl+b");
    let (col, row) = editor.screen().cell_of("b.txt").expect("in the tree");
    editor.pause().click(col, row);
    assert_eq!(editor.text(), "second\n");
}

#[cfg(unix)]
mod terminal {
    use super::*;

    fn with_sh(name: &str) -> Scenario {
        Scenario::new(name)
            .user_settings(
                r#"{
                    "terminal.integrated.defaultProfile.linux": "sh",
                    "terminal.integrated.defaultProfile.osx": "sh",
                    "terminal.integrated.profiles.linux": { "sh": { "path": "/bin/sh", "env": { "PS1": "ready$ " } } },
                    "terminal.integrated.profiles.osx": { "sh": { "path": "/bin/sh", "env": { "PS1": "ready$ " } } }
                }"#,
            )
            .file("a.txt", "text\n")
    }

    fn terminal_text(editor: &deco_e2e::Editor) -> String {
        editor
            .session()
            .terminal()
            .map(|terminal| terminal.screen().text())
            .unwrap_or_default()
    }

    #[test]
    fn text_dragged_over_in_the_terminal_is_copied_and_pasted() {
        let scenario = with_sh("mouse-terminal-copy");
        let mut editor = scenario.launch(&["a.txt"]);
        editor.press("ctrl+`");
        editor.settle_until("the prompt", |e| terminal_text(e).contains("ready$"));
        editor.type_text("echo marker42\n");
        editor.settle_until("the output", |e| {
            terminal_text(e).contains("marker42\nready$")
        });

        let screen = editor.screen();
        // The output line, not the command line that also shows it.
        let row = screen
            .lines()
            .iter()
            .rposition(|line| line.trim_start().starts_with("marker42"))
            .expect("the output on screen") as u16;
        let col = screen.lines()[row as usize].find("marker42").unwrap() as u16;
        editor.drag((col, row), (col + 7, row));
        editor.press("ctrl+shift+c");
        // Back to the text, which the click there also does.
        let (col, row) = editor.screen().cell_of("text").expect("the document");
        editor.pause().click(col, row).press("ctrl+v");
        assert_eq!(editor.text(), "marker42text\n");
    }

    #[test]
    fn a_program_that_asks_for_the_mouse_receives_it() {
        let scenario = with_sh("mouse-terminal-program");
        let mut editor = scenario.launch(&["a.txt"]);
        editor.press("ctrl+`");
        editor.settle_until("the prompt", |e| terminal_text(e).contains("ready$"));
        // Mouse tracking in the SGR form, then `od` shows what arrives.
        editor.type_text("printf '\\033[?1000h\\033[?1006h'; od -c\n");
        editor.settle_until("tracking on", |e| {
            e.session()
                .terminal()
                .is_some_and(|t| t.screen().modes().mouse != Default::default())
        });
        let rect = editor.session().regions().panel.expect("the panel");
        // Row 1 of the screen, column 3: below the panel's heading.
        let top = deco_tui::render::tab_bar_height(editor.session()) as u16;
        editor.click(rect.x as u16 + 3, top + rect.y as u16 + 2);
        // The bytes are a line without its end: the first `ctrl+d` hands it to
        // `od`, the second ends its input.
        editor.press("ctrl+d").press("ctrl+d");
        editor.settle_until("the bytes", |e| {
            terminal_text(e).contains("<   0   ;   4   ;   2   M")
        });
    }
}
