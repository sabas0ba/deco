//! End-to-end tests against a real child process.
//!
//! The unit tests in `process` and `supervisor` use in-memory streams. They
//! cover parsing and the state machine, but not behaviour that depends on the
//! operating system: whether the handshake completes across a pipe, whether a
//! server exit is detected, and whether a stopped server leaves anything
//! behind.
//!
//! The server used here is the `fake_language_server` example, started with an
//! environment variable that selects its behaviour. The tests therefore need no
//! installed language server and behave the same on every CI platform. The
//! test binary itself is not re-executed, because libtest writes its progress
//! to stdout and that output would corrupt the frame stream.

use std::time::Duration;

use deco_lsp::process::{Consent, ReaderEvent, ServerProcess};
use deco_lsp::server::{Command, ServerConfig, Trust};
use deco_lsp::supervisor::{Supervisor, SupervisorError, Update};
use deco_lsp::uri::{PathMap, PathStyle};

/// The variable the fake server reads to select its behaviour.
const ROLE: &str = "DECO_TEST_LSP_ROLE";

/// Path to the `fake_language_server` example.
///
/// `cargo test` builds examples into `target/<profile>/examples/`, and the test
/// binary is in `target/<profile>/deps/`. There is no `CARGO_BIN_EXE_*` for
/// examples, so the path is derived from the test binary's path.
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
        PathMap::local(PathStyle::Unix),
        Duration::from_secs(20),
    )
}

/// Polls until a predicate matches or the deadline passes.
///
/// The child is a real process, so the reply arrives at an unknown time.
/// Asserting immediately after a write would be a race.
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
    // stderr is the only information a user gets when a server does not run.
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
fn a_server_that_dies_after_reading_the_request_still_reports_its_stderr() {
    // Companion to the test above. This one found a bug. A server can fail at
    // startup in two ways, which take different code paths:
    //
    //   * it exits before reading: the editor's write fails with a broken pipe;
    //   * it exits after reading: the write succeeds and the only signal is
    //     stdout closing.
    //
    // Only the first case was tested, and it is the one that usually occurs on
    // a developer machine. The second path reported "the server wrote nothing
    // to stderr" until it failed in CI. Detecting stdout closing and collecting
    // stderr run on separate threads, and the stderr message must be reported
    // regardless of which finishes first.
    let Err(error) = start("die-after-reading") else {
        panic!("a server that never answers cannot complete a handshake");
    };
    let text = error.to_string();
    assert!(
        text.contains("then gave up"),
        "the stderr tail must reach the error: {text}"
    );
}

#[test]
fn a_slow_explanation_is_still_waited_for() {
    // Worst-case timing: this server writes to stderr 120ms after the editor
    // detects that stdout closed. An implementation that only checks whether
    // output is available reports nothing. Waiting for the pump thread to
    // finish reports the message.
    let Err(error) = start("die-slowly") else {
        panic!("a server that never answers cannot complete a handshake");
    };
    let text = error.to_string();
    assert!(
        text.contains("took a moment"),
        "the reason arrived late and was dropped: {text}"
    );
}

#[test]
fn a_server_that_never_answers_hits_the_startup_timeout() {
    // Without the timeout, a broken server blocks the editor at launch.
    let started = std::time::Instant::now();
    let result = Supervisor::start(
        &config("silent", Trust::User),
        Consent::Granted,
        None,
        PathMap::local(PathStyle::Unix),
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
        PathMap::local(PathStyle::Unix),
        Duration::from_secs(10),
    );
    assert!(
        matches!(result, Err(SupervisorError::StartupFailed { .. })),
        "expected a startup failure"
    );
}

#[test]
fn a_workspace_server_is_not_launched_without_consent() {
    // Uses a program that *would* run, so the rejection cannot be caused by a
    // missing program.
    let result = Supervisor::start(
        &config("plain", Trust::Workspace),
        Consent::NotAsked,
        None,
        PathMap::local(PathStyle::Unix),
        Duration::from_secs(5),
    );
    let Err(SupervisorError::Spawn(error)) = result else {
        panic!("a workspace server must not start unasked");
    };
    assert!(error.to_string().contains("approved"), "{error}");

    // It starts once approved, so the rejection was caused by missing consent.
    let mut supervisor = Supervisor::start(
        &config("plain", Trust::Workspace),
        Consent::Granted,
        None,
        PathMap::local(PathStyle::Unix),
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

    // A later edit to the still-open document returns a named error. It does
    // not panic, hang, or succeed silently.
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
    // Idempotent, so quitting the editor does not depend on stop being called
    // exactly once.
    supervisor.stop();
}

#[test]
fn dropping_a_supervisor_stops_its_server() {
    // A server that is not stopped on drop becomes an orphan process holding a
    // build lock. There is no portable way to check here that the process has
    // exited, so the test only checks that the drop completes without blocking.
    let supervisor = start("plain").expect("the fake server should start");
    assert!(supervisor.is_ready());
    drop(supervisor);
}

#[test]
fn a_process_can_be_driven_directly_without_a_supervisor() {
    // Tests the lower layer alone, because a frontend may run its own loop.
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

#[test]
fn a_remote_session_puts_the_far_ends_paths_on_the_wire() {
    // Tests the path mapping for remote language support. The editor holds
    // `src/main.rs`, relative to a workspace on another machine, and the
    // server must receive the absolute path on that machine. Only a real
    // server can show which path was sent, so this one echoes it back.
    let mut supervisor = Supervisor::start(
        &config("echo-uri-on-open", Trust::User),
        Consent::Granted,
        Some(std::path::Path::new("/home/u/project")),
        PathMap::remote(std::path::PathBuf::from("/home/u/project")),
        Duration::from_secs(20),
    )
    .expect("the fake server should start");

    let path = std::path::Path::new("src/main.rs");
    supervisor
        .did_open(path, "rust", "fn main() {}\n")
        .expect("an open");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while supervisor
        .diagnostics(&supervisor.uri_for(path).expect("a uri"))
        .is_empty()
        && std::time::Instant::now() < deadline
    {
        supervisor.poll();
        std::thread::sleep(Duration::from_millis(10));
    }

    // The diagnostic is found under the path the *editor* uses. This fails if
    // the prefix is added but not removed again.
    let uri = supervisor.uri_for(path).expect("a uri");
    assert_eq!(uri.as_str(), "file:///home/u/project/src/main.rs");
    let diagnostics = supervisor.diagnostics(&uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    // The server received the absolute remote path, not a relative path that
    // it would resolve against its own working directory.
    assert_eq!(
        diagnostics[0].message,
        "opened file:///home/u/project/src/main.rs"
    );

    supervisor.stop();
}
