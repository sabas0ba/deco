//! A language server on the other end of a pipe, and the keys that reach it.
//!
//! `deco-lsp` has 285 tests and `deco-tui::lsp` has its own; between them they
//! cover the protocol and what an answer *does* to a session. What neither can
//! say is whether the chain holds: a server definition in `settings.json`, a
//! process started from it, a capability turned into a context key, a keybinding
//! gated on that context key, a request, an answer arriving on a later poll, and
//! something different on the screen. Six components have to agree, and each of
//! them is tested against its own idea of the other five.
//!
//! So these scenarios configure a real server the way a user would, press the
//! key the feature is bound to, and wait for the answer the way the editor waits
//! for it. The server is `examples/language_server.rs`.

use deco_e2e::{Editor, Scenario};

/// A file with something worth asking about in it, and a server for it.
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
    // The capabilities it announced became context keys, which is what decides
    // whether the keys below are bound to anything at all.
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
    // The count, where the editor puts counts.
    screen.assert_status("×1");
}

#[test]
fn editing_replaces_the_diagnostics_rather_than_adding_to_them() {
    // A stale diagnostic is worse than none: it points at a line that has moved.
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

    // Line 3 on screen is line 2 to the protocol, which counts from zero — and
    // the fact that those are different is exactly what this is checking.
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
    // happen — and nothing should be *said*, because a key that was never bound
    // did not fail.
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
/// Never opened by any of these scenarios: it is there so that a rename has
/// somewhere to reach that the user is not looking at, which is the case the
/// whole workspace-edit path exists for.
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

    // The file on screen: both occurrences, and nothing else touched.
    assert_eq!(
        editor.text(),
        "fn main() {\n    hello(\"world\");\n}\n\nfn hello(who: &str) {}\n"
    );

    // The file nobody opened: changed, held unsaved, and *not* written.
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

    // And one keystroke takes the whole thing back, in both files.
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
    // The prompt opens with the current name selected, so enter on its own is an
    // ordinary slip. Answering it with an edit per occurrence would mark every
    // file that mentions the symbol dirty for no change at all.
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

    // The `when` clause is `editorHasRenameProvider`, so the key resolves to
    // nothing at all — not to a command that then apologises.
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
    // The second column: the kind for the ones that can run, and the reason for
    // the one that cannot.
    screen.assert_shows("quickfix");
    screen.assert_shows("not on a variable");

    // The first is selected, and it arrived with its edit already on it.
    editor.press("enter");
    editor.settle_until("the edit to land", |editor| editor.is_dirty());
    assert_eq!(
        editor.text(),
        "fn main() {\n    _greet(\"world\");\n}\n\nfn greet(who: &str) {}\n"
    );
}

#[test]
fn an_action_with_no_edit_is_resolved_before_it_is_applied() {
    // The second entry arrives with `data` and no edit. Choosing it has to send
    // that action back and apply what comes home, rather than reporting that
    // there is nothing to do.
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
    // `codeActionProvider` is only sent by this example server, so a scenario
    // without a server at all is the case where the context key is false.
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
    // Reached through a binding of this scenario's own, which also covers a
    // `keybindings.json` entry reaching the command; the default `ctrl+space` is
    // pressed by the scenario below.
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
    // `ctrl+space` is deco's default binding for `editor.action.triggerSuggest`,
    // and it used to do nothing at all in a terminal. A terminal sends NUL for
    // Ctrl+Space; crossterm parses that byte into `KeyCode::Char(' ')` with
    // CONTROL, which became `Key::Char(' ')` — while the binding parsed to
    // `Key::Named(Space)`, a key the terminal path only ever produced from
    // `KeyCode::Null`, which crossterm's unix parser never emits. The two never
    // met, and nothing was said, which is what made it hard to diagnose.
    //
    // Now `space` is one key in one representation, so the default binding is
    // pressable and the scenario above no longer needs a binding of its own.
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
    // A one-line insert at the top rather than a whole-document rewrite: an edit
    // applied at the wrong offset still passes a test that only checks the file
    // changed.
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
    // The one that matters most. `.vscode/settings.json` arrives with somebody
    // else's repository, and a server definition is a command line. Cloning a
    // repository and opening a file in it must not execute what that repository
    // chose — and the refusal has to be said out loud, or it reads as the editor
    // being broken.
    //
    // The language is one nothing else claims, so that the built-in registry has
    // no candidate of its own here and the refusal is the only thing to report.
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

    // Given every chance to start, and it must not have.
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
    // Two tabs, one server. Switching tabs has to close one document with the
    // server and open the other, or the answers describe the wrong file.
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
    // The ordinary case: somebody who has configured a server of their own for a
    // language clones a repository that defines its own. Their server starts,
    // correctly, and the repository's is declined, also correctly — and the
    // decline still has to be said, because "move it into your own settings if
    // you do want it" is a decision the user cannot make without being told.
    //
    // `Lsp::attach` used to collect refusals as it walked the candidates and
    // report them after the loop, and the loop `return`s as soon as a trusted
    // candidate starts. So the report was reached only when *every* candidate had
    // been refused — the one case where the user has no server of their own.
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

    // The user's own server did start, which is the half that always worked.
    assert!(editor.driver().lsp().is_ready());

    let problems = editor.problems();
    assert!(
        problems.iter().any(|problem| problem.contains("hostile")),
        "the refusal should be named in the problem list: {problems:?}"
    );
    // The problem list and not the status bar: `attach` runs on every tab switch
    // and language change, and a row that came back each time would push aside
    // whatever the editor was saying about the server that *is* running.
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
    // `attach` is called on every tab switch and language change. A disclosure
    // that appended each time would fill the problem list with copies of itself
    // and make `--print-config` unreadable.
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
