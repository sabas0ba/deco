//! Running a language server as a child process and moving bytes to and from it.
//!
//! Everything else in this crate is a pure state machine. This module is the
//! part that is not: it spawns a program, owns two threads, and has to shut all
//! of that down without leaking. The rules it follows, and the failure each one
//! prevents:
//!
//! - **A workspace-scoped server is not launched without confirmation.**
//!   [`ServerProcess::spawn`] takes a [`Consent`] argument rather than reading
//!   a flag, so the check cannot be forgotten at a call site. Cloning a
//!   repository must not be enough to run a program.
//! - **The command is an argument vector.** No shell is involved anywhere; see
//!   [`crate::server`].
//! - **stderr is drained continuously.** A pipe nobody reads fills up, and the
//!   next write from the child blocks — forever, since the editor is waiting on
//!   stdout. A server that logs verbosely would appear to hang. The last lines
//!   are kept in a bounded buffer, because they are the only explanation
//!   available when a server dies during startup.
//! - **Shutting down is `shutdown`, then `exit`, then wait, then kill.** A
//!   process that is only dropped becomes an orphan holding a lock on the
//!   project; a process that is only killed never flushes.
//! - **A frame is size-checked before it is allocated.** Inherited from
//!   [`crate::jsonrpc::MAX_FRAME_BYTES`]: a server is a program the user
//!   installed, not a trusted part of the editor.
//!
//! The pumping is split out into [`pump_messages`] and [`pump_lines`], which
//! take any reader, so the interesting behaviour is tested against in-memory
//! streams rather than against a language server that CI would have to install.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Command as OsCommand, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::jsonrpc::{self, Message, ProtocolError};
use crate::server::{ServerConfig, Trust};

/// Whether the user has agreed to run this particular server.
///
/// A separate type rather than a `bool` so that `spawn(config, true)` cannot be
/// written by accident, and so the decision is visible in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// The definition is trusted by where it came from, or the user has agreed.
    Granted,
    /// No decision has been made. A workspace-scoped server will not start.
    NotAsked,
}

/// What the reader thread saw.
#[derive(Debug)]
pub enum ReaderEvent {
    /// A message arrived.
    Message(Message),
    /// The stream ended cleanly. The server is gone.
    Closed,
    /// The stream produced something unusable.
    ///
    /// Carried as a string rather than the error: the receiver is on another
    /// thread and only ever renders this for a log.
    Failed(String),
}

/// The last lines a server wrote to stderr.
///
/// Bounded, because a server that logs a line per keystroke would otherwise
/// grow this without limit for the entire session. The *last* lines are kept
/// rather than the first: when a server dies, the reason is at the end.
#[derive(Debug, Clone)]
pub struct ErrorLog {
    lines: VecDeque<String>,
    capacity: usize,
    /// Total lines seen, including the ones dropped, so a summary can say how
    /// much is missing rather than implying this is everything.
    total: usize,
}

impl ErrorLog {
    /// A log holding at most `capacity` lines.
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            capacity: capacity.max(1),
            total: 0,
        }
    }

    /// Records a line, evicting the oldest if the log is full.
    pub fn push(&mut self, line: impl Into<String>) {
        self.total += 1;
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line.into());
    }

    /// The retained lines, oldest first.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    /// How many lines the server wrote in total, including dropped ones.
    pub fn total(&self) -> usize {
        self.total
    }

    /// How many were dropped to stay within capacity.
    pub fn dropped(&self) -> usize {
        self.total - self.lines.len()
    }

    /// Whether the server has said nothing.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// A one-paragraph rendering for an error message.
    pub fn summary(&self) -> String {
        if self.lines.is_empty() {
            return "the server wrote nothing to stderr".to_owned();
        }
        let mut text = self.lines().collect::<Vec<_>>().join("\n");
        if self.dropped() > 0 {
            text = format!("[{} earlier lines dropped]\n{text}", self.dropped());
        }
        text
    }
}

/// Reads framed messages until the stream ends, sending each one on.
///
/// Returns when the stream closes or the receiver hangs up. A protocol error is
/// terminal: the framing is length-prefixed, so a bad frame means the stream
/// position is no longer known and every subsequent read would be garbage.
pub fn pump_messages(mut reader: impl BufRead, tx: &Sender<ReaderEvent>) {
    loop {
        let event = match jsonrpc::read(&mut reader) {
            Ok(Some(message)) => ReaderEvent::Message(message),
            Ok(None) => {
                let _ = tx.send(ReaderEvent::Closed);
                return;
            }
            Err(error) => {
                let _ = tx.send(ReaderEvent::Failed(describe(&error)));
                return;
            }
        };
        // A send failure means the editor dropped the process. Stopping is the
        // correct response; there is nobody left to tell.
        if tx.send(event).is_err() {
            return;
        }
    }
}

fn describe(error: &ProtocolError) -> String {
    match error {
        // A truncated frame is what a server crashing mid-write looks like, and
        // saying "unexpected end of file" alone sends people hunting for a bug
        // in the editor.
        ProtocolError::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
            format!("the server stopped mid-message ({io})")
        }
        other => other.to_string(),
    }
}

/// Reads lines until the stream ends, appending each to a shared log.
///
/// Invalid UTF-8 is replaced rather than fatal: this is a diagnostic channel,
/// and a server that logs a stray byte should not cost the log.
pub fn pump_lines(reader: impl BufRead, log: &Mutex<ErrorLog>) {
    for line in reader.split(b'\n') {
        let Ok(bytes) = line else { return };
        let text = String::from_utf8_lossy(&bytes);
        let text = text.trim_end_matches('\r');
        if text.is_empty() {
            continue;
        }
        // A poisoned mutex means a pump thread panicked. The log is only ever
        // read for display, so recovering is strictly better than propagating.
        match log.lock() {
            Ok(mut log) => log.push(text),
            Err(poisoned) => poisoned.into_inner().push(text),
        }
    }
}

/// Why a server could not be started.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The definition needs the user's agreement and did not have it.
    #[error("`{id}` is defined by this workspace and has not been approved to run")]
    NeedsConsent {
        /// The server id.
        id: String,
    },
    /// The program could not be executed.
    ///
    /// Overwhelmingly "not installed", which is worth saying plainly: deco
    /// cannot install a language server and should not pretend the failure is
    /// mysterious.
    #[error("could not run `{program}`: {source}")]
    NotRunnable {
        /// What was attempted.
        program: String,
        /// Why it failed.
        source: std::io::Error,
    },
    /// The child was spawned but a pipe was missing, which should not happen.
    #[error("`{program}` started but its {stream} could not be captured")]
    NoPipe {
        /// What was started.
        program: String,
        /// Which stream.
        stream: &'static str,
    },
}

/// How long to wait for a server to exit on its own before killing it.
///
/// Long enough for a server to finish flushing an index, short enough that
/// quitting the editor does not feel broken.
pub const EXIT_GRACE: Duration = Duration::from_millis(2000);

/// How many stderr lines to keep.
const STDERR_LINES: usize = 200;

/// A running language server.
pub struct ServerProcess {
    id: String,
    program: String,
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<ReaderEvent>,
    stderr: Arc<Mutex<ErrorLog>>,
    stdout_thread: Option<JoinHandle<()>>,
    /// Held separately from the stdout pump so it can be waited on alone: a
    /// finished stderr thread is the only reliable signal that everything the
    /// server said has been collected. See [`Self::stderr_after_exit`].
    stderr_thread: Option<JoinHandle<()>>,
}

impl ServerProcess {
    /// Starts a server.
    ///
    /// `consent` is checked against the definition's [`Trust`]: a
    /// workspace-scoped definition without [`Consent::Granted`] is refused, and
    /// nothing is spawned.
    pub fn spawn(config: &ServerConfig, consent: Consent) -> Result<Self, SpawnError> {
        if config.trust.needs_confirmation() && consent != Consent::Granted {
            return Err(SpawnError::NeedsConsent {
                id: config.id.clone(),
            });
        }

        let program = config.command.program.clone();
        let mut command = OsCommand::new(&program);
        command
            .args(&config.command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped rather than inherited: a server writing to the terminal's
            // real stderr would paint over the editor's alternate screen.
            .stderr(Stdio::piped());
        for (name, value) in &config.env {
            command.env(name, value);
        }

        let mut child = command.spawn().map_err(|source| SpawnError::NotRunnable {
            program: program.clone(),
            source,
        })?;

        let stdout = child.stdout.take().ok_or(SpawnError::NoPipe {
            program: program.clone(),
            stream: "stdout",
        })?;
        let stderr = child.stderr.take().ok_or(SpawnError::NoPipe {
            program: program.clone(),
            stream: "stderr",
        })?;
        let stdin = child.stdin.take().ok_or(SpawnError::NoPipe {
            program: program.clone(),
            stream: "stdin",
        })?;

        let (tx, incoming) = mpsc::channel();
        let log = Arc::new(Mutex::new(ErrorLog::new(STDERR_LINES)));

        let stdout_thread = std::thread::Builder::new()
            .name(format!("deco-lsp-{}-stdout", config.id))
            .spawn(move || pump_messages(BufReader::new(stdout), &tx))
            .map_err(|source| SpawnError::NotRunnable {
                program: program.clone(),
                source,
            })?;

        // Its own thread, not folded into the stdout loop: both can block, and
        // an unread stderr pipe filling up would stall the server's next write.
        let stderr_log = Arc::clone(&log);
        let stderr_thread = std::thread::Builder::new()
            .name(format!("deco-lsp-{}-stderr", config.id))
            .spawn(move || pump_lines(BufReader::new(stderr), &stderr_log))
            .map_err(|source| SpawnError::NotRunnable {
                program: program.clone(),
                source,
            })?;

        Ok(Self {
            id: config.id.clone(),
            program,
            child,
            stdin: Some(stdin),
            incoming,
            stderr: log,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
        })
    }

    /// The server's id, as defined in settings.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The program being run.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Writes one message to the server.
    pub fn send(&mut self, message: &Message) -> Result<(), ProtocolError> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            ProtocolError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the server's stdin is closed",
            ))
        })?;
        jsonrpc::write(stdin, message)
    }

    /// Takes the next message if one is waiting, without blocking.
    pub fn try_recv(&self) -> Option<ReaderEvent> {
        // Both failures collapse to `None`. `Empty` means nothing yet;
        // `Disconnected` means the reader thread finished, which it only does
        // after sending `Closed` or `Failed` — so that news has already been
        // delivered and there is nothing further to report.
        self.incoming.try_recv().ok()
    }

    /// Waits up to `timeout` for the next message.
    ///
    /// Used during startup, where the editor genuinely has to wait for the
    /// `initialize` reply before it can send anything else.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<ReaderEvent> {
        // As in `try_recv`: a disconnected channel has already delivered
        // whatever the reader thread had to say.
        self.incoming.recv_timeout(timeout).ok()
    }

    /// The last lines the server wrote to stderr, as collected so far.
    ///
    /// "So far" is the important part: the pump runs on its own thread, so a
    /// caller that has just learned the server is gone may well be reading this
    /// before the reason arrived. Use [`Self::stderr_after_exit`] when the
    /// output is being read *because* the server died.
    pub fn stderr_tail(&self) -> ErrorLog {
        match self.stderr.lock() {
            Ok(log) => log.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Everything the server wrote to stderr, once there is no more coming.
    ///
    /// Two waits, in order, because they answer different questions:
    ///
    /// 1. **Has the process exited?** Until it has, more output may still be
    ///    produced, so there is nothing to conclude from an empty log.
    /// 2. **Has the pump finished?** This is the part a naive implementation
    ///    gets wrong. "Is there output yet" is a guess that wins or loses
    ///    depending on machine speed — it passed on a developer laptop and
    ///    failed on a CI runner. The thread returning is a *fact*: the pump ends
    ///    when its read hits end of stream, which happens when the last handle
    ///    to the write end of the pipe closes.
    ///
    /// Both are bounded, because neither event is guaranteed: a server may not
    /// be exiting at all, and one that has exited may have left a grandchild
    /// holding the pipe open — `rust-analyzer` running `cargo` is exactly that
    /// shape. On a timeout this returns what has been collected, which is no
    /// worse than not having waited.
    pub fn stderr_after_exit(&mut self, grace: Duration) -> ErrorLog {
        let deadline = Instant::now() + grace;

        while self.child.try_wait().ok().flatten().is_none() {
            if Instant::now() >= deadline {
                return self.stderr_tail();
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        // `is_finished` rather than `join`, so a grandchild holding the pipe
        // open cannot turn this into an unbounded wait.
        if let Some(thread) = &self.stderr_thread {
            while !thread.is_finished() {
                if Instant::now() >= deadline {
                    return self.stderr_tail();
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        self.stderr_tail()
    }

    /// Whether the process has exited, and with what status.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        // `try_wait` also reaps, which is what keeps a finished server from
        // lingering as a zombie for the rest of the session.
        self.child.try_wait().ok().flatten()
    }

    /// Closes stdin, which is how a server is told there is nothing more coming.
    ///
    /// Separate from [`Self::stop`] because it must happen *after* `exit` is
    /// sent and not before: a server reading its stdin would see the close as
    /// an abrupt disconnect.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Waits for the process to exit, killing it if it outstays `grace`.
    ///
    /// Returns whether it left on its own. Both outcomes are normal enough to
    /// report rather than to fail on: some servers exit on `exit`, and some
    /// need the signal.
    pub fn stop(&mut self, grace: Duration) -> bool {
        self.close_stdin();

        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                self.join_threads();
                return true;
            }
            // Polling rather than blocking on `wait`, so the grace period is
            // actually bounded. 10ms is below the threshold of noticing and
            // costs nothing over a two-second window.
            std::thread::sleep(Duration::from_millis(10));
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
        self.join_threads();
        false
    }

    /// Joins the pump threads.
    ///
    /// They end on their own once the child's pipes close, which killing or
    /// reaping the child guarantees. Joining after that is what stops threads
    /// accumulating across a session that restarts a server repeatedly.
    fn join_threads(&mut self) {
        for thread in [self.stdout_thread.take(), self.stderr_thread.take()]
            .into_iter()
            .flatten()
        {
            let _ = thread.join();
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        // A dropped server that is never stopped becomes an orphan — still
        // holding a build lock on the project, still using a core. The grace
        // period here is short: a caller that wanted a clean shutdown should
        // have called `stop` after `exit`, and by the time `drop` runs the
        // editor is usually on its way out.
        if self.child.try_wait().ok().flatten().is_none() {
            self.stop(Duration::from_millis(200));
        } else {
            self.join_threads();
        }
    }
}

impl std::fmt::Debug for ServerProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `Child` and `JoinHandle` render as noise, and a
        // server is identified by its id and pid.
        f.debug_struct("ServerProcess")
            .field("id", &self.id)
            .field("program", &self.program)
            .field("pid", &self.child.id())
            .finish()
    }
}

/// Whether a config would be allowed to start, without starting it.
///
/// Lets a frontend decide whether to prompt before anything is spawned.
pub fn needs_consent(config: &ServerConfig) -> bool {
    config.trust == Trust::Workspace
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::{Notification, Request};
    use crate::server::Command;
    use std::io::Cursor;

    fn framed(messages: &[Message]) -> Vec<u8> {
        let mut out = Vec::new();
        for message in messages {
            jsonrpc::write(&mut out, message).unwrap();
        }
        out
    }

    fn notification(method: &str) -> Message {
        Message::Notification(Notification {
            method: method.into(),
            params: None,
        })
    }

    fn config(trust: Trust) -> ServerConfig {
        ServerConfig {
            id: "test".into(),
            language_ids: vec!["rust".into()],
            command: Command {
                program: "definitely-not-a-real-program-cbf3a1".into(),
                args: Vec::new(),
            },
            env: Vec::new(),
            initialization_options: None,
            trust,
        }
    }

    #[test]
    fn the_pump_forwards_every_message_then_reports_the_close() {
        let stream = framed(&[notification("a"), notification("b")]);
        let (tx, rx) = mpsc::channel();
        pump_messages(Cursor::new(stream), &tx);
        drop(tx);

        let methods: Vec<String> = rx
            .iter()
            .map(|event| match event {
                ReaderEvent::Message(m) => m.method().unwrap_or("?").to_owned(),
                ReaderEvent::Closed => "<closed>".into(),
                ReaderEvent::Failed(reason) => format!("<failed: {reason}>"),
            })
            .collect();
        assert_eq!(methods, vec!["a", "b", "<closed>"]);
    }

    #[test]
    fn a_clean_end_of_stream_is_closed_not_failed() {
        // The difference is how the editor reports it: a server that shut down
        // is not the same as a server that crashed.
        let (tx, rx) = mpsc::channel();
        pump_messages(Cursor::new(Vec::new()), &tx);
        assert!(matches!(rx.recv().unwrap(), ReaderEvent::Closed));
    }

    #[test]
    fn a_truncated_frame_is_reported_as_stopping_mid_message() {
        // What a server crashing mid-write looks like. Saying "unexpected end
        // of file" alone sends people looking for a bug in the editor.
        // Two messages, the second cut short: the first must still arrive, so
        // that a crash mid-conversation does not discard what was already said.
        let mut stream = framed(&[notification("a"), notification("b")]);
        stream.truncate(stream.len() - 5);
        let (tx, rx) = mpsc::channel();
        pump_messages(Cursor::new(stream), &tx);

        assert!(matches!(rx.recv().unwrap(), ReaderEvent::Message(_)));
        let ReaderEvent::Failed(reason) = rx.recv().unwrap() else {
            panic!("a truncated frame must be a failure");
        };
        assert!(reason.contains("mid-message"), "{reason}");
    }

    #[test]
    fn the_pump_stops_at_the_first_protocol_error() {
        // The framing is length-prefixed, so after a bad frame the stream
        // position is unknown and every later read would be garbage. Continuing
        // would turn one error into a flood.
        let mut stream = b"Content-Length: notanumber\r\n\r\n".to_vec();
        stream.extend_from_slice(&framed(&[notification("never-read")]));
        let (tx, rx) = mpsc::channel();
        pump_messages(Cursor::new(stream), &tx);
        drop(tx);

        let events: Vec<_> = rx.iter().collect();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(events[0], ReaderEvent::Failed(_)));
    }

    #[test]
    fn the_pump_stops_when_the_receiver_is_gone() {
        // Otherwise the thread spins reading a server nobody is listening to,
        // for as long as that server keeps talking.
        let messages: Vec<Message> = (0..50).map(|_| notification("a")).collect();
        let stream = framed(&messages);
        let (tx, rx) = mpsc::channel();
        drop(rx);
        // The test is that this returns at all rather than blocking forever.
        pump_messages(Cursor::new(stream), &tx);
    }

    #[test]
    fn a_request_survives_the_round_trip_through_the_pump() {
        let message = Message::Request(Request {
            id: 7.into(),
            method: "textDocument/hover".into(),
            params: Some(serde_json::json!({"line": 1})),
        });
        let (tx, rx) = mpsc::channel();
        pump_messages(Cursor::new(framed(std::slice::from_ref(&message))), &tx);
        let ReaderEvent::Message(received) = rx.recv().unwrap() else {
            panic!("expected a message");
        };
        assert_eq!(received, message);
    }

    #[test]
    fn stderr_lines_are_collected() {
        let log = Mutex::new(ErrorLog::new(10));
        pump_lines(Cursor::new(b"first\nsecond\n".as_ref()), &log);
        let log = log.into_inner().unwrap();
        assert_eq!(log.lines().collect::<Vec<_>>(), vec!["first", "second"]);
    }

    #[test]
    fn stderr_handles_crlf_and_a_missing_final_newline() {
        let log = Mutex::new(ErrorLog::new(10));
        pump_lines(Cursor::new(b"a\r\nb".as_ref()), &log);
        let log = log.into_inner().unwrap();
        assert_eq!(log.lines().collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn stderr_survives_invalid_utf8() {
        // A diagnostic channel must not be lost to a stray byte.
        let log = Mutex::new(ErrorLog::new(10));
        pump_lines(Cursor::new(b"ok\n\xff\xfe\nlast\n".as_ref()), &log);
        let log = log.into_inner().unwrap();
        assert_eq!(log.lines().count(), 3);
        assert!(log.lines().any(|l| l == "last"));
    }

    #[test]
    fn the_error_log_keeps_the_last_lines_not_the_first() {
        // When a server dies, the reason is at the end.
        let mut log = ErrorLog::new(3);
        for i in 0..10 {
            log.push(format!("line {i}"));
        }
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            vec!["line 7", "line 8", "line 9"]
        );
        assert_eq!(log.total(), 10);
        assert_eq!(log.dropped(), 7);
    }

    #[test]
    fn the_error_log_is_bounded() {
        // A server logging a line per keystroke would otherwise grow this for
        // the whole session.
        let mut log = ErrorLog::new(5);
        for i in 0..100_000 {
            log.push(format!("{i}"));
        }
        assert_eq!(log.lines().count(), 5);
    }

    #[test]
    fn a_zero_capacity_log_still_keeps_one_line() {
        // Rounded up rather than rejected: a log that silently discards
        // everything is worse than one that keeps the last line.
        let mut log = ErrorLog::new(0);
        log.push("only");
        assert_eq!(log.lines().collect::<Vec<_>>(), vec!["only"]);
    }

    #[test]
    fn the_summary_says_how_much_it_is_hiding() {
        let mut log = ErrorLog::new(2);
        for i in 0..5 {
            log.push(format!("line {i}"));
        }
        let summary = log.summary();
        assert!(summary.contains("3 earlier lines dropped"), "{summary}");
        assert!(summary.contains("line 4"), "{summary}");
    }

    #[test]
    fn an_empty_summary_says_so_rather_than_being_blank() {
        // A blank error message reads like the editor lost the reason.
        let log = ErrorLog::new(5);
        assert!(log.is_empty());
        assert_eq!(log.summary(), "the server wrote nothing to stderr");
    }

    #[test]
    fn a_workspace_server_is_not_spawned_without_consent() {
        // Cloning a repository must not be enough to run a program. Checked
        // before the spawn, so the refusal does not depend on the program being
        // absent.
        let config = config(Trust::Workspace);
        assert!(needs_consent(&config));
        assert!(matches!(
            ServerProcess::spawn(&config, Consent::NotAsked),
            Err(SpawnError::NeedsConsent { .. })
        ));
    }

    #[test]
    fn a_user_or_builtin_server_needs_no_consent() {
        for trust in [Trust::User, Trust::BuiltIn] {
            let config = config(trust);
            assert!(!needs_consent(&config));
            // It still fails, but on the program being missing rather than on
            // consent — which is the distinction being asserted.
            assert!(matches!(
                ServerProcess::spawn(&config, Consent::NotAsked),
                Err(SpawnError::NotRunnable { .. })
            ));
        }
    }

    #[test]
    fn a_missing_program_says_which_one() {
        // deco cannot install a language server, so the message has to be
        // plain enough to act on.
        let Err(error) = ServerProcess::spawn(&config(Trust::User), Consent::Granted) else {
            panic!("a nonexistent program cannot start");
        };
        let text = error.to_string();
        assert!(
            text.contains("definitely-not-a-real-program-cbf3a1"),
            "{text}"
        );
    }

    #[test]
    fn consent_is_a_type_rather_than_a_bool() {
        // Guards the property the type exists for: the two states are not
        // interchangeable with `true`/`false` at a call site.
        assert_ne!(Consent::Granted, Consent::NotAsked);
    }
}
