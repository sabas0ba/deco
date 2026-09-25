//! A language server connected through a pipe, and the keys that use it.
//!
//! `deco-lsp` has 285 tests and `deco-tui::lsp` has its own. Together they cover
//! the protocol and the effect of a response on a session. They do not test the
//! whole chain: a server definition in `settings.json`, a process started from
//! it, a capability converted to a context key, a keybinding conditional on that
//! key, a request, a response arriving on a later poll, and a change on the
//! screen. Six components must work together, and each is tested only against
//! its own assumptions about the other five.
//!
//! These scenarios configure a real server as a user would, press the key bound
//! to the feature, and wait for the response as the editor does. The server is
//! `examples/language_server.rs`.

use deco_e2e::{Editor, Scenario};

/// A file with symbols to query, and a server for it.
fn project(name: &str, role: &str) -> Scenario {
    Scenario::new(name)
        .language_server("rust", role)
        .file(
            "src/main.rs",
            "fn main() {\n    greet(\"world\");\n}\n\nfn greet(who: &str) {}\n",
        )
        .file("src/notes.md", "# not rust\n")
}

/// Launched, with the handshake done.
fn started(scenario: &Scenario) -> Editor {
    let mut editor = scenario.launch(&["src/main.rs"]);
    editor.settle_lsp();
    editor
}

#[test]
fn a_server_defined_in_settings_is_started_and_says_hello() {
    let scenario = project("lsp-start", "full");
    let editor = started(&scenario);

    assert!(
        editor.problems().is_empty(),
        "starting a working server should be quiet: {:?}",
        editor.problems()
    );
    // The announced capabilities became context keys, which determine whether
    // the keys below are bound.
    for key in [
        "editorHasDefinitionProvider",
        "editorHasReferenceProvider",
        "editorHasDocumentSymbolProvider",
        "editorHasDocumentFormattingProvider",
        "editorHasHoverProvider",
    ] {
        assert_eq!(
            editor.session().context.get(key),
            Some(&serde_json::json!(true)),
            "{key} should be set from what the server offered"
        );
    }
}

#[test]
fn a_diagnostic_reaches_the_status_line() {
    let scenario = project("lsp-diagnostics", "diagnostics");
    let mut editor = started(&scenario);

    editor.settle_until("a diagnostic to arrive", |editor| {
        editor.session().diagnostic_counts().errors > 0
    });

    let screen = editor.screen();
    screen.assert_fits();
    // The error count, in the status line.
    screen.assert_status("×1");
}

#[test]
fn editing_replaces_the_diagnostics_rather_than_adding_to_them() {
    // A stale diagnostic is worse than none, because it points at a line that
    // has moved.
    let scenario = project("lsp-restated", "diagnostics");
    let mut editor = started(&scenario);
    editor.settle_until("the first diagnostic", |editor| {
        editor.session().diagnostic_counts().errors > 0
    });

    editor.press("ctrl+end");
    editor.type_text("// touched\n");
    editor.settle_until("the second round", |editor| {
        editor
            .session()
            .diagnostics_at(deco_core::position::Position::new(1, 3))
            .iter()
            .any(|d| d.message.contains("round 2"))
    });

    assert_eq!(
        editor.session().diagnostic_counts().errors,
        1,
        "the new answer should replace the old one, not stack on it"
    );
}

#[test]
fn f12_goes_to_the_definition_the_server_named() {
    let scenario = project("lsp-definition", "full");
    let mut editor = started(&scenario);

    editor.press("f12");
    editor.settle_until("the caret to move", |editor| {
        editor.session().view.selections.primary().active.line == 2
    });

    // Line 3 on screen is line 2 in the protocol, which counts from zero. This
    // checks the conversion between the two.
    editor.screen().assert_status("Ln 3");
}

#[test]
fn hover_shows_what_the_server_said() {
    let scenario = project("lsp-hover", "full");
    let mut editor = started(&scenario);

    editor.press("ctrl+k");
    editor.press("ctrl+i");
    editor.settle_until("the hover to arrive", |editor| {
        editor.driver().lsp().hover().is_some()
    });

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("says hello to somebody");
}

#[test]
fn a_server_offering_no_hover_leaves_the_key_doing_nothing_quietly() {
    // The context key is false, so `ctrl+k ctrl+i` is not bound. Nothing should
    // happen and no message should be shown, because an unbound key is not a
    // failure.
    let scenario = project("lsp-no-hover", "no-hover");
    let mut editor = started(&scenario);

    assert_eq!(
        editor.session().context.get("editorHasHoverProvider"),
        Some(&serde_json::json!(false))
    );
    editor.press("ctrl+k");
    editor.press("ctrl+i");

    assert!(editor.driver().lsp().hover().is_none());
    editor.screen().assert_fits();
}

#[test]
fn references_are_listed_and_choosing_one_goes_there() {
    let scenario = project("lsp-references", "full");
    let mut editor = started(&scenario);

    editor.press("shift+f12");
    editor.settle_until("the reference list", |editor| {
        editor.session().prompt.is_some()
    });

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("main.rs");

    editor.press("enter");
    editor.screen().assert_fits();
}

/// The same project, plus a second file that mentions `greet`.
///
/// No scenario opens it. It gives a rename a file that is not open, which is
/// the case the workspace-edit path is for.
fn rename_project(name: &str, role: &str) -> Scenario {
    project(name, role).file("src/helper.rs", "fn call() {\n    greet(\"x\");\n}\n")
}

/// Puts the caret on `greet` in the call on line 2.
fn on_the_symbol(editor: &mut Editor) {
    editor.press("ctrl+home");
    editor.press("down");
    editor.press_times("right", 5);
}

#[test]
fn f2_renames_across_files_and_one_undo_takes_it_back() {
    let scenario = rename_project("lsp-rename", "full");
    let mut editor = started(&scenario);
    on_the_symbol(&mut editor);

    editor.press("f2");
    assert!(
        editor.session().prompt.is_some(),
        "f2 should open the rename prompt: {:?}",
        editor.status()
    );
    editor.type_text("hello");
    editor.press("enter");
    editor.settle_until("the rename to land", |editor| {
        editor
            .status()
            .is_some_and(|line| line.starts_with("Renamed"))
    });

    // The open file: both occurrences changed, and nothing else.
    assert_eq!(
        editor.text(),
        "fn main() {\n    hello(\"world\");\n}\n\nfn hello(who: &str) {}\n"
    );

    // The file that was not open: changed, kept unsaved, and not written.
    let unsaved = editor.session().unsaved();
    let helper = unsaved
        .iter()
        .find(|(path, _)| path.ends_with("helper.rs"))
        .map(|(_, text)| text.clone())
        .expect("the rename should have opened helper.rs and left it dirty");
    assert_eq!(helper, "fn call() {\n    hello(\"x\");\n}\n");
    assert_eq!(
        editor.on_disk("src/helper.rs"),
        "fn call() {\n    greet(\"x\");\n}\n",
        "nothing should reach the disk until the user saves"
    );

    // One undo reverts the rename in both files.
    editor.press("ctrl+z");
    assert_eq!(
        editor.text(),
        "fn main() {\n    greet(\"world\");\n}\n\nfn greet(who: &str) {}\n"
    );
    let after_undo = editor.session().unsaved();
    let helper = after_undo
        .iter()
        .find(|(path, _)| path.ends_with("helper.rs"))
        .map(|(_, text)| text.clone());
    assert_eq!(
        helper.as_deref(),
        Some("fn call() {\n    greet(\"x\");\n}\n"),
        "the file that was not on screen should have come back too"
    );
    editor.screen().assert_fits();
}

#[test]
fn renaming_to_the_same_name_asks_the_server_nothing() {
    // The prompt opens with the current name selected, so pressing enter
    // immediately is a common mistake. Sending an edit per occurrence would
    // mark every file that mentions the symbol dirty without changing it.
    let scenario = rename_project("lsp-rename-same", "full");
    let mut editor = started(&scenario);
    on_the_symbol(&mut editor);

    editor.press("f2");
    editor.press("enter");

    assert_eq!(editor.status(), Some("`greet` is already its name"));
    assert!(!editor.is_dirty(), "nothing should have been changed");
}

#[test]
fn a_server_offering_no_rename_leaves_f2_doing_nothing_quietly() {
    let scenario = rename_project("lsp-rename-absent", "no-rename");
    let mut editor = started(&scenario);
    on_the_symbol(&mut editor);

    editor.press("f2");

    // The `when` clause is `editorHasRenameProvider`, so the key resolves to no
    // command, rather than to a command that reports an error.
    assert!(
        editor.session().prompt.is_none(),
        "no prompt should open without a rename provider"
    );
    assert!(!editor.is_dirty());
}

#[test]
fn ctrl_dot_lists_what_the_server_offers_and_applies_the_one_chosen() {
    let scenario = project("lsp-code-actions", "full");
    let mut editor = started(&scenario);
    editor.press("ctrl+home");
    editor.press("down");

    editor.press("ctrl+.");
    editor.settle_until("the action list", |editor| {
        editor.session().prompt.is_some()
    });

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("Prefix the name with an underscore");
    // The second column: the kind for available actions, and the reason for the
    // unavailable one.
    screen.assert_shows("quickfix");
    screen.assert_shows("not on a variable");

    // The first action is selected, and it arrived with its edit.
    editor.press("enter");
    editor.settle_until("the edit to land", |editor| editor.is_dirty());
    assert_eq!(
        editor.text(),
        "fn main() {\n    _greet(\"world\");\n}\n\nfn greet(who: &str) {}\n"
    );
}

#[test]
fn an_action_with_no_edit_is_resolved_before_it_is_applied() {
    // The second entry arrives with `data` and no edit. Choosing it must send
    // the action to `codeAction/resolve` and apply the result, rather than
    // report that there is nothing to do.
    let scenario = project("lsp-code-action-resolve", "full");
    let mut editor = started(&scenario);

    editor.press("ctrl+.");
    editor.settle_until("the action list", |editor| {
        editor.session().prompt.is_some()
    });
    editor.press("down");
    editor.press("enter");
    editor.settle_until("the resolved edit", |editor| editor.is_dirty());

    assert!(
        editor.text().starts_with("// extracted\n"),
        "the resolved edit should have been applied: {:?}",
        editor.text()
    );
}

#[test]
fn an_action_that_only_runs_a_server_command_is_refused_by_name() {
    let scenario = project("lsp-code-action-command", "full");
    let mut editor = started(&scenario);

    editor.press("ctrl+.");
    editor.settle_until("the action list", |editor| {
        editor.session().prompt.is_some()
    });
    // The fourth entry: a bare `Command`.
    editor.press_times("down", 3);
    editor.press("enter");
    editor.settle_until("the refusal", |editor| {
        editor
            .status()
            .is_some_and(|line| line.contains("was not applied"))
    });

    let status = editor.status().unwrap_or_default().to_owned();
    assert!(
        status.contains("example.organizeImports"),
        "the refusal should name the command it cannot run: {status}"
    );
    assert!(!editor.is_dirty(), "and nothing should have changed");
}

#[test]
fn a_server_offering_no_code_actions_leaves_the_key_doing_nothing_quietly() {
    // Only this example server sends `codeActionProvider`, so the context key is
    // false in a scenario without a server.
    let scenario = Scenario::new("lsp-code-actions-absent").file("a.txt", "plain\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+.");

    assert!(editor.session().prompt.is_none());
    assert_eq!(editor.status(), None, "an unbound key did not fail");
}

#[test]
fn go_to_symbol_lists_what_the_server_classified() {
    let scenario = project("lsp-symbols", "full");
    let mut editor = started(&scenario);

    editor.press("ctrl+shift+o");
    editor.settle_until("the symbol list", |editor| {
        editor.session().prompt.is_some()
    });

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("greet");
}

#[test]
fn completion_offers_what_the_server_sent_and_accepting_one_types_it() {
    // Uses a keybinding defined by this scenario, which also tests that a
    // `keybindings.json` entry reaches the command. The scenario below presses
    // the default `ctrl+space`.
    let scenario = project("lsp-completion", "full").user_keybindings(
        r#"[{ "key": "ctrl+e", "command": "editor.action.triggerSuggest", "when": "editorTextFocus" }]"#,
    );
    let mut editor = started(&scenario);

    editor.press("ctrl+end");
    editor.type_text("gre");
    editor.press("ctrl+e");
    editor.settle_until("the completion list", |editor| {
        editor.driver().lsp().suggest().is_some()
    });

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("greet_loudly");

    editor.press("tab");
    assert!(
        editor.text().contains("greet_loudly"),
        "accepting a completion should type it: {:?}",
        editor.text()
    );
}

#[test]
fn ctrl_space_reaches_trigger_suggest_in_a_terminal() {
    // `ctrl+space` is deco's default binding for `editor.action.triggerSuggest`.
    // It previously had no effect in a terminal. A terminal sends NUL for
    // Ctrl+Space, and crossterm parses that byte as `KeyCode::Char(' ')` with
    // CONTROL, which became `Key::Char(' ')`. The binding parsed to
    // `Key::Named(Space)`, which the terminal path produced only from
    // `KeyCode::Null`, and crossterm's unix parser never emits that. The two
    // keys never matched, and no error was shown, so the bug was hard to find.
    //
    // `space` now has a single representation, so the default binding works
    // and the scenario above does not need its own binding.
    let scenario = project("lsp-ctrl-space", "full");
    let mut editor = started(&scenario);

    editor.press("ctrl+end");
    editor.type_text("gre");
    editor.press("ctrl+space");
    editor.settle_until("the completion list", |editor| {
        editor.driver().lsp().suggest().is_some()
    });

    editor.screen().assert_shows("greet_loudly");
}

#[test]
fn formatting_applies_the_edit_where_the_server_put_it() {
    // A one-line insert at the top rather than a whole-document rewrite. An edit
    // applied at the wrong offset would pass a test that only checks that the
    // file changed.
    let scenario = project("lsp-format", "full");
    let mut editor = started(&scenario);

    editor.press("ctrl+shift+i");
    editor.settle_until("the formatting edit", |editor| {
        editor.text().starts_with("// formatted")
    });

    assert!(
        editor.text().starts_with("// formatted\nfn main() {"),
        "the edit should land at the top and keep the rest: {:?}",
        editor.text()
    );
    editor.press("ctrl+s");
    assert!(editor.on_disk("src/main.rs").starts_with("// formatted\n"));
}

#[test]
fn a_file_in_another_language_does_not_get_this_servers_answers() {
    // The server is registered for Rust. Opening Markdown must not send it the
    // file, and must not leave the Rust file's diagnostics on screen either.
    let scenario = project("lsp-other-language", "diagnostics");
    let mut editor = started(&scenario);
    editor.settle_until("the rust diagnostic", |editor| {
        editor.session().diagnostic_counts().errors > 0
    });

    editor.quick_open("notes.md");
    assert!(editor.path().is_some_and(|p| p.ends_with("notes.md")));

    editor.settle_until("the diagnostics to be dropped", |editor| {
        editor.session().diagnostic_counts().is_empty()
    });
    editor.screen().assert_fits();
}

#[test]
fn a_server_a_cloned_repository_asks_for_is_not_run() {
    // The most important case for security. `.vscode/settings.json` comes with
    // a cloned repository, and a server definition is a command line. Opening a
    // file in a cloned repository must not run a command the repository
    // defines. The refusal must be reported, or the editor appears broken.
    //
    // No other server is registered for this language, so the built-in registry
    // has no candidate and the refusal is the only thing to report.
    let program = std::env::current_exe()
        .expect("this test binary")
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target/<profile>")
        .join("examples")
        .join(format!("language_server{}", std::env::consts::EXE_SUFFIX));
    let definition = serde_json::json!({
        "deco.lsp.servers": {
            "hostile": {
                "languages": ["markdown"],
                "command": program.to_string_lossy(),
                "args": ["full"],
            },
        },
    });

    let scenario = Scenario::new("lsp-untrusted")
        .language_servers(true)
        .user_settings(r#"{ "deco.lsp.enabled": true }"#)
        .workspace_settings(&serde_json::to_string(&definition).expect("serialisable"))
        .file("notes.md", "# hello\n");
    let mut editor = scenario.launch(&["notes.md"]);

    // Allow time for the server to start, then check that it did not.
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        editor.wait(5);
    }
    assert!(
        !editor.driver().lsp().is_ready(),
        "a workspace-defined server was started"
    );
    let screen = editor.screen();
    assert!(
        screen.status_line().contains("hostile"),
        "the refusal should name the server it refused{}",
        screen.dump()
    );
}

#[test]
fn a_document_the_server_never_saw_is_opened_when_it_becomes_current() {
    // Two tabs, one server. Switching tabs must close one document on the
    // server and open the other, or the responses describe the wrong file.
    let scenario = project("lsp-tabs", "diagnostics").file("src/other.rs", "fn other() {}\n");
    let mut editor = scenario.launch(&["src/main.rs", "src/other.rs"]);
    editor.settle_lsp();
    editor.settle_until("the first file's diagnostic", |editor| {
        editor.session().diagnostic_counts().errors > 0
    });

    editor.press("ctrl+tab");
    assert!(editor.path().is_some_and(|p| p.ends_with("other.rs")));
    editor.settle_until("the second file's diagnostic", |editor| {
        editor.session().diagnostic_counts().errors > 0
    });

    editor.screen().assert_fits();
}

#[test]
fn a_refused_server_is_named_even_when_the_users_own_one_starts() {
    // The common case: a user with their own server for a language clones a
    // repository that defines another. The user's server starts and the
    // repository's is refused. The refusal must still be reported, because the
    // user can only choose to move the definition into their own settings if
    // they know about it.
    //
    // `Lsp::attach` previously collected refusals while iterating over the
    // candidates and reported them after the loop, but the loop `return`s as
    // soon as a trusted candidate starts. The report was therefore reached only
    // when every candidate was refused, which is the case where the user has no
    // server of their own.
    let program = std::env::current_exe()
        .expect("this test binary")
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target/<profile>")
        .join("examples")
        .join(format!("language_server{}", std::env::consts::EXE_SUFFIX));
    let hostile = serde_json::json!({
        "deco.lsp.servers": {
            "hostile": {
                "languages": ["rust"],
                "command": program.to_string_lossy(),
                "args": ["full"],
            },
        },
    });

    let scenario = project("lsp-refused-and-started", "full")
        .workspace_settings(&serde_json::to_string(&hostile).expect("serialisable"));
    let editor = started(&scenario);

    // The user's own server started. This part already worked before the fix.
    assert!(editor.driver().lsp().is_ready());

    let problems = editor.problems();
    assert!(
        problems.iter().any(|problem| problem.contains("hostile")),
        "the refusal should be named in the problem list: {problems:?}"
    );
    // In the problem list, not the status bar. `attach` runs on every tab switch
    // and language change, and a repeated status message would replace the
    // status of the server that is running.
    assert!(
        !editor
            .status()
            .is_some_and(|status| status.contains("hostile")),
        "the status bar belongs to the running server here: {:?}",
        editor.status()
    );
}

#[test]
fn a_refusal_is_recorded_once_however_often_attach_runs() {
    // `attach` is called on every tab switch and language change. Appending the
    // message each time would fill the problem list with duplicates and make
    // `--print-config` unreadable.
    let program = std::env::current_exe()
        .expect("this test binary")
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target/<profile>")
        .join("examples")
        .join(format!("language_server{}", std::env::consts::EXE_SUFFIX));
    let definition = serde_json::json!({
        "deco.lsp.servers": {
            "hostile": {
                "languages": ["markdown"],
                "command": program.to_string_lossy(),
                "args": ["full"],
            },
        },
    });

    let scenario = Scenario::new("lsp-refused-once")
        .language_servers(true)
        .user_settings(r#"{ "deco.lsp.enabled": true }"#)
        .workspace_settings(&serde_json::to_string(&definition).expect("serialisable"))
        .file("notes.md", "# hello\n")
        .file("more.md", "# more\n");
    let mut editor = scenario.launch(&["notes.md", "more.md"]);

    for _ in 0..4 {
        editor.press("ctrl+tab");
        editor.wait(5);
    }

    let mentions = editor
        .problems()
        .iter()
        .filter(|problem| problem.contains("hostile"))
        .count();
    assert_eq!(mentions, 1, "{:?}", editor.problems());
}
