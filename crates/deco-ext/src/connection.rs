//! Starting a host process and exchanging messages with it.
//!
//! [`crate::host::build_spec`] defines the command to run, and [`crate::protocol`]
//! defines the messages. This module starts the process and carries the messages
//! between the two sides.
//!
//! # The framing
//!
//! One JSON object per line, which is what the Node side writes. This is not
//! `Content-Length` framing like the Language Server Protocol: there is no external
//! specification to match, since deco owns both sides. A reader can resynchronise
//! at the next newline, so a bad frame loses one message instead of the rest of the
//! stream.
//!
//! # Where the capability model is applied
//!
//! [`dispatch`] is the only path from an inbound request to the editor. It is a
//! pure function of the broker and the request, so every path through it can be
//! tested without a process. It denies in two cases: a method that
//! [`crate::protocol::required_capabilities`] does not recognise, and a capability
//! the manifest did not declare.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::capability::{Broker, CheckResult};
use crate::host::{HostSpec, PROTOCOL_VERSION};
use crate::protocol::{required_capabilities, ErrorCode, Message, Notification, Request, Response};

/// An event from the reader.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// A message arrived.
    Message(Message),
    /// A line could not be decoded.
    ///
    /// Not terminal, unlike a length-prefixed framing error: the next newline is a
    /// known position, so one unreadable line loses one message.
    Garbled(String),
    /// The stream ended, which means the process is exiting or has exited.
    Closed,
}

/// The last lines the host wrote to stderr.
///
/// Bounded, because a host that logs on every keystroke would otherwise grow this
/// for the whole session. The *last* lines are kept, not the first, because the
/// reason a process exits is usually at the end.
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
/// Returns when the stream closes or the receiver is dropped. A line that does not
/// parse is reported and skipped instead of ending the read. The framing is a
/// newline, so the next message starts at a known position.
pub fn pump_messages(reader: impl BufRead, tx: &Sender<HostEvent>) {
    for line in reader.lines() {
        let Ok(line) = line else {
            // An I/O error on the pipe means the process has exited.
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
/// Invalid UTF-8 is replaced instead of ending the read. This is a diagnostic
/// channel, and a stray byte from the host must not discard the log.
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
/// The only path from a request to the editor. It is a pure function, so every path
/// through it is testable without a process. It denies in two cases:
///
/// - a method that [`required_capabilities`] does not recognise is rejected as
///   unknown, so a host from a newer deco cannot reach an older deco's editor
///   surface through a method the older version does not know;
/// - a capability the manifest did not declare is denied by the broker, regardless
///   of later user grants. The declaration is an upper bound.
///
/// A request that needs several capabilities, such as a rename, which touches two
/// paths, is allowed only when every one is allowed. A denial of any of them refuses
/// it, even when another still needs consent, so the user is not asked about a
/// request that would be refused anyway. Otherwise the first capability that needs
/// consent is returned. Only one question is asked at a time, so the caller asks
/// again about the next one after the answer.
pub fn dispatch(broker: &Broker, request: &Request) -> Dispatch {
    let Ok(needed) = required_capabilities(&request.method, &request.params) else {
        return Dispatch::Refused(Response::err(
            request.id,
            ErrorCode::MethodNotFound,
            format!("deco does not know the method `{}`", request.method),
        ));
    };
    // An empty list is allowed: the method only affects state that deco owns and
    // shows to the user.
    let mut consent = None;
    for capability in &needed {
        match broker.check(capability) {
            CheckResult::Allowed => {}
            CheckResult::NeedsConsent { capability } => {
                consent.get_or_insert(capability);
            }
            CheckResult::Denied { reason } => {
                return Dispatch::Refused(Response::err(
                    request.id,
                    ErrorCode::PermissionDenied,
                    reason.to_string(),
                ));
            }
        }
    }
    match consent {
        Some(capability) => Dispatch::Consent { capability },
        None => Dispatch::Allowed,
    }
}

/// Whether a `$/ready` notification agrees with deco about the protocol.
///
/// A separate function so the rule can be tested without a process, using the same
/// code as the handshake.
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
    /// The program could not be run.
    Launch {
        /// The program that was tried.
        program: String,
        /// The operating system error.
        error: std::io::Error,
    },
    /// A pipe to the process could not be taken.
    Pipes,
    /// The program is a bare name, not an absolute path.
    ///
    /// The host's environment is built from scratch, so it has no `PATH` for the
    /// operating system to search. A bare `node` would fail as "no such file", which
    /// does not explain the cause. This error states the cause instead.
    NotAbsolute {
        /// The configured program.
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
    /// It did not send `$/ready` in time.
    TimedOut {
        /// The timeout.
        after_ms: u64,
    },
    /// It exited, or its pipe closed, before sending `$/ready`.
    Closed,
    /// It sent `$/ready` with a protocol version deco does not support.
    ///
    /// The Node side checks the version first, so this is reached only when the two
    /// sides' checks disagree. Rejecting is safer than using a mismatched protocol.
    Protocol {
        /// The version the host reported.
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
/// The host exits as soon as it handles `$/shutdown`, so this mainly covers the
/// time to deliver the notification and for the process to exit. It is short
/// enough that a host that does not stop does not delay quitting the editor.
pub const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// The request that loads an extension and runs its `activate`.
pub const ACTIVATE: &str = "$/activate";

/// The request that runs a command the extension registered.
///
/// Defined once here because both sides must use the same name. The other side is
/// `extension-host/src/vscode.js`.
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
    /// Kept so a reply can be matched to its request. The id alone does not identify
    /// the method, and the caller needs the method to route the result.
    pending: BTreeMap<u64, String>,
}

impl Host {
    /// Starts the process described by `spec`.
    ///
    /// The environment is exactly `spec`'s. [`crate::host::build_spec`] builds it from
    /// scratch instead of filtering the parent's, and `env_clear` ensures that no
    /// inherited variable remains.
    pub fn spawn(spec: &HostSpec) -> Result<Self, SpawnError> {
        // Checked before spawning. `env_clear` leaves no `PATH`, so the operating
        // system would report a bare name only as "no such file", which does not
        // explain the cause.
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
        // One line per message, including the newline. The reader on the other side
        // splits on it.
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
    /// `path` is the extension directory **as the host sees it**. When the host
    /// runs in a container, this differs from deco's own path: translate it with
    /// [`crate::sandbox::Prepared::seen_by_host`] first. An untranslated path is
    /// outside every mount and fails to open, with an error that does not mention
    /// the container.
    pub fn activate(&mut self, path: &str, main: &str) -> std::io::Result<u64> {
        self.request(
            ACTIVATE,
            serde_json::json!({ "extensionPath": path, "main": main }),
        )
    }

    /// Asks the host to run one of the commands its extension registered.
    ///
    /// The reply carries the value the extension's callback returned, or an error
    /// if the callback threw or the command is not registered in this host. This is
    /// the reverse direction of `commands.registerCommand`: the extension registers
    /// a name with deco, and deco uses this to invoke it.
    pub fn execute_command(&mut self, command: &str, args: Value) -> std::io::Result<u64> {
        self.request(
            EXECUTE_COMMAND,
            serde_json::json!({ "command": command, "args": args }),
        )
    }

    /// The method a reply answers. Removes the request from the pending list.
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
            // The reader thread has exited, which it only does after sending `Closed`.
            Err(TryRecvError::Disconnected) => Some(HostEvent::Closed),
            Err(TryRecvError::Empty) => None,
        }
    }

    /// Waits for the host's `$/ready`, returning the events seen on the way.
    ///
    /// Events that arrive before `$/ready` are returned instead of dropped. Log
    /// output during startup is worth keeping, and a request that arrives first
    /// still needs a reply.
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
                    // Sleep briefly instead of spinning. Starting a Node process takes
                    // tens of milliseconds, and a busy wait would occupy a core for
                    // that time.
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

    /// Asks the host to stop, then kills it if it has not exited.
    ///
    /// Sends `$/shutdown` first. The host restores its sandbox and exits
    /// immediately; it does not call the extension's `deactivate`, which runs
    /// only on a `$/deactivate` request, and deco does not send one. If the
    /// process has not exited after [`SHUTDOWN_GRACE`], it is killed, because a
    /// host that does not stop must not keep the editor open.
    pub fn shutdown(&mut self) {
        let _ = self.notify("$/shutdown", Value::Null);
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // Grace period expired, or waiting failed: stop polling.
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // A host that outlived the editor would keep running extension code with no
        // editor to report to.
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
        // This is why the framing uses newlines instead of length prefixes: the next
        // message starts at a known position.
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
        // When a process exits, the reason is usually at the end.
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
        // Rejected by name alone: a host from a newer deco cannot reach an older
        // deco's editor surface through a method the older version does not know.
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
        // These only affect state that deco owns and shows to the user, so an
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
        // The declaration is an upper bound. Regardless of later user grants, an
        // extension cannot exceed what its manifest declares.
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
        // An SSH key, reached through `..` from inside the workspace.
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

    // ---- Requests that touch two paths -------------------------------------

    fn subtree(path: &str) -> PathScope {
        PathScope::Subtree { path: path.into() }
    }

    /// Writes under `/w/allowed`, reads under `/w/readonly`, and nothing else.
    fn two_scopes(policy: DefaultPolicy) -> Broker {
        broker(
            vec![
                Capability::WriteFile {
                    scope: subtree("/w/allowed"),
                },
                Capability::ReadFile {
                    scope: subtree("/w/readonly"),
                },
            ],
            policy,
        )
    }

    fn transfer(method: &str, source: &str, target: &str) -> Request {
        request(
            method,
            serde_json::json!({ "source": source, "target": target }),
        )
    }

    fn refused_for_permission(dispatched: Dispatch) {
        match dispatched {
            Dispatch::Refused(response) => assert_eq!(
                response.error.expect("a refusal carries one").code,
                ErrorCode::PermissionDenied
            ),
            other => panic!("should have been refused: {other:?}"),
        }
    }

    #[test]
    fn a_rename_out_of_an_undeclared_source_is_refused() {
        // The target is covered. Checking only the target would let an extension
        // move a key into its own directory and read it there.
        let broker = two_scopes(DefaultPolicy::Allow);
        refused_for_permission(dispatch(
            &broker,
            &transfer("fs.rename", "/home/u/.ssh/id_ed25519", "/w/allowed/x"),
        ));
    }

    #[test]
    fn a_rename_source_that_climbs_out_with_dots_is_refused() {
        let broker = two_scopes(DefaultPolicy::Allow);
        refused_for_permission(dispatch(
            &broker,
            &transfer(
                "fs.rename",
                "/w/allowed/../../home/u/.ssh/id",
                "/w/allowed/x",
            ),
        ));
    }

    #[test]
    fn a_rename_within_the_writable_scope_is_allowed() {
        let broker = two_scopes(DefaultPolicy::Allow);
        assert_eq!(
            dispatch(
                &broker,
                &transfer("fs.rename", "/w/allowed/a", "/w/allowed/b")
            ),
            Dispatch::Allowed
        );
    }

    #[test]
    fn a_rename_out_of_a_read_only_scope_is_refused() {
        // Moving a file out of a directory changes that directory.
        let broker = two_scopes(DefaultPolicy::Allow);
        refused_for_permission(dispatch(
            &broker,
            &transfer("fs.rename", "/w/readonly/a", "/w/allowed/a"),
        ));
    }

    #[test]
    fn a_copy_needs_only_to_read_its_source() {
        let broker = two_scopes(DefaultPolicy::Allow);
        assert_eq!(
            dispatch(
                &broker,
                &transfer("fs.copy", "/w/readonly/a", "/w/allowed/a")
            ),
            Dispatch::Allowed
        );
    }

    #[test]
    fn a_copy_from_an_undeclared_source_is_refused() {
        let broker = two_scopes(DefaultPolicy::Allow);
        refused_for_permission(dispatch(
            &broker,
            &transfer("fs.copy", "/home/u/.ssh/id_ed25519", "/w/allowed/x"),
        ));
    }

    #[test]
    fn a_copy_into_a_read_only_scope_is_refused() {
        let broker = two_scopes(DefaultPolicy::Allow);
        refused_for_permission(dispatch(
            &broker,
            &transfer("fs.copy", "/w/allowed/a", "/w/readonly/a"),
        ));
    }

    #[test]
    fn consent_is_asked_for_one_path_at_a_time_target_first() {
        // The consent prompt holds one question. After the first answer, the
        // request asks about the path that is still undecided.
        let mut broker = two_scopes(DefaultPolicy::Prompt);
        let rename = transfer("fs.rename", "/w/allowed/a", "/w/allowed/b");
        let target = Capability::WriteFile {
            scope: subtree("/w/allowed/b"),
        };
        let source = Capability::WriteFile {
            scope: subtree("/w/allowed/a"),
        };
        assert_eq!(
            dispatch(&broker, &rename),
            Dispatch::Consent {
                capability: target.clone()
            }
        );
        broker.remember(target, crate::capability::Decision::Allow);
        assert_eq!(
            dispatch(&broker, &rename),
            Dispatch::Consent {
                capability: source.clone()
            }
        );
        broker.remember(source, crate::capability::Decision::Allow);
        assert_eq!(dispatch(&broker, &rename), Dispatch::Allowed);
    }

    #[test]
    fn a_denied_source_refuses_without_asking_about_the_target() {
        // Asking about the target would be pointless: the request would be refused
        // whatever the answer.
        let broker = two_scopes(DefaultPolicy::Prompt);
        refused_for_permission(dispatch(
            &broker,
            &transfer("fs.rename", "/home/u/.ssh/id_ed25519", "/w/allowed/x"),
        ));
    }

    // ---- Starting a process ------------------------------------------------

    #[test]
    fn a_program_named_rather_than_located_is_refused_with_the_reason() {
        // Found in practice. The environment is built from scratch, so there is no
        // `PATH`, and a bare `node` fails as "no such file". That message suggests a
        // missing file instead of a missing directory in the path.
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
        // Derive an absolute path for the current platform. `/nonexistent/...`
        // has no drive letter on Windows and would test path validation instead
        // of the intended process-spawn failure.
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
        // The Node side checks first, so this is reached only when the two sides'
        // checks disagree. Rejecting is safer than using a mismatched protocol.
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
        // A missing version does not mean the current one. deco cannot confirm
        // agreement with a host that reports no version.
        assert!(agrees_on_protocol(&Notification {
            method: "$/ready".to_owned(),
            params: Value::Null,
        })
        .is_err());
    }

    #[test]
    fn every_error_says_what_to_do_about_it() {
        // These reach the user through the problem list, so they must be sentences,
        // not variant names.
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
