//! Starting a host process and talking to it.
//!
//! The piece that was missing between [`crate::host::build_spec`], which says what
//! command to run, and [`crate::protocol`], which says what the two ends say to each
//! other. Everything below it was built and tested against itself; nothing started a
//! process.
//!
//! # The framing
//!
//! One JSON object per line, which is what the Node side writes. Not
//! `Content-Length` framing like the Language Server Protocol: there is no
//! specification to match here, both ends are deco's, and a line is the format a
//! reader can resynchronise from — a bad frame costs one message rather than the
//! rest of the stream.
//!
//! # Where the capability model is applied
//!
//! [`dispatch`] is the only way an inbound request reaches the editor, and it is a
//! pure function of the broker and the request so that every path through it can be
//! tested without a process. It fails closed twice over: a method
//! [`crate::protocol::required_capability`] does not recognise is refused, and so is
//! a capability the manifest never declared.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::capability::{Broker, CheckResult};
use crate::host::{HostSpec, PROTOCOL_VERSION};
use crate::protocol::{required_capability, ErrorCode, Message, Notification, Request, Response};

/// What the reader saw.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// A message arrived.
    Message(Message),
    /// A line could not be understood.
    ///
    /// Not terminal, unlike a length-prefixed framing error: the next newline is a
    /// known position, so one unreadable line costs one message.
    Garbled(String),
    /// The stream ended, which means the process is going or gone.
    Closed,
}

/// The last lines the host wrote to stderr.
///
/// Bounded, because a host that logs per keystroke would otherwise grow this for the
/// length of the session. The *last* lines are kept rather than the first: when a
/// process dies, the reason is at the end.
#[derive(Debug, Default)]
pub struct ErrorLog {
    lines: std::collections::VecDeque<String>,
}

/// How many stderr lines are kept.
pub const ERROR_LOG_LINES: usize = 40;

impl ErrorLog {
    /// Adds a line, dropping the oldest if the log is full.
    pub fn push(&mut self, line: String) {
        if self.lines.len() == ERROR_LOG_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// The log as one string, oldest first.
    pub fn joined(&self) -> String {
        self.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// Whether nothing has been logged.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Reads newline-delimited JSON until the stream ends, sending each message on.
///
/// Returns when the stream closes or the receiver hangs up. A line that does not
/// parse is reported and skipped rather than ending the read: the framing is a
/// newline, so the next message starts at a position that is still known.
pub fn pump_messages(reader: impl BufRead, tx: &Sender<HostEvent>) {
    for line in reader.lines() {
        let Ok(line) = line else {
            // An I/O error on the pipe means the process is gone.
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let event = match Message::decode(&line) {
            Ok(message) => HostEvent::Message(message),
            Err(error) => HostEvent::Garbled(format!("{error}")),
        };
        if tx.send(event).is_err() {
            return;
        }
    }
    let _ = tx.send(HostEvent::Closed);
}

/// Reads lines until the stream ends, appending each to a shared log.
///
/// Invalid UTF-8 is replaced rather than fatal: this is a diagnostic channel, and a
/// host that logs a stray byte should not cost the log.
pub fn pump_lines(reader: impl BufRead, log: &Mutex<ErrorLog>) {
    for line in reader.split(b'\n') {
        let Ok(line) = line else {
            break;
        };
        let text = String::from_utf8_lossy(&line).trim_end().to_owned();
        if text.is_empty() {
            continue;
        }
        if let Ok(mut log) = log.lock() {
            log.push(text);
        }
    }
}

/// What deco does with one inbound request.
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatch {
    /// Refused before it reached the editor. Carries the reply to send back.
    Refused(Response),
    /// The user has to be asked before this can proceed.
    Consent {
        /// What to ask about.
        capability: crate::Capability,
    },
    /// Allowed through to the editor surface.
    Allowed,
}

/// Decides what happens to an inbound request.
///
/// The only way a request reaches the editor, and a pure function so that every path
/// through it is testable without a process. It fails closed twice:
///
/// - a method [`required_capability`] does not recognise is refused as unknown, so a
///   host built from a newer deco cannot reach an older one's editor surface by
///   naming something it has never heard of;
/// - a capability the manifest did not declare is refused by the broker, whatever the
///   user has agreed to since — the declaration is a ceiling and not a starting point.
pub fn dispatch(broker: &Broker, request: &Request) -> Dispatch {
    let Ok(needed) = required_capability(&request.method, &request.params) else {
        return Dispatch::Refused(Response::err(
            request.id,
            ErrorCode::MethodNotFound,
            format!("deco does not know the method `{}`", request.method),
        ));
    };
    // No capability needed: a method that only touches state deco already owns and
    // shows to the user.
    let Some(needed) = needed else {
        return Dispatch::Allowed;
    };
    match broker.check(&needed) {
        CheckResult::Allowed => Dispatch::Allowed,
        CheckResult::NeedsConsent { capability } => Dispatch::Consent { capability },
        CheckResult::Denied { reason } => Dispatch::Refused(Response::err(
            request.id,
            ErrorCode::PermissionDenied,
            reason.to_string(),
        )),
    }
}

/// Whether a `$/ready` notification agrees with deco about the protocol.
///
/// Its own function so that the rule can be tested without a process, and so the test
/// exercises the same code the handshake does rather than a copy of it.
pub fn agrees_on_protocol(ready: &Notification) -> Result<(), ReadyError> {
    let claimed = ready
        .params
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if claimed == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ReadyError::Protocol {
            host: claimed.to_owned(),
        })
    }
}

/// Why a host could not be started.
#[derive(Debug)]
pub enum SpawnError {
    /// The program could not be run at all.
    Launch {
        /// The program that was tried.
        program: String,
        /// What the operating system said.
        error: std::io::Error,
    },
    /// A pipe to the process could not be taken.
    Pipes,
    /// The program was named rather than located.
    ///
    /// The host's environment is built from nothing, so it has no `PATH` for the
    /// operating system to search — a bare `node` fails as "no such file", which is
    /// true and unhelpful. Refused here instead, where the reason can be said.
    NotAbsolute {
        /// What was asked for.
        program: String,
    },
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Launch { program, error } => {
                write!(f, "could not start the extension host `{program}`: {error}")
            }
            Self::Pipes => write!(f, "could not open pipes to the extension host"),
            Self::NotAbsolute { program } => write!(
                f,
                "the extension host's program must be an absolute path, not `{program}`: \
                 its environment carries no PATH to search"
            ),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Why the host never became usable.
#[derive(Debug, PartialEq)]
pub enum ReadyError {
    /// It did not say `$/ready` in time.
    TimedOut {
        /// How long was allowed.
        after_ms: u64,
    },
    /// It exited, or its pipe closed, first.
    Closed,
    /// It said `$/ready` with a protocol version deco does not speak.
    ///
    /// The Node side refuses first, so reaching this means the two disagree about
    /// which of them is wrong — still better than half-speaking an older protocol.
    Protocol {
        /// What the host claimed.
        host: String,
    },
}

impl std::fmt::Display for ReadyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut { after_ms } => {
                write!(f, "the extension host did not start within {after_ms} ms")
            }
            Self::Closed => write!(f, "the extension host exited before it was ready"),
            Self::Protocol { host } => write!(
                f,
                "the extension host speaks protocol {host}, deco speaks {PROTOCOL_VERSION}"
            ),
        }
    }
}

/// How long to wait for a host to exit on its own before killing it.
///
/// Long enough for `deactivate` to finish something short, short enough that quitting
/// the editor is not held up by an extension that will not stop.
pub const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// The request that loads an extension and runs its `activate`.
pub const ACTIVATE: &str = "$/activate";

/// The request that runs a command the extension registered.
///
/// Named here rather than written at each call site because both ends have to
/// agree on it, and the other end is `extension-host/src/vscode.js`.
pub const EXECUTE_COMMAND: &str = "$/executeCommand";

/// A running host process.
pub struct Host {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<HostEvent>,
    errors: Arc<Mutex<ErrorLog>>,
    next_id: u64,
    /// Requests sent and not yet answered, by id and method.
    ///
    /// Kept so a reply can be attributed: the id alone says nothing about what it is
    /// answering, and a caller that has forgotten cannot route the result.
    pending: BTreeMap<u64, String>,
}

impl Host {
    /// Starts the process described by `spec`.
    ///
    /// The environment is `spec`'s in full — [`crate::host::build_spec`] builds it from
    /// nothing rather than filtering the parent's, and `env_clear` is what makes that
    /// true in practice rather than only on paper.
    pub fn spawn(spec: &HostSpec) -> Result<Self, SpawnError> {
        // Checked before spawning, because `env_clear` leaves no `PATH` and the
        // operating system's answer for a bare name would be "no such file" — true,
        // and no help at all to whoever configured it.
        if !spec.program.is_absolute() {
            return Err(SpawnError::NotAbsolute {
                program: spec.program.display().to_string(),
            });
        }
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|error| SpawnError::Launch {
            program: spec.program.display().to_string(),
            error,
        })?;

        let stdin = child.stdin.take().ok_or(SpawnError::Pipes)?;
        let stdout = child.stdout.take().ok_or(SpawnError::Pipes)?;
        let stderr = child.stderr.take().ok_or(SpawnError::Pipes)?;

        let (tx, events) = channel();
        std::thread::spawn(move || pump_messages(BufReader::new(stdout), &tx));

        let errors = Arc::new(Mutex::new(ErrorLog::default()));
        let log = Arc::clone(&errors);
        std::thread::spawn(move || pump_lines(BufReader::new(stderr), &log));

        Ok(Self {
            child,
            stdin,
            events,
            errors,
            next_id: 1,
            pending: BTreeMap::new(),
        })
    }

    /// Sends a message.
    pub fn send(&mut self, message: &Message) -> std::io::Result<()> {
        // One line per message, newline included: the reader on the other side splits
        // on it.
        self.stdin.write_all(message.encode().as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()
    }

    /// Sends a request and returns the id it was given.
    pub fn request(&mut self, method: &str, params: Value) -> std::io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, method.to_owned());
        self.send(&Message::Request(Request {
            id,
            method: method.to_owned(),
            params,
        }))?;
        Ok(id)
    }

    /// Sends a notification.
    pub fn notify(&mut self, method: &str, params: Value) -> std::io::Result<()> {
        self.send(&Message::Notification(Notification {
            method: method.to_owned(),
            params,
        }))
    }

    /// Asks the host to load an extension and run its `activate`.
    ///
    /// `path` is the extension directory **as the host sees it**, which is not
    /// the same as deco's own path when the host is in a container: translate it
    /// through [`crate::sandbox::Prepared::seen_by_host`] first. A host path
    /// passed straight through would be outside every mount and fail to open,
    /// which is a confusing way to learn that a container is involved.
    pub fn activate(&mut self, path: &str, main: &str) -> std::io::Result<u64> {
        self.request(
            ACTIVATE,
            serde_json::json!({ "extensionPath": path, "main": main }),
        )
    }

    /// Asks the host to run one of the commands its extension registered.
    ///
    /// The reply carries whatever the extension's callback returned, or an error
    /// if it threw or the command is not registered there. This is the other
    /// direction of `commands.registerCommand`: the extension tells deco a name,
    /// and this is deco calling it back.
    pub fn execute_command(&mut self, command: &str, args: Value) -> std::io::Result<u64> {
        self.request(
            EXECUTE_COMMAND,
            serde_json::json!({ "command": command, "args": args }),
        )
    }

    /// The method a reply is answering, forgetting it in the process.
    pub fn answered(&mut self, id: u64) -> Option<String> {
        self.pending.remove(&id)
    }

    /// How many requests are waiting for a reply.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The next event, if one has arrived.
    pub fn poll(&mut self) -> Option<HostEvent> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            // The reader thread is gone, which it only does after sending `Closed`.
            Err(TryRecvError::Disconnected) => Some(HostEvent::Closed),
            Err(TryRecvError::Empty) => None,
        }
    }

    /// Waits for the host's `$/ready`, returning the events seen on the way.
    ///
    /// Anything that arrives before `$/ready` is handed back rather than dropped: a
    /// host that logs while starting up has said something worth keeping, and a
    /// request that arrives first still has to be answered.
    pub fn wait_for_ready(
        &mut self,
        timeout: Duration,
    ) -> (Result<(), ReadyError>, Vec<HostEvent>) {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            match self.poll() {
                Some(HostEvent::Message(Message::Notification(note)))
                    if note.method == "$/ready" =>
                {
                    return (agrees_on_protocol(&note), seen);
                }
                Some(HostEvent::Closed) => return (Err(ReadyError::Closed), seen),
                Some(other) => seen.push(other),
                None => {
                    if Instant::now() >= deadline {
                        return (
                            Err(ReadyError::TimedOut {
                                after_ms: timeout.as_millis() as u64,
                            }),
                            seen,
                        );
                    }
                    // A short sleep rather than a spin: starting a Node process takes
                    // tens of milliseconds and a busy wait would spend them burning a
                    // core.
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    /// The last lines the host wrote to stderr.
    pub fn errors(&self) -> String {
        self.errors
            .lock()
            .map(|log| log.joined())
            .unwrap_or_default()
    }

    /// Asks the host to stop, then makes sure it has.
    ///
    /// `$/shutdown` first, so the sandbox is restored and `deactivate` gets to run;
    /// then a kill, because an extension that ignores the notification must not keep
    /// the editor open.
    pub fn shutdown(&mut self) {
        let _ = self.notify("$/shutdown", Value::Null);
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // Gone, or unwaitable: either way there is nothing left to ask.
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // A host outliving the editor would keep running somebody's JavaScript with
        // nothing to report to.
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, DefaultPolicy, GrantStore, PathScope, ResolutionContext};
    use std::io::Cursor;
    use std::path::PathBuf;

    fn events(input: &str) -> Vec<HostEvent> {
        let (tx, rx) = channel();
        pump_messages(Cursor::new(input.to_owned()), &tx);
        drop(tx);
        rx.into_iter().collect()
    }

    fn ready_line() -> String {
        format!(
            r#"{{"type":"notification","method":"$/ready","params":{{"protocol":"{PROTOCOL_VERSION}"}}}}"#
        )
    }

    #[test]
    fn one_message_per_line_is_read_in_order() {
        let input = format!(
            "{}\n{}\n",
            ready_line(),
            r#"{"type":"request","id":7,"method":"log.append","params":{}}"#
        );
        let seen = events(&input);
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(matches!(
            &seen[0],
            HostEvent::Message(Message::Notification(n)) if n.method == "$/ready"
        ));
        assert!(matches!(
            &seen[1],
            HostEvent::Message(Message::Request(r)) if r.id == 7
        ));
        assert_eq!(seen[2], HostEvent::Closed, "the stream ended");
    }

    #[test]
    fn a_blank_line_is_not_a_message() {
        let seen = events(&format!("\n\n{}\n", ready_line()));
        assert_eq!(seen.len(), 2, "{seen:?}");
    }

    #[test]
    fn one_unreadable_line_costs_one_message_and_not_the_stream() {
        // The reason for newlines rather than length prefixes: the next message starts
        // at a position that is still known.
        let seen = events(&format!("{{ not json\n{}\n", ready_line()));
        assert!(matches!(seen[0], HostEvent::Garbled(_)), "{seen:?}");
        assert!(
            matches!(
                &seen[1],
                HostEvent::Message(Message::Notification(n)) if n.method == "$/ready"
            ),
            "the reader carried on: {seen:?}"
        );
    }

    #[test]
    fn the_stream_ending_is_reported_once() {
        assert_eq!(events(""), vec![HostEvent::Closed]);
    }

    #[test]
    fn the_error_log_keeps_the_last_lines_not_the_first() {
        // When a process dies, the reason is at the end.
        let log = Mutex::new(ErrorLog::default());
        let mut input = String::new();
        for i in 0..(ERROR_LOG_LINES + 5) {
            input.push_str(&format!("line {i}\n"));
        }
        pump_lines(Cursor::new(input), &log);
        let joined = log.lock().unwrap().joined();
        assert!(joined.contains(&format!("line {}", ERROR_LOG_LINES + 4)));
        assert!(!joined.contains("line 0"), "the oldest was dropped");
        assert_eq!(joined.lines().count(), ERROR_LOG_LINES);
    }

    #[test]
    fn invalid_utf8_on_stderr_is_replaced_rather_than_fatal() {
        let log = Mutex::new(ErrorLog::default());
        pump_lines(Cursor::new(b"bad \xff byte\nand more\n".to_vec()), &log);
        let joined = log.lock().unwrap().joined();
        assert_eq!(joined.lines().count(), 2, "{joined:?}");
    }

    // ---- The capability seam ----------------------------------------------

    fn broker(declared: Vec<Capability>, policy: DefaultPolicy) -> Broker {
        Broker::new(
            declared,
            GrantStore::default(),
            policy,
            ResolutionContext {
                workspace_roots: vec![PathBuf::from("/w")],
                ..Default::default()
            },
        )
    }

    fn request(method: &str, params: Value) -> Request {
        Request {
            id: 1,
            method: method.to_owned(),
            params,
        }
    }

    #[test]
    fn a_method_deco_does_not_know_is_refused() {
        // Fails closed on the name alone: a host from a newer deco cannot reach an
        // older one's editor surface by naming something it has never heard of.
        let broker = broker(Vec::new(), DefaultPolicy::Allow);
        let refusal = dispatch(&broker, &request("fs.deleteEverything", Value::Null));
        match refusal {
            Dispatch::Refused(response) => {
                let error = response.error.expect("a refusal carries one");
                assert_eq!(error.code, ErrorCode::MethodNotFound);
                assert!(error.message.contains("fs.deleteEverything"), "{error:?}");
            }
            other => panic!("should have been refused: {other:?}"),
        }
    }

    #[test]
    fn a_mediated_method_needs_no_capability_at_all() {
        // These only touch state deco already owns and shows to the user, so an
        // extension that declared nothing can still register a command.
        let broker = broker(Vec::new(), DefaultPolicy::Deny);
        for method in [
            "commands.registerCommand",
            "window.showInformationMessage",
            "log.append",
            "$/ready",
        ] {
            assert_eq!(
                dispatch(&broker, &request(method, Value::Null)),
                Dispatch::Allowed,
                "{method}"
            );
        }
    }

    #[test]
    fn a_capability_the_manifest_never_declared_is_refused() {
        // The declaration is a ceiling, not a starting point: whatever the user has
        // agreed to since, an extension cannot exceed what it asked for in writing.
        let broker = broker(Vec::new(), DefaultPolicy::Allow);
        let params = serde_json::json!({ "path": "/w/src/main.rs" });
        match dispatch(&broker, &request("fs.readFile", params)) {
            Dispatch::Refused(response) => {
                assert_eq!(
                    response.error.expect("a refusal carries one").code,
                    ErrorCode::PermissionDenied
                );
            }
            other => panic!("should have been refused: {other:?}"),
        }
    }

    #[test]
    fn a_declared_capability_is_allowed_within_its_scope() {
        let broker = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            dispatch(
                &broker,
                &request(
                    "fs.readFile",
                    serde_json::json!({ "path": "/w/src/main.rs" })
                )
            ),
            Dispatch::Allowed
        );
    }

    #[test]
    fn a_declared_capability_outside_its_scope_is_still_refused() {
        // The classic target, reached by walking out of the workspace.
        let broker = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Allow,
        );
        assert!(matches!(
            dispatch(
                &broker,
                &request(
                    "fs.readFile",
                    serde_json::json!({ "path": "/w/../.ssh/id_ed25519" })
                )
            ),
            Dispatch::Refused(_)
        ));
    }

    #[test]
    fn a_policy_of_prompt_produces_a_question_rather_than_an_answer() {
        let broker = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Prompt,
        );
        assert!(matches!(
            dispatch(
                &broker,
                &request("fs.readFile", serde_json::json!({ "path": "/w/a.rs" }))
            ),
            Dispatch::Consent { .. }
        ));
    }

    // ---- Starting a process ------------------------------------------------

    #[test]
    fn a_program_named_rather_than_located_is_refused_with_the_reason() {
        // Found by using this: the environment is built from nothing, so there is no
        // `PATH`, and a bare `node` fails as "no such file" — which sends whoever
        // configured it looking for a missing file rather than for a missing directory
        // name.
        let spec = HostSpec {
            program: PathBuf::from("node"),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: std::env::temp_dir(),
        };
        match Host::spawn(&spec) {
            Err(error @ SpawnError::NotAbsolute { .. }) => {
                let said = error.to_string();
                assert!(said.contains("absolute"), "{said}");
                assert!(said.contains("PATH"), "{said}");
            }
            Err(other) => panic!("{other} instead of a named refusal"),
            Ok(_) => panic!("a bare program name should not have started"),
        }
    }

    #[test]
    fn a_program_that_does_not_exist_is_a_named_failure() {
        // Built from the temporary directory rather than written down, because a
        // literal `/nonexistent/...` is only absolute on Unix — on Windows it has no
        // drive letter, so `spawn` would refuse it for the other reason and this test
        // would pass for a lie.
        let spec = HostSpec {
            program: std::env::temp_dir().join("deco-node-that-is-not-there"),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: std::env::temp_dir(),
        };
        match Host::spawn(&spec) {
            Err(SpawnError::Launch { program, .. }) => {
                assert!(program.contains("node-that-is-not-there"), "{program}");
            }
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("should not have started"),
        }
    }

    #[test]
    fn a_ready_with_the_wrong_protocol_is_refused() {
        // The Node side checks first, so reaching this means the two ends disagree
        // about which of them is wrong — still better than half-speaking.
        let ready = |protocol: &str| Notification {
            method: "$/ready".to_owned(),
            params: serde_json::json!({ "protocol": protocol }),
        };
        assert_eq!(
            agrees_on_protocol(&ready("0.0.0-ancient")),
            Err(ReadyError::Protocol {
                host: "0.0.0-ancient".to_owned()
            })
        );
        assert_eq!(agrees_on_protocol(&ready(PROTOCOL_VERSION)), Ok(()));
    }

    #[test]
    fn a_ready_that_names_no_protocol_at_all_is_refused() {
        // Absent is not "the current one": a host that does not say is a host deco
        // cannot know it agrees with.
        assert!(agrees_on_protocol(&Notification {
            method: "$/ready".to_owned(),
            params: Value::Null,
        })
        .is_err());
    }

    #[test]
    fn every_error_says_what_to_do_about_it() {
        // These reach the user through the problem list, so they have to read as
        // sentences rather than as variant names.
        assert!(ReadyError::TimedOut { after_ms: 10_000 }
            .to_string()
            .contains("10000 ms"));
        assert!(ReadyError::Closed.to_string().contains("exited"));
        assert!(ReadyError::Protocol {
            host: "9.9.9".to_owned()
        }
        .to_string()
        .contains(PROTOCOL_VERSION));
    }
}
