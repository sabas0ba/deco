//! One extension, started for real, registering a command.
//!
//! Everything in `deco-ext`'s unit tests drives the connection over a `Cursor` or a
//! channel, because the Rust suite has to run anywhere — including under Wine, which
//! has no Node. This is the other half: the whole stack against the real
//! `extension-host`, proving that the framing, the environment, the sandbox, the
//! `vscode` shim and the capability seam agree with each other and not merely with
//! their own tests.
//!
//! `#[ignore]`d, so `cargo test` stays portable. CI runs it in the job that already
//! installs Node:
//!
//! ```console
//! $ cargo test -p deco-ext --test host_round_trip -- --ignored
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use deco_ext::capability::{Broker, DefaultPolicy, GrantStore, ResolutionContext};
use deco_ext::connection::{dispatch, Dispatch, Host, HostEvent};
use deco_ext::host::{build_spec, HostConfig, HostLimits};
use deco_ext::protocol::{Message, Response};

/// The repository root, from this crate's own location.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/deco-ext is two levels down")
        .to_path_buf()
}

/// A directory holding a minimal extension that registers one command.
fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deco-host-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp directory");
    std::fs::write(
        dir.join("package.json"),
        r#"{
  "name": "round-trip",
  "main": "./extension.js",
  "activationEvents": ["*"],
  "contributes": { "commands": [{ "command": "roundTrip.hello", "title": "Hello" }] }
}"#,
    )
    .expect("a manifest");
    // Declares nothing, so it may only reach the mediated surface — which is exactly
    // what registering a command is.
    std::fs::write(
        dir.join("extension.js"),
        r#"'use strict';
const vscode = require('vscode');
function activate(context) {
  context.subscriptions.push(
    vscode.commands.registerCommand('roundTrip.hello', () => 'hello from the host'),
  );
}
module.exports = { activate };
"#,
    )
    .expect("an extension");
    dir
}

/// An absolute path to `node`.
///
/// Absolute because the host's environment is built from nothing and so carries no
/// `PATH` for the operating system to search — resolving it is the caller's job, and
/// here the caller is this test. `DECO_TEST_NODE` overrides the search.
fn node() -> PathBuf {
    if let Ok(given) = std::env::var("DECO_TEST_NODE") {
        return PathBuf::from(given);
    }
    let path = std::env::var_os("PATH").expect("a PATH to search");
    std::env::split_paths(&path)
        .map(|dir| dir.join(if cfg!(windows) { "node.exe" } else { "node" }))
        .find(|candidate| candidate.is_file())
        .expect("node should be on the PATH; set DECO_TEST_NODE to point at it")
}

#[test]
#[ignore = "needs node; run with --ignored in the extension-host CI job"]
fn an_extension_activates_and_registers_a_command() {
    let root = repo_root();
    let bootstrap = root.join("extension-host/src/bootstrap.js");
    assert!(bootstrap.is_file(), "{} is missing", bootstrap.display());
    let extension = fixture("register");

    let config = HostConfig {
        node: node(),
        bootstrap: bootstrap.clone(),
        // The host's own code and this one extension. Nothing else is readable,
        // including the home directory.
        readable_roots: vec![root.join("extension-host"), extension.clone()],
        cwd: extension.clone(),
        limits: HostLimits {
            startup_timeout_ms: 20_000,
            ..HostLimits::default()
        },
        node_permission_model: true,
        allow_code_generation: false,
    };
    let spec = build_spec(&config, "test.round-trip");

    let mut host = Host::spawn(&spec).expect("node should start");
    let (ready, before) = host.wait_for_ready(Duration::from_millis(20_000));
    assert!(
        ready.is_ok(),
        "host never became ready: {ready:?}; saw {before:?}; stderr:\n{}",
        host.errors()
    );

    // An extension that declared no capabilities at all.
    let broker = Broker::new(
        Vec::new(),
        GrantStore::default(),
        DefaultPolicy::Deny,
        ResolutionContext::default(),
    );

    host.request(
        "$/activate",
        serde_json::json!({
            "extensionPath": extension.to_string_lossy(),
            "main": "./extension.js",
        }),
    )
    .expect("the pipe should take a request");

    // What the extension does on activation reaches deco as a request, and it has to
    // survive the capability seam to get here — `commands.registerCommand` needs no
    // declaration, which is the point of the mediated surface.
    let mut registered = false;
    let mut activated = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline && !(registered && activated) {
        match host.poll() {
            Some(HostEvent::Message(Message::Request(request))) => {
                assert_eq!(
                    dispatch(&broker, &request),
                    Dispatch::Allowed,
                    "an extension that declared nothing should still be able to \
                     register a command: {request:?}"
                );
                if request.method == "commands.registerCommand" {
                    assert_eq!(request.params["command"], "roundTrip.hello");
                    registered = true;
                }
                host.send(&Message::Response(Response::ok(
                    request.id,
                    serde_json::Value::Null,
                )))
                .expect("a reply should go out");
            }
            Some(HostEvent::Message(Message::Response(response))) => {
                // The reply to `$/activate`.
                let method = host.answered(response.id);
                assert_eq!(method.as_deref(), Some("$/activate"));
                assert!(
                    response.error.is_none(),
                    "activation failed: {:?}; stderr:\n{}",
                    response.error,
                    host.errors()
                );
            }
            Some(HostEvent::Message(Message::Notification(note))) => {
                if note.method == "$/activated" {
                    activated = true;
                }
            }
            Some(HostEvent::Garbled(what)) => panic!("unreadable line: {what}"),
            Some(HostEvent::Closed) => panic!("the host exited; stderr:\n{}", host.errors()),
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    assert!(
        registered,
        "no command was registered; stderr:\n{}",
        host.errors()
    );
    assert!(activated, "no `$/activated`; stderr:\n{}", host.errors());

    host.shutdown();
    let _ = std::fs::remove_dir_all(&extension);
}

#[test]
#[ignore = "needs node; run with --ignored in the extension-host CI job"]
fn the_host_starts_with_nothing_but_the_two_variables_it_is_given() {
    // The environment is built from nothing rather than filtered from the parent's, and
    // this is the test that it is true of the running process and not only of the spec.
    // An extension that could read `$GITHUB_TOKEN` would make every other guard moot.
    let root = repo_root();
    let extension = fixture("environment");
    std::fs::write(
        extension.join("extension.js"),
        r#"'use strict';
const vscode = require('vscode');
function activate() {
  const leaked = Object.keys(process.env).filter((k) => !k.startsWith('DECO_'));
  vscode.commands.registerCommand('env.report:' + leaked.sort().join(','), () => 0);
}
module.exports = { activate };
"#,
    )
    .expect("an extension");

    let config = HostConfig {
        node: node(),
        bootstrap: root.join("extension-host/src/bootstrap.js"),
        readable_roots: vec![root.join("extension-host"), extension.clone()],
        cwd: extension.clone(),
        limits: HostLimits {
            startup_timeout_ms: 20_000,
            ..HostLimits::default()
        },
        node_permission_model: true,
        allow_code_generation: false,
    };
    let mut spec = build_spec(&config, "test.environment");
    // Something a real parent process would have, to prove it is not inherited.
    std::env::set_var("DECO_TEST_SECRET_TOKEN_SHOULD_NOT_LEAK", "hunter2");
    spec.env.remove("NOTHING");

    let mut host = Host::spawn(&spec).expect("node should start");
    let (ready, _) = host.wait_for_ready(Duration::from_millis(20_000));
    assert!(ready.is_ok(), "not ready: {ready:?}\n{}", host.errors());

    host.request(
        "$/activate",
        serde_json::json!({
            "extensionPath": extension.to_string_lossy(),
            "main": "./extension.js",
        }),
    )
    .expect("a request");

    let mut reported: Option<String> = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline && reported.is_none() {
        match host.poll() {
            Some(HostEvent::Message(Message::Request(request))) => {
                if request.method == "commands.registerCommand" {
                    reported = request.params["command"]
                        .as_str()
                        .and_then(|name| name.strip_prefix("env.report:"))
                        .map(str::to_owned);
                }
                let _ = host.send(&Message::Response(Response::ok(
                    request.id,
                    serde_json::Value::Null,
                )));
            }
            Some(HostEvent::Closed) => panic!("the host exited; stderr:\n{}", host.errors()),
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    let leaked = reported.expect("the extension should have reported");
    let names: Vec<&str> = leaked.split(',').filter(|n| !n.is_empty()).collect();
    // Windows needs `SystemRoot` to start Node at all, which `build_spec` adds
    // deliberately; nothing else belongs here.
    let allowed: BTreeMap<&str, ()> = [("SystemRoot", ())].into_iter().collect();
    let unexpected: Vec<&&str> = names
        .iter()
        .filter(|name| !allowed.contains_key(**name))
        .collect();
    assert!(
        unexpected.is_empty(),
        "the host inherited {unexpected:?} from the parent"
    );

    host.shutdown();
    let _ = std::fs::remove_dir_all(&extension);
}
