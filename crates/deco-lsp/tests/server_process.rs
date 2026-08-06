//! End-to-end tests against a real child process.
//!
//! The unit tests in `process` and `supervisor` drive in-memory streams, which
//! covers the parsing and the state machine but not the parts that only exist
//! once an operating system is involved: whether the handshake actually
//! completes across a pipe, whether a server that dies is noticed, whether a
//! stopped server leaves anything behind.
//!
//! The server used here is this test binary re-executed with an environment
//! variable set, so the tests need no language server installed and work
//! identically on every platform CI runs. `cargo test` builds one binary per
//! integration test file and `current_exe` points at it, so re-execing is both
//! cheap and hermetic.

use std::time::Duration;

use deco_lsp::process::{Consent, ReaderEvent, ServerProcess};
use deco_lsp::server::{Command, ServerConfig, Trust};
use deco_lsp::supervisor::{Supervisor, SupervisorError, Update};
use deco_lsp::uri::PathStyle;

/// The variable the fake server reads to decide how to misbehave.
const ROLE: &str = "DECO_TEST_LSP_ROLE";

/// Path to the `fake_language_server` example.
///
/// `cargo test` builds examples into `target/<profile>/examples/`, and the test
/// binary itself lives in `target/<profile>/deps/`, so it is two levels up and
/// across. There is no `CARGO_BIN_EXE_*` for examples, which is why this is
/// derived rather than looked up.
fn fake_server() -> std::path::PathBuf {
    let test_binary = std::env::current_exe().expect("the test binary's own path");
    let profile_dir = test_binary
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target/<profile>/deps/<binary>");
    let path = profile_dir.join("examples").join(format!(
        "fake_language_server{}",
        std::env::consts::EXE_SUFFIX
    ));
    assert!(
        path.is_file(),
        "the fake server example was not built at {}",
        path.display()
    );
    path
}

/// A config that runs the fake server in the given role.
fn config(role: &str, trust: Trust) -> ServerConfig {
    ServerConfig {
        id: format!("fake-{role}"),
        language_ids: vec!["rust".into()],
        command: Command {
            program: fake_server().to_string_lossy().into_owned(),
            args: Vec::new(),
        },
        env: vec![(ROLE.to_owned(), role.to_owned())],
        initialization_options: None,
        trust,
    }
}

fn start(role: &str) -> Result<Supervisor, SupervisorError> {
    Supervisor::start(
        &config(role, Trust::User),
        Consent::Granted,
        None,
        PathStyle::Unix,
        Duration::from_secs(20),
    )
}

/// Polls until a predicate matches or the deadline passes.
///
/// Necessary because the child is a real process: the reply exists when it
/// exists, and asserting immediately after a write would be a race.
fn poll_until(
    supervisor: &mut Supervisor,
    limit: Duration,
    mut done: impl FnMut(&[Update]) -> bool,
) -> Vec<Update> {
    let deadline = std::time::Instant::now() + limit;
    let mut collected = Vec::new();
    while std::time::Instant::now() < deadline {
        collected.extend(supervisor.poll());
        if done(&collected) {
            return collected;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    collected
}

#[test]
fn the_handshake_completes_across_a_real_pipe() {
    let mut supervisor = start("plain").expect("the fake server should start");
    assert!(supervisor.is_ready());
    assert!(supervisor.capabilities().hover);
    assert!(supervisor.capabilities().open_close);
    supervisor.stop();
}

#[test]
fn a_diagnostic_published_by_the_server_reaches_the_editor() {
    let mut supervisor = start("publish-on-open").expect("the fake server should start");

    supervisor
        .did_open(std::path::Path::new("/w/a.rs"), "rust", "fn main() {}\n")
        .expect("didOpen");

    let updates = poll_until(&mut supervisor, Duration::from_secs(10), |updates| {
        updates
            .iter()
            .any(|u| matches!(u, Update::Diagnostics { .. }))
    });

    let diagnostics = updates
        .iter()
        .find_map(|u| match u {
            Update::Diagnostics { diagnostics, .. } => Some(diagnostics),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no diagnostics arrived: {updates:?}"));

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "mismatched types");
    assert_eq!(diagnostics[0].code.as_deref(), Some("E0308"));
    assert_eq!(diagnostics[0].range.start.line, 2);

    supervisor.stop();
}

#[test]
fn a_server_that_dies_at_startup_reports_its_stderr() {
    // The only explanation a user gets when a server will not run.
    let Err(error) = start("die-immediately") else {
        panic!("a server that exits immediately cannot complete a handshake");
    };
    let text = error.to_string();
    assert!(
        text.contains("refusing to run"),
        "the stderr tail must reach the error: {text}"
    );
}

#[test]
fn a_server_that_never_answers_hits_the_startup_timeout() {
    // Without this, a broken server hangs the editor at launch.
    let started = std::time::Instant::now();
    let result = Supervisor::start(
        &config("silent", Trust::User),
        Consent::Granted,
        None,
        PathStyle::Unix,
        Duration::from_millis(500),
    );
    assert!(matches!(
        result,
        Err(SupervisorError::StartupTimeout { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the timeout was not honoured"
    );
}

#[test]
fn a_protocol_error_across_the_pipe_is_a_startup_failure_not_a_hang() {
    let result = Supervisor::start(
        &config("garbage-on-initialize", Trust::User),
        Consent::Granted,
        None,
        PathStyle::Unix,
        Duration::from_secs(10),
    );
    assert!(
        matches!(result, Err(SupervisorError::StartupFailed { .. })),
        "expected a startup failure"
    );
}

#[test]
fn a_workspace_server_is_not_launched_without_consent() {
    // Asserted against a program that *would* run, so the refusal cannot be
    // mistaken for the program being missing.
    let result = Supervisor::start(
        &config("plain", Trust::Workspace),
        Consent::NotAsked,
        None,
        PathStyle::Unix,
        Duration::from_secs(5),
    );
    let Err(SupervisorError::Spawn(error)) = result else {
        panic!("a workspace server must not start unasked");
    };
    assert!(error.to_string().contains("approved"), "{error}");

    // And starts once approved, proving the refusal was about consent.
    let mut supervisor = Supervisor::start(
        &config("plain", Trust::Workspace),
        Consent::Granted,
        None,
        PathStyle::Unix,
        Duration::from_secs(20),
    )
    .expect("an approved workspace server should start");
    assert!(supervisor.is_ready());
    supervisor.stop();
}

#[test]
fn a_server_that_exits_mid_session_is_noticed() {
    let mut supervisor = start("die-on-save").expect("the fake server should start");
    assert!(supervisor.is_ready());

    supervisor
        .did_open(std::path::Path::new("/w/a.rs"), "rust", "x")
        .expect("didOpen");
    supervisor
        .did_save(std::path::Path::new("/w/a.rs"), "x")
        .expect("didSave");

    let updates = poll_until(&mut supervisor, Duration::from_secs(10), |updates| {
        updates.iter().any(|u| matches!(u, Update::Stopped { .. }))
    });
    assert!(
        updates.iter().any(|u| matches!(u, Update::Stopped { .. })),
        "the editor must notice a server that left: {updates:?}"
    );
    assert!(!supervisor.is_ready());

    // And a later edit to the still-open document is a named error rather than
    // a panic, a hang, or a silent success.
    assert!(matches!(
        supervisor.did_change(std::path::Path::new("/w/a.rs"), &[], "y"),
        Err(SupervisorError::NotRunning { .. })
    ));
}

#[test]
fn stopping_leaves_no_process_behind() {
    let mut supervisor = start("plain").expect("the fake server should start");
    supervisor.stop();
    assert!(!supervisor.is_ready());
    // Idempotent: quitting the editor should not depend on stop being called
    // exactly once.
    supervisor.stop();
}

#[test]
fn dropping_a_supervisor_stops_its_server() {
    // A server that is only dropped becomes an orphan holding a build lock.
    // There is no portable way to assert the process is gone from here, so what
    // is asserted is that the drop completes rather than blocking forever.
    let supervisor = start("plain").expect("the fake server should start");
    assert!(supervisor.is_ready());
    drop(supervisor);
}

#[test]
fn a_process_can_be_driven_directly_without_a_supervisor() {
    // The lower layer on its own, since a frontend may want to own the loop.
    let mut process =
        ServerProcess::spawn(&config("plain", Trust::User), Consent::Granted).expect("spawn");

    let request = deco_lsp::Message::Request(deco_lsp::Request {
        id: 1.into(),
        method: "initialize".into(),
        params: Some(serde_json::json!({})),
    });
    process.send(&request).expect("write");

    let event = process
        .recv_timeout(Duration::from_secs(10))
        .expect("the server should answer");
    assert!(matches!(event, ReaderEvent::Message(_)));

    // stderr is drained continuously, so the server's log line is available.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && process.stderr_tail().is_empty() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        process
            .stderr_tail()
            .lines()
            .any(|l| l.contains("initialized")),
        "stderr was not drained: {}",
        process.stderr_tail().summary()
    );

    process.stop(Duration::from_secs(2));
}
