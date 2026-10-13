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
