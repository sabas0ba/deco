//! One extension, started in a real host process, registering a command.
//!
//! `deco-ext`'s unit tests drive the connection over a `Cursor` or a channel,
//! because the Rust suite must run anywhere, including under Wine, which has no
//! Node. These tests run the whole stack against the real `extension-host`. They
//! check that the framing, the environment, the sandbox, the `vscode` shim and the
//! capability checks work together, not only in their own unit tests.
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
    // Declares no capabilities, so it can only use the mediated surface, which
    // includes registering a command.
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
/// Absolute because the host's environment is built from scratch and has no `PATH`
/// for the operating system to search. The caller, here this test, must resolve it.
/// `DECO_TEST_NODE` overrides the search.
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

    // An extension that declared no capabilities.
    let broker = Broker::new(
        Vec::new(),
        GrantStore::default(),
        DefaultPolicy::Deny,
        ResolutionContext::default(),
    );

    let activation = host
        .activate(&extension.to_string_lossy(), "./extension.js")
        .expect("the pipe should take a request");

    // The extension's calls during activation reach deco as requests and must pass
    // the capability check. `commands.registerCommand` is part of the mediated
    // surface, so it needs no declaration.
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

    // The reverse direction, which a palette entry needs: deco invokes a command
    // the extension registered and receives the callback's return value. The
    // `vscode` shim has supported this since it was written, but no test used it.
    let hello = host
        .execute_command("roundTrip.hello", serde_json::json!([]))
        .expect("a request");
    let unknown = host
        .execute_command("roundTrip.notThere", serde_json::json!([]))
        .expect("a request");

    let mut said: Option<serde_json::Value> = None;
    let mut refused: Option<String> = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline && (said.is_none() || refused.is_none()) {
        match host.poll() {
            Some(HostEvent::Message(Message::Response(response))) => {
                // The reply to `$/activate` can still be in flight: the loop above
                // stops on `$/activated`, which the host sends before answering.
                let method = host.answered(response.id);
                let expected = if response.id == activation {
                    "$/activate"
                } else {
                    "$/executeCommand"
                };
                assert_eq!(method.as_deref(), Some(expected), "{response:?}");
                if response.id == hello {
                    assert!(
                        response.error.is_none(),
                        "the command failed: {:?}",
                        response.error
                    );
                    said = response.result;
                } else if response.id == unknown {
                    // A command that is not registered gets an error reply and
                    // does not break the connection.
                    refused = Some(
                        response
                            .error
                            .map(|error| error.message)
                            .expect("an unregistered command should be an error"),
                    );
                }
            }
            Some(HostEvent::Closed) => panic!("the host exited; stderr:\n{}", host.errors()),
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    assert_eq!(
        said,
        Some(serde_json::json!("hello from the host")),
        "the extension's own return value should come back; stderr:\n{}",
        host.errors()
    );
    let refused = refused.expect("no reply for the unregistered command");
    assert!(
        refused.contains("roundTrip.notThere"),
        "the refusal should name the command: {refused}"
    );

    host.shutdown();
    let _ = std::fs::remove_dir_all(&extension);
}

#[test]
#[ignore = "needs node; run with --ignored in the extension-host CI job"]
fn the_host_starts_with_nothing_but_the_two_variables_it_is_given() {
    // The environment is built from scratch instead of filtered from the parent's.
    // This test checks that this holds for the running process, not only for the
    // spec. An extension that could read `$GITHUB_TOKEN` would defeat the other
    // protections.
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
    // A variable a real parent process could have, to check it is not inherited.
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
    // Windows needs `SystemRoot` to start Node, so `build_spec` adds it. No other
    // variable is allowed.
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

/// The extension used by the container test. It reports the environment it sees
/// through a call that needs no capability.
///
/// It reports every name, including deco's own two, unlike the process-mode
/// fixture, which filters out `DECO_*`. In a container the whole environment is
/// built explicitly, so the exact set is known. A filter would hide a
/// `DECO_`-prefixed variable inherited from the parent.
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
    // The other two tests use the `node` installed on the machine. This one uses
    // the runtime deco is meant to use: the image named by `DEFAULT_IMAGE`, pulled
    // by digest, with the network blocked by the kernel and nothing writable. If
    // the pinned digest stops providing a working Node, this test fails.
    let root = repo_root();
    let extension = reporting_fixture("container");
    // Variables a real parent process could have that the extension must not see.
    // Set before spawning, so they exist in deco's environment when the container
    // starts.
    std::env::set_var("DECO_TEST_PARENT_SECRET", "hunter2");
    std::env::set_var("PARENT_SECRET_SHOULD_NOT_LEAK", "hunter2");

    let config = HostConfig {
        // Unused in a container, because the image supplies Node. Set to an
        // invalid path on purpose, so a spec that used the machine's runtime
        // would fail here.
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

    // Reaching this point means the whole stack works inside the container: the
    // mounts, the translated paths, `--permission` with container roots, the
    // `vscode` shim, and the capability checks.
    let reported = reported.expect("the extension should have registered its report");
    let names: Vec<&str> = reported
        .split(',')
        .filter(|name| !name.is_empty())
        .collect();

    // A container's environment is `--env`, plus what the image sets in its own
    // layers, plus what the runtime adds. Podman sets `container=podman` and
    // Docker sets nothing, so this checks against a permitted set instead of an
    // exact match. Every name must still be listed by deco, so a new name fails
    // the test. An exact match would pass on Docker and fail on Podman, which
    // would not reflect a problem in deco.
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
    // deco's own two variables arrived, so the check above did not pass because
    // the report was empty.
    assert!(names.contains(&"DECO_EXTENSION_ID"), "{names:?}");
    assert!(names.contains(&"DECO_HOST_PROTOCOL"), "{names:?}");
    // Implied by the above, but asserted explicitly: neither parent variable
    // reached the container, including the one with a `DECO_` prefix.
    for secret in ["PARENT_SECRET_SHOULD_NOT_LEAK", "DECO_TEST_PARENT_SECRET"] {
        assert!(
            !names.contains(&secret),
            "{secret} crossed into the container"
        );
    }

    host.shutdown();
    let _ = std::fs::remove_dir_all(&extension);
}
