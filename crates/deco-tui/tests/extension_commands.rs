//! An extension found on disk, invoked by name, answered by deco.
//!
//! Everything else about this path is tested without a process: the catalogue
//! decides with no filesystem, `sandbox` builds an argv, `dispatch` is pure, and
//! `deco-ext`'s own round trip proves the wire — including inside a container. What
//! none of them prove is that the *editor* joins them up: that a directory becomes
//! a palette entry, that invoking the entry starts a host, that the host's reply
//! reaches the status bar, and that a capability the manifest never declared is
//! refused when a real extension really asks for it.
//!
//! `#[ignore]`d, so `cargo test` stays portable — this one needs Node. Run by
//! `cargo xtask host-test`, in the CI job that has one.
//!
//! The sandbox is `process` here on purpose. What this test is about is the
//! editor's half, and `deco-ext`'s round trip already starts the same stack inside
//! the pinned image; running a container here would test that twice and this once.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deco_config::{Scope, Settings};
use deco_editor::Session;
use deco_tui::extensions::{discover, rows, Hosts};

/// The repository root, from this crate's own location.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/deco-tui is two levels down")
        .to_path_buf()
}

/// An extensions directory holding one extension.
fn install(name: &str, manifest: &str, code: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("deco-ext-commands-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let directory = root.join("acme.tools-1.0.0");
    std::fs::create_dir_all(&directory).expect("a directory");
    std::fs::write(directory.join("package.json"), manifest).expect("a manifest");
    std::fs::write(directory.join("extension.js"), code).expect("an extension");
    root
}

/// Tells `Hosts` where deco's own host code is.
///
/// A test binary lives in `target/debug/deps`, which is not the layout an installed
/// deco has, so the search would come up empty.
fn point_at_the_host() {
    std::env::set_var(
        "DECO_HOST_BOOTSTRAP",
        repo_root().join("extension-host/src/bootstrap.js"),
    );
}

/// A session that runs extensions as a plain process rather than in a container.
fn session() -> Session {
    let mut settings = Settings::with_defaults();
    settings.set(
        Scope::User,
        deco_ext::sandbox::SANDBOX_KEY,
        serde_json::json!("process"),
    );
    Session::new(settings, None, deco_keymap::binding::Platform::Linux)
}

/// Polls until `done` is satisfied, or panics with what the log says.
fn until(
    hosts: &mut Hosts,
    session: &mut Session,
    what: &str,
    done: impl Fn(&Hosts, &Session) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        hosts.poll(session);
        if done(hosts, session) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "{what} did not happen; status: {:?}; log:\n{}",
        session.status,
        hosts.log().collect::<Vec<_>>().join("\n")
    );
}

#[test]
#[ignore = "needs node; run through `cargo xtask host-test`"]
fn a_command_from_an_installed_extension_runs_and_answers() {
    let root = install(
        "runs",
        r#"{
  "name": "tools",
  "publisher": "acme",
  "displayName": "Acme Tools",
  "main": "./extension.js",
  "activationEvents": [],
  "contributes": {
    "commands": [{ "command": "acme.greet", "title": "Greet", "category": "Acme" }]
  }
}"#,
        r#"'use strict';
const vscode = require('vscode');
function activate(context) {
  context.subscriptions.push(
    vscode.commands.registerCommand('acme.greet', () => 'hello from the extension'),
  );
}
module.exports = { activate };
"#,
    );

    // The directory becomes a palette entry, named after the extension so that
    // "which extension is this" is answerable before running it.
    let catalogue = discover(std::slice::from_ref(&root));
    let listed = rows(&catalogue);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].id, "acme.greet");
    assert_eq!(listed[0].title, "Acme: Greet");
    assert_eq!(listed[0].detail.as_deref(), Some("Acme Tools"));

    let mut session = session();
    session.frontend_commands.extend(listed);
    // Before the `Hosts`, which looks for the bootstrap when it is built: a test
    // binary does not sit in the layout an installed deco does.
    point_at_the_host();
    let mut hosts = Hosts::new(catalogue);

    // Invoked the way the palette invokes it: by identifier, through the session,
    // which routes it to the frontend because the frontend declared it.
    let outcome = session.run("acme.greet", None, 0);
    assert_eq!(
        outcome,
        deco_editor::commands::Outcome::Frontend("acme.greet".to_owned()),
        "the session should hand an extension's command to the frontend"
    );
    assert!(
        hosts.run_command(&mut session, "acme.greet"),
        "the frontend should own it"
    );
    assert_eq!(hosts.started(), 1, "a host should have been started");

    // The extension's own return value, in the status bar.
    until(
        &mut hosts,
        &mut session,
        "the command to answer",
        |_, session| {
            session
                .status
                .as_deref()
                .is_some_and(|said| said.contains("hello from the extension"))
        },
    );

    // Asked again, the host is reused rather than started a second time.
    session.status = None;
    assert!(hosts.run_command(&mut session, "acme.greet"));
    assert_eq!(hosts.started(), 1);
    until(
        &mut hosts,
        &mut session,
        "the second answer",
        |_, session| {
            session
                .status
                .as_deref()
                .is_some_and(|said| said.contains("hello from the extension"))
        },
    );

    hosts.shutdown();
    assert_eq!(hosts.started(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs node; run through `cargo xtask host-test`"]
fn a_capability_the_manifest_never_declared_is_refused_when_it_is_really_asked_for() {
    // The design's whole claim, against a real extension rather than a `Request`
    // built in a test: this one declares nothing and tries to read a file.
    let root = install(
        "refused",
        r#"{
  "name": "tools",
  "publisher": "acme",
  "displayName": "Acme Tools",
  "main": "./extension.js",
  "contributes": { "commands": [{ "command": "acme.peek", "title": "Peek" }] }
}"#,
        r#"'use strict';
const vscode = require('vscode');
function activate(context) {
  context.subscriptions.push(
    vscode.commands.registerCommand('acme.peek', async () => {
      try {
        await vscode.workspace.fs.readFile('/etc/passwd');
        return 'read it';
      } catch (error) {
        // What an extension sees: an error, not an empty file.
        return `refused: ${error && error.message}`;
      }
    }),
  );
}
module.exports = { activate };
"#,
    );

    point_at_the_host();
    let mut session = session();
    let catalogue = discover(std::slice::from_ref(&root));
    session.frontend_commands.extend(rows(&catalogue));
    let mut hosts = Hosts::new(catalogue);

    assert!(hosts.run_command(&mut session, "acme.peek"));
    until(&mut hosts, &mut session, "the refusal", |_, session| {
        session
            .status
            .as_deref()
            .is_some_and(|said| said.contains("refused"))
    });

    let said = session.status.clone().unwrap_or_default();
    assert!(
        !said.contains("read it"),
        "an undeclared capability must not have been served: {said}"
    );
    // And deco's own record of it, which is what makes the refusal explicable
    // rather than an extension that mysteriously does not work.
    let log = hosts.log().collect::<Vec<_>>().join("\n");
    assert!(
        log.contains("refused fs.readFile"),
        "the refusal should be recorded: {log}"
    );

    hosts.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}
