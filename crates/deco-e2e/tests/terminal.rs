//! The integrated terminal, with a real shell in a real pseudo-terminal.
//!
//! The shell is `/bin/sh` with a fixed prompt, set through a user profile, so
//! the scenarios do not depend on the login shell or its configuration of
//! whoever runs them. Unix only: the Windows pseudo console is covered by the
//! unit tests in `deco-term`.

#![cfg(unix)]

use deco_e2e::Scenario;

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
fn a_command_typed_in_the_terminal_runs_in_the_workspace() {
    let scenario = with_sh("terminal-run");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+`");
    editor.settle_until("the prompt", |editor| {
        terminal_text(editor).contains("ready$")
    });
    editor.type_text("echo \"got:$TERM\"; ls\n");
    editor.settle_until("the command's output", |editor| {
        terminal_text(editor).contains("got:xterm-256color")
    });
    editor.settle_until("the listing", |editor| {
        terminal_text(editor).contains("a.txt")
    });

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("TERMINAL");
    screen.assert_shows("got:xterm-256color");
    assert_eq!(editor.text(), "text\n", "the document was not typed into");
    assert_eq!(editor.on_disk("a.txt"), "text\n");
}

#[test]
fn the_shell_exiting_is_reported_and_a_key_closes_the_terminal() {
    let scenario = with_sh("terminal-exit");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+`");
    editor.settle_until("the prompt", |editor| {
        terminal_text(editor).contains("ready$")
    });
    editor.type_text("exit 4\n");
    editor.settle_until("the exit", |editor| {
        editor
            .status()
            .is_some_and(|status| status.contains("code 4"))
    });
    editor.press("x");
    assert!(editor.session().terminal().is_none());
    editor.screen().assert_lacks("TERMINAL");
    assert_eq!(editor.text(), "text\n");
}

#[test]
fn ctrl_backtick_moves_between_the_text_and_a_running_shell() {
    let scenario = with_sh("terminal-toggle");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+`");
    editor.settle_until("the prompt", |editor| {
        terminal_text(editor).contains("ready$")
    });
    // Hidden, and the next key edits the text.
    editor.press("ctrl+`");
    editor.type_text("x");
    assert_eq!(editor.text(), "xtext\n");
    // Shown again, with the same shell.
    editor.press("ctrl+`");
    editor.type_text("echo again\n");
    editor.settle_until("the same shell", |editor| {
        terminal_text(editor).contains("again\nready$")
    });
}

#[test]
fn a_second_terminal_runs_its_own_shell_and_each_keeps_its_screen() {
    let scenario = with_sh("terminal-two");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+`");
    editor.settle_until("the first prompt", |editor| {
        terminal_text(editor).contains("ready$")
    });
    editor.type_text("echo one\n");
    editor.settle_until("the first output", |editor| {
        terminal_text(editor).contains("one\nready$")
    });

    editor.press("ctrl+shift+`");
    editor.settle_until("the second prompt", |editor| {
        editor.session().terminals().len() == 2 && terminal_text(editor).contains("ready$")
    });
    assert!(
        !terminal_text(&editor).contains("one"),
        "a screen of its own"
    );
    editor.screen().assert_shows("TERMINAL 2/2");
    editor.type_text("echo two\n");
    editor.settle_until("the second output", |editor| {
        terminal_text(editor).contains("two\nready$")
    });

    // Back to the first, whose shell and screen are as they were.
    editor.press("ctrl+pagedown");
    editor.screen().assert_shows("TERMINAL 1/2");
    assert!(terminal_text(&editor).contains("one\nready$"));
    assert!(!terminal_text(&editor).contains("two"));

    // Killing it leaves the second.
    editor.palette("Terminal: Kill the Active Terminal Instance");
    assert_eq!(editor.session().terminals().len(), 1);
    assert!(terminal_text(&editor).contains("two"));
    editor.screen().assert_shows("TERMINAL");
    editor.screen().assert_lacks("TERMINAL 1/");
}
