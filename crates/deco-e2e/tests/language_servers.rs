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
    // Reached through a binding of this scenario's own, because the default one
    // cannot be pressed — see the scenario below.
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
fn ctrl_space_never_reaches_trigger_suggest_in_a_terminal() {
    // A finding, pinned rather than asserted as good.
    //
    // `ctrl+space` is deco's default binding for `editor.action.triggerSuggest`,
    // and in a terminal it cannot fire. A terminal sends NUL for Ctrl+Space;
    // crossterm 0.29 parses that byte into `KeyCode::Char(' ')` with CONTROL
    // (`event/sys/unix/parse.rs`), and `deco_tui::keys::chord_from_event` turns
    // that into `Key::Char(' ')`. The binding parses to `Key::Named(Space)`,
    // which the terminal path only ever produces from `KeyCode::Null` — a code
    // crossterm's unix parser never emits.
    //
    // So the two never meet. The scenario above shows the feature itself is
    // fine: bound to any other key it works. The GUI frontend does map its
    // Space to `NamedKey::Space`, so the two frontends also disagree about what
    // the space bar is.
    let scenario = project("lsp-ctrl-space", "full");
    let mut editor = started(&scenario);

    editor.press("ctrl+end");
    editor.type_text("gre");
    editor.press("ctrl+space");

    // Given real time to answer, in case it were merely slow.
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        editor.wait(5);
    }
    assert!(
        editor.driver().lsp().suggest().is_none(),
        "ctrl+space reached trigger-suggest after all — this finding is fixed \
         and the scenario should become an assertion that it works"
    );
    // And nothing was said, which is what makes it hard to diagnose: the key
    // simply does nothing.
    assert_eq!(editor.status(), None);
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
fn a_refused_server_is_not_mentioned_when_the_users_own_one_starts() {
    // A finding, pinned rather than asserted as good.
    //
    // The refusal in the scenario above is only heard because nothing else
    // claimed the language. Here the user has a server of their own for the same
    // language, which is the ordinary case: somebody who has configured
    // `rust-analyzer` clones a repository that defines its own Rust server.
    //
    // Their server starts, correctly, and the repository's is declined, also
    // correctly — and nothing is said about the decline at all. `Lsp::attach`
    // collects refusals as it walks the candidates and reports them after the
    // loop, but the loop `return`s as soon as a trusted candidate is started or
    // fails to start. So the report is reached only when *every* candidate was
    // refused, which is the case where the user has no server of their own.
    //
    // The comment beside the refusal says it is named "so the user can decide to
    // move it into their own settings if they do want it" — which is the thing
    // that does not happen. The security decision is right; only the disclosure
    // is lost.
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

    // The user's own server did start, which is the half that works.
    assert!(editor.driver().lsp().is_ready());

    let said = format!("{:?} {:?}", editor.status(), editor.problems());
    assert!(
        !said.contains("hostile"),
        "the refusal is reported now — this finding is fixed, and the scenario \
         should become an assertion that it is said: {said}"
    );
}
