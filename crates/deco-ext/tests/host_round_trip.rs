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

use deco_config::Settings;
use deco_ext::capability::{Broker, DefaultPolicy, GrantStore, ResolutionContext};
use deco_ext::connection::{dispatch, Dispatch, Host, HostEvent};
use deco_ext::host::{build_spec, HostConfig, HostLimits};
use deco_ext::protocol::{Message, Response};
use deco_ext::sandbox::{containerise, ContainerConfig};

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

/// The extension used by the container test: it reports the environment it can
/// see through the one call that needs no capability at all.
///
/// Every name, including deco's own two — unlike the process-mode fixture, which
/// filters `DECO_*` out. In a container the whole environment is built by hand,
/// so the exact set is knowable, and a `DECO_`-prefixed variable that the parent
/// happened to have would otherwise hide inside the filter.
fn reporting_fixture(name: &str) -> PathBuf {
    let dir = fixture(name);
    std::fs::write(
        dir.join("extension.js"),
        r#"'use strict';
const vscode = require('vscode');
function activate() {
  const seen = Object.keys(process.env).sort();
  vscode.commands.registerCommand('env.report:' + seen.join(','), () => 0);
}
module.exports = { activate };
"#,
    )
    .expect("an extension");
    dir
}

#[test]
#[ignore = "needs a container runtime; run through `cargo xtask host-test`"]
fn a_container_host_activates_an_extension_and_hands_it_no_environment() {
    // The other two tests prove the stack against a borrowed `node`. This one
    // proves it against the runtime deco actually intends to use: the image named
    // by `DEFAULT_IMAGE`, pulled by digest, with the network severed by the
    // kernel and nothing writable. If the pinned digest ever stops being a
    // working Node, this is the test that says so.
    let root = repo_root();
    let extension = reporting_fixture("container");
    // What a real parent process has and the extension must not see. Set before
    // spawning, because the point is that it exists on deco's side at the moment
    // the container starts.
    std::env::set_var("DECO_TEST_PARENT_SECRET", "hunter2");
    std::env::set_var("PARENT_SECRET_SHOULD_NOT_LEAK", "hunter2");

    let config = HostConfig {
        // Unused in a container — the image supplies Node — and left at
        // something obviously wrong on purpose, so a spec that reached for the
        // machine's own runtime would fail loudly here.
        node: PathBuf::from("/nonexistent/node"),
        bootstrap: root.join("extension-host/src/bootstrap.js"),
        readable_roots: vec![root.join("extension-host"), extension.clone()],
        cwd: extension.clone(),
        limits: HostLimits {
            // The first run on a machine pulls the image.
            startup_timeout_ms: 240_000,
            ..HostLimits::default()
        },
        node_permission_model: true,
        allow_code_generation: false,
    };

    // Default settings: the shipped image, and whichever runtime is installed.
    let container = ContainerConfig::resolve(
        &Settings::with_defaults(),
        std::env::var_os("PATH").as_deref(),
    )
    .expect("a container runtime; `cargo xtask host-test` should not have run this without one");
    let made = containerise(&config, &container, "test.container").expect("a container spec");
    let inside = made
        .mounts
        .inside(&extension)
        .expect("the extension is mounted, so it has a path inside");

    let mut host = Host::spawn(&made.spec).expect("the container runtime should start");
    let (ready, before) = host.wait_for_ready(Duration::from_millis(240_000));
    assert!(
        ready.is_ok(),
        "the host in the container never became ready: {ready:?}; saw {before:?}; stderr:\n{}",
        host.errors()
    );

    host.request(
        "$/activate",
        serde_json::json!({ "extensionPath": inside, "main": "./extension.js" }),
    )
    .expect("the pipe should take a request");

    let mut reported: Option<String> = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
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
            Some(HostEvent::Message(Message::Response(response))) => assert!(
                response.error.is_none(),
                "activation failed inside the container: {:?}; stderr:\n{}",
                response.error,
                host.errors()
            ),
            Some(HostEvent::Closed) => {
                panic!("the container exited; stderr:\n{}", host.errors())
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }

    // Reaching here at all means the whole stack works inside the container:
    // the mounts, the translated paths, `--permission` with container roots, the
    // `vscode` shim, and the capability seam.
    let reported = reported.expect("the extension should have registered its report");
    let names: Vec<&str> = reported
        .split(',')
        .filter(|name| !name.is_empty())
        .collect();

    // A container's environment is `--env`, plus what the image sets in its own
    // layers, plus what the runtime adds for itself: Podman sets
    // `container=podman` where Docker sets nothing, which is why this is a
    // permitted set rather than an equality. Every name still has to be one deco
    // has accounted for, so a new one is a failure and not a shrug — what an
    // equality would buy on top of that is a test that passes on Docker and fails
    // on Podman, which says nothing about deco.
    let mut permitted: Vec<&str> = deco_ext::sandbox::IMAGE_ENVIRONMENT.to_vec();
    permitted.extend(deco_ext::sandbox::RUNTIME_INJECTED);
    permitted.extend(["DECO_EXTENSION_ID", "DECO_HOST_PROTOCOL"]);
    let unaccounted: Vec<&&str> = names
        .iter()
        .filter(|name| !permitted.contains(name))
        .collect();
    assert!(
        unaccounted.is_empty(),
        "the container handed the extension {unaccounted:?}, which deco did not \
         account for; all of it was {names:?}"
    );
    // And deco's own two did arrive, so the emptiness above cannot be passing for
    // the wrong reason.
    assert!(names.contains(&"DECO_EXTENSION_ID"), "{names:?}");
    assert!(names.contains(&"DECO_HOST_PROTOCOL"), "{names:?}");
    // Implied by the above, but asserted where it can be read: neither of the
    // parent's variables crossed — not the ordinary one, and not the one whose
    // name looks like something deco itself would pass.
    for secret in ["PARENT_SECRET_SHOULD_NOT_LEAK", "DECO_TEST_PARENT_SECRET"] {
        assert!(
            !names.contains(&secret),
            "{secret} crossed into the container"
        );
    }

    host.shutdown();
    let _ = std::fs::remove_dir_all(&extension);
}
