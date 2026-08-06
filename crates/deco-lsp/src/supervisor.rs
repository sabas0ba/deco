//! One language server, driven end to end.
//!
//! [`Client`] knows the protocol, [`ServerProcess`] moves the bytes, and
//! [`DocumentSync`] and [`DiagnosticStore`] hold the state. This is the piece
//! that joins them, so a frontend can write
//!
//! ```text
//! supervisor.did_open(path, "rust", text)?;
//! for update in supervisor.poll() { … }
//! ```
//!
//! and not think about ids, versions, framing or lifecycle ordering.
//!
//! # It never blocks the editor
//!
//! [`Supervisor::poll`] drains whatever has arrived and returns. Starting a
//! server is the one exception — the protocol forbids sending anything before
//! the `initialize` reply — and even that is bounded by
//! [`Supervisor::start`]'s timeout, because a server that never answers must
//! not be able to hang the editor at launch.
//!
//! # A server that misbehaves costs itself
//!
//! Every failure path here degrades to "this server is not running" and leaves
//! the editor working: a crash during startup, a protocol error mid-session, a
//! server that exits on its own. The stderr tail is attached to the report,
//! since it is usually the only explanation available.

use std::path::Path;
use std::time::Duration;

use crate::client::{Client, ClientEvent, LspError, Outgoing, State};
use crate::diagnostics::{Diagnostic, DiagnosticStore, Published};
use crate::jsonrpc::{Message, ProtocolError};
use crate::process::{Consent, ReaderEvent, ServerProcess, SpawnError, EXIT_GRACE};
use crate::server::ServerConfig;
use crate::sync::{ContentChange, DocumentSync, SyncError};
use crate::uri::{PathStyle, Uri};

/// How long to wait for `initialize` to be answered.
///
/// Generous, because a server may be reading a large project's metadata before
/// it replies — but finite, because the alternative is an editor that hangs at
/// startup when a server is broken.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);

/// Something the editor should react to.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The server finished starting and is ready for requests.
    Ready {
        /// Which server.
        id: String,
    },
    /// Diagnostics for a document changed. The complete new set is included, so
    /// the caller replaces rather than merges.
    Diagnostics {
        /// Which document.
        uri: Uri,
        /// Everything now known about it.
        diagnostics: Vec<Diagnostic>,
    },
    /// The server asked for something to be shown to the user.
    Message {
        /// 1 error, 2 warning, 3 info, 4 log.
        kind: i64,
        /// The text.
        message: String,
    },
    /// The server is gone. No further updates will arrive.
    Stopped {
        /// Which server.
        id: String,
        /// Why, in a form fit to show a user.
        reason: String,
    },
    /// Something was ignored. Interesting only in a log.
    Noted {
        /// What happened.
        detail: String,
    },
}

/// Why a supervisor call failed.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// The server could not be started.
    #[error(transparent)]
    Spawn(#[from] SpawnError),
    /// The protocol state machine refused the call.
    #[error(transparent)]
    Protocol(#[from] LspError),
    /// A document synchronisation rule was broken.
    #[error(transparent)]
    Sync(#[from] SyncError),
    /// Writing to the server failed.
    #[error("could not write to `{id}`: {source}")]
    Write {
        /// Which server.
        id: String,
        /// Why.
        source: ProtocolError,
    },
    /// The server did not answer `initialize` in time.
    #[error("`{id}` did not answer initialize within {}s: {stderr}", timeout.as_secs())]
    StartupTimeout {
        /// Which server.
        id: String,
        /// How long was allowed.
        timeout: Duration,
        /// What it wrote to stderr, which is usually the reason.
        stderr: String,
    },
    /// The server exited or failed during startup.
    #[error("`{id}` failed to start: {reason}")]
    StartupFailed {
        /// Which server.
        id: String,
        /// Why, including the stderr tail.
        reason: String,
    },
    /// The server is not running.
    #[error("`{id}` is not running")]
    NotRunning {
        /// Which server.
        id: String,
    },
}

/// Waits briefly for a dying server's last words, then renders them.
///
/// The stderr pump runs on its own thread, so at the moment a write fails the
/// explanation may not have been collected yet. Waiting for the process to exit
/// is the reliable signal that there is nothing more coming — and it is bounded,
/// because a server that is broken in some other way must not stall startup.
fn drain_stderr(process: &mut ServerProcess, grace: Duration) -> String {
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if process.exited().is_some() && !process.stderr_tail().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    process.stderr_tail().summary()
}

/// One language server and everything the editor knows about its state.
#[derive(Debug)]
pub struct Supervisor {
    id: String,
    client: Client,
    process: Option<ServerProcess>,
    sync: DocumentSync,
    diagnostics: DiagnosticStore,
    style: PathStyle,
    /// Set once the server is gone, so a later call reports the original reason
    /// rather than a bare "not running".
    stopped: Option<String>,
    /// Updates produced while handling the current message.
    ///
    /// A side channel because `dispatch` has to return what must be written
    /// back to the server, and threading two collections through every call
    /// site made the state changes harder to follow than this does.
    pending_updates: Vec<Update>,
}

impl Supervisor {
    /// Starts a server and completes the handshake.
    ///
    /// Blocks until the server answers `initialize` or the timeout expires —
    /// the only place this crate blocks, and unavoidable: the protocol forbids
    /// sending anything else first.
    pub fn start(
        config: &ServerConfig,
        consent: Consent,
        root: Option<&Path>,
        style: PathStyle,
        timeout: Duration,
    ) -> Result<Self, SupervisorError> {
        let mut process = ServerProcess::spawn(config, consent)?;
        let mut client = Client::new();

        let root_uri = root.and_then(|path| Uri::from_path(path, style).ok());
        let Outgoing(message) =
            client.initialize(root_uri.as_ref(), config.initialization_options.clone())?;

        // A write failure here is almost always a server that exited before it
        // read anything — a missing runtime, a bad argument, a licence check.
        // Reporting the raw "broken pipe" would hide the one thing that
        // explains it, so the process is given a moment to finish dying and its
        // stderr is attached instead.
        if let Err(source) = process.send(&message) {
            let stderr = drain_stderr(&mut process, Duration::from_millis(500));
            return Err(SupervisorError::StartupFailed {
                id: config.id.clone(),
                reason: format!("{source}\n{stderr}"),
            });
        }

        let mut supervisor = Self {
            id: config.id.clone(),
            client,
            process: Some(process),
            sync: DocumentSync::new(),
            diagnostics: DiagnosticStore::new(),
            style,
            stopped: None,
            pending_updates: Vec::new(),
        };

        supervisor.await_ready(timeout)?;
        Ok(supervisor)
    }

    /// Pumps messages until the handshake completes, fails or times out.
    fn await_ready(&mut self, timeout: Duration) -> Result<(), SupervisorError> {
        let deadline = std::time::Instant::now() + timeout;

        while self.client.state() != State::Ready {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(SupervisorError::StartupTimeout {
                    id: self.id.clone(),
                    timeout,
                    stderr: self.stderr_summary(),
                });
            }

            let Some(process) = self.process.as_ref() else {
                return Err(self.startup_failed("the server stopped"));
            };
            let Some(event) = process.recv_timeout(remaining) else {
                continue;
            };

            match event {
                ReaderEvent::Message(message) => {
                    // A refused handshake leaves the client in `Exited`, which
                    // this loop would otherwise spin on until the deadline.
                    let outgoing = self.dispatch(message)?;
                    if self.client.state() == State::Exited {
                        return Err(self.startup_failed("the server refused to initialize"));
                    }
                    self.send_all(outgoing)?;
                }
                ReaderEvent::Closed => {
                    return Err(self.startup_failed("the server exited during startup"))
                }
                ReaderEvent::Failed(reason) => return Err(self.startup_failed(&reason)),
            }
        }
        Ok(())
    }

    fn startup_failed(&self, reason: &str) -> SupervisorError {
        SupervisorError::StartupFailed {
            id: self.id.clone(),
            reason: format!("{reason}\n{}", self.stderr_summary()),
        }
    }

    fn stderr_summary(&self) -> String {
        match &self.process {
            Some(process) => process.stderr_tail().summary(),
            None => "the server is gone".to_owned(),
        }
    }

    /// Which server this is.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the server is running and past its handshake.
    pub fn is_ready(&self) -> bool {
        self.process.is_some() && self.client.state() == State::Ready
    }

    /// What the server said it can do.
    pub fn capabilities(&self) -> &crate::capabilities::ServerCapabilities {
        self.client.capabilities()
    }

    /// Diagnostics currently known for a document.
    pub fn diagnostics(&self, uri: &Uri) -> &[Diagnostic] {
        self.diagnostics.for_uri(uri)
    }

    /// The URI a path maps to under this server's path style.
    pub fn uri_for(&self, path: &Path) -> Option<Uri> {
        Uri::from_path(path, self.style).ok()
    }

    /// Tells the server a document is open.
    ///
    /// A no-op returning `Ok` when the server said it does not want open and
    /// close notifications — the caller should not have to check.
    pub fn did_open(
        &mut self,
        path: &Path,
        language_id: &str,
        text: &str,
    ) -> Result<(), SupervisorError> {
        let Some(uri) = self.uri_for(path) else {
            return Ok(());
        };
        if !self.client.capabilities().open_close {
            return Ok(());
        }
        let params = self.sync.open(uri, language_id, text)?;
        self.notify("textDocument/didOpen", params)
    }

    /// Tells the server a document changed.
    ///
    /// `changes` is used only when the server negotiated incremental sync;
    /// `text` is the whole document, used for a full sync. Passing both lets the
    /// caller stay ignorant of which was negotiated.
    pub fn did_change(
        &mut self,
        path: &Path,
        changes: &[ContentChange],
        text: &str,
    ) -> Result<(), SupervisorError> {
        let Some(uri) = self.uri_for(path) else {
            return Ok(());
        };
        if !self.sync.is_open(&uri) {
            // Not an error: a document the server was never told about (because
            // it does not want open notifications, or is not this server's
            // language) has nothing to change.
            return Ok(());
        }
        let kind = self.client.capabilities().sync_kind;
        let Some(params) = self.sync.change(&uri, kind, changes, text)? else {
            return Ok(());
        };
        self.notify("textDocument/didChange", params)
    }

    /// Tells the server a document was saved.
    pub fn did_save(&mut self, path: &Path, text: &str) -> Result<(), SupervisorError> {
        let Some(uri) = self.uri_for(path) else {
            return Ok(());
        };
        let Some(options) = self.client.capabilities().save else {
            return Ok(());
        };
        if !self.sync.is_open(&uri) {
            return Ok(());
        }
        let params = self.sync.save(&uri, options.include_text, text)?;
        self.notify("textDocument/didSave", params)
    }

    /// Tells the server a document is closed.
    ///
    /// Its diagnostics go with it: nothing will ever retract them once the
    /// server has stopped tracking the file.
    pub fn did_close(&mut self, path: &Path) -> Result<(), SupervisorError> {
        let Some(uri) = self.uri_for(path) else {
            return Ok(());
        };
        if !self.sync.is_open(&uri) {
            return Ok(());
        }
        let params = self.sync.close(&uri)?;
        self.diagnostics.clear(&uri);
        self.notify("textDocument/didClose", params)
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), SupervisorError> {
        let Outgoing(message) = self.client.notify(method, params)?;
        self.write(&message)
    }

    fn write(&mut self, message: &Message) -> Result<(), SupervisorError> {
        let id = self.id.clone();
        let process = self
            .process
            .as_mut()
            .ok_or(SupervisorError::NotRunning { id: id.clone() })?;
        process
            .send(message)
            .map_err(|source| SupervisorError::Write { id, source })
    }

    fn send_all(&mut self, outgoing: Vec<Outgoing>) -> Result<(), SupervisorError> {
        for Outgoing(message) in outgoing {
            self.write(&message)?;
        }
        Ok(())
    }

    /// The next thing the reader thread has for us, if any. Never blocks.
    ///
    /// `None` covers both "nothing yet" and "no process", which are the same
    /// thing to a caller draining a queue.
    fn next_event(&self) -> Option<ReaderEvent> {
        self.process.as_ref()?.try_recv()
    }

    /// Drains whatever has arrived from the server. Never blocks.
    ///
    /// A write failure while answering the server is reported as an update
    /// rather than returned: the caller asked for news, and failing the whole
    /// call would discard the updates already collected.
    pub fn poll(&mut self) -> Vec<Update> {
        let mut updates = Vec::new();

        // The borrow of `self.process` has to end before each iteration's body,
        // which calls `&mut self` methods — hence a helper rather than reaching
        // into the field inside the loop head.
        while let Some(event) = self.next_event() {
            match event {
                ReaderEvent::Message(message) => match self.dispatch(message) {
                    Ok(outgoing) => {
                        updates.extend(std::mem::take(&mut self.pending_updates));
                        if let Err(error) = self.send_all(outgoing) {
                            updates.push(self.stop_with(error.to_string()));
                            break;
                        }
                    }
                    Err(error) => {
                        updates.push(self.stop_with(error.to_string()));
                        break;
                    }
                },
                ReaderEvent::Closed => {
                    updates.push(self.stop_with("the server exited".to_owned()));
                    break;
                }
                ReaderEvent::Failed(reason) => {
                    updates.push(self.stop_with(reason));
                    break;
                }
            }
        }

        // A server can also die without closing its pipes in an order the
        // reader notices, so the exit status is checked independently.
        if let Some(process) = self.process.as_mut() {
            if let Some(status) = process.exited() {
                updates.push(self.stop_with(format!("the server exited with {status}")));
            }
        }

        updates
    }

    /// Marks the server gone and produces the update saying so.
    fn stop_with(&mut self, reason: String) -> Update {
        let detail = match &self.process {
            Some(process) => {
                let tail = process.stderr_tail();
                if tail.is_empty() {
                    reason.clone()
                } else {
                    format!("{reason}\n{}", tail.summary())
                }
            }
            None => reason.clone(),
        };
        // Dropping the process joins its threads and reaps it, so a stopped
        // server leaves nothing behind.
        self.process = None;
        self.diagnostics.clear_all();
        self.stopped = Some(detail.clone());
        Update::Stopped {
            id: self.id.clone(),
            reason: detail,
        }
    }

    /// Asks the server to stop, then waits for it.
    ///
    /// `shutdown`, then `exit`, then a bounded wait, then a kill — see
    /// [`ServerProcess::stop`]. Errors are swallowed deliberately: this runs
    /// while the editor is quitting, and there is nothing useful to do with a
    /// failure to shut down a process that is about to be killed anyway.
    pub fn stop(&mut self) {
        if let Ok(Outgoing(message)) = self.client.shutdown() {
            let _ = self.write(&message);
        }
        if let Ok(Outgoing(message)) = self.client.exit() {
            let _ = self.write(&message);
        }
        if let Some(mut process) = self.process.take() {
            process.stop(EXIT_GRACE);
        }
        self.diagnostics.clear_all();
    }
}

// Updates produced while handling one message, collected out of band because
// `dispatch` also has to return what must be written back.
impl Supervisor {
    fn dispatch(&mut self, message: Message) -> Result<Vec<Outgoing>, SupervisorError> {
        let (outgoing, events) = self.client.handle(message)?;
        self.pending_updates = events
            .into_iter()
            .filter_map(|event| self.absorb(event))
            .collect();
        Ok(outgoing)
    }

    /// Turns a protocol event into an editor-facing update, applying any state
    /// change it implies. `None` means nothing needs reporting.
    fn absorb(&mut self, event: ClientEvent) -> Option<Update> {
        match event {
            ClientEvent::Initialized { .. } => Some(Update::Ready {
                id: self.id.clone(),
            }),
            ClientEvent::Notification(notification)
                if notification.method == "textDocument/publishDiagnostics" =>
            {
                self.absorb_diagnostics(notification.params.unwrap_or_default())
            }
            ClientEvent::Notification(notification) => Some(Update::Noted {
                detail: format!("unhandled notification {}", notification.method),
            }),
            ClientEvent::ShowMessage { kind, message } => Some(Update::Message { kind, message }),
            ClientEvent::LogMessage { kind, message } => Some(Update::Noted {
                detail: format!("server log ({kind}): {message}"),
            }),
            ClientEvent::Response { method, error, .. } => {
                // Responses to requests deco does not raise yet. Expected
                // failures — cancellation, content-modified — are not worth a
                // line; they happen constantly during ordinary typing.
                let error = error?;
                if crate::jsonrpc::ErrorCode::from_code(error.code)
                    .is_some_and(|code| code.is_expected())
                {
                    return None;
                }
                Some(Update::Noted {
                    detail: format!("{method} failed: {error}"),
                })
            }
            ClientEvent::Ignored { reason } => Some(Update::Noted { detail: reason }),
        }
    }

    fn absorb_diagnostics(&mut self, params: serde_json::Value) -> Option<Update> {
        let uri = Uri::from_string(params.get("uri")?.as_str()?);
        let version = params
            .get("version")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);
        let diagnostics: Vec<Diagnostic> = params
            .get("diagnostics")?
            .as_array()?
            .iter()
            .filter_map(Diagnostic::from_json)
            .collect();

        let current = self.sync.version(&uri);
        match self
            .diagnostics
            .publish(uri.clone(), version, diagnostics, current)
        {
            Published::Replaced { .. } => Some(Update::Diagnostics {
                diagnostics: self.diagnostics.for_uri(&uri).to_vec(),
                uri,
            }),
            // Not reported: the editor's current diagnostics are still correct,
            // and a message saying a stale result was dropped is noise during
            // ordinary typing.
            Published::Stale { published, current } => Some(Update::Noted {
                detail: format!(
                    "dropped diagnostics for {uri} computed against version \
                     {published} (document is at {current})"
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{ServerCapabilities, TextDocumentSyncKind};
    use crate::jsonrpc::{Notification, Response};
    use serde_json::json;

    /// A supervisor with no process behind it.
    ///
    /// Everything except `start`, `poll` and the write path is a pure function
    /// of the client, sync and diagnostic state, so most of the interesting
    /// behaviour can be exercised without a server. The process-owning parts
    /// are covered by the `process` module and by `tests/server_process.rs`.
    fn detached(capabilities: serde_json::Value) -> Supervisor {
        let mut client = Client::new();
        let Outgoing(Message::Request(init)) = client.initialize(None, None).unwrap() else {
            panic!("initialize is a request");
        };
        client
            .handle(Message::Response(Response::ok(
                init.id,
                json!({ "capabilities": capabilities }),
            )))
            .unwrap();

        Supervisor {
            id: "test".into(),
            client,
            process: None,
            sync: DocumentSync::new(),
            diagnostics: DiagnosticStore::new(),
            style: PathStyle::Unix,
            stopped: None,
            pending_updates: Vec::new(),
        }
    }

    fn publish(uri: &str, version: Option<i64>, ranges: &[(u32, &str)]) -> Message {
        let diagnostics: Vec<serde_json::Value> = ranges
            .iter()
            .map(|(line, message)| {
                json!({
                    "range": {
                        "start": {"line": line, "character": 0},
                        "end": {"line": line, "character": 4},
                    },
                    "severity": 1,
                    "message": message,
                })
            })
            .collect();
        let mut params = json!({ "uri": uri, "diagnostics": diagnostics });
        if let Some(version) = version {
            params["version"] = json!(version);
        }
        Message::Notification(Notification {
            method: "textDocument/publishDiagnostics".into(),
            params: Some(params),
        })
    }

    /// Feeds a message straight into the state machine, as `poll` would.
    fn feed(supervisor: &mut Supervisor, message: Message) -> Vec<Update> {
        supervisor.dispatch(message).expect("dispatch");
        std::mem::take(&mut supervisor.pending_updates)
    }

    #[test]
    fn diagnostics_reach_the_editor_as_a_complete_set() {
        let mut s = detached(json!({}));
        let updates = feed(&mut s, publish("file:///w/a.rs", None, &[(3, "boom")]));

        assert_eq!(updates.len(), 1);
        let Update::Diagnostics { uri, diagnostics } = &updates[0] else {
            panic!("expected diagnostics, got {updates:?}");
        };
        assert_eq!(uri.as_str(), "file:///w/a.rs");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "boom");
    }

    #[test]
    fn a_second_publication_replaces_the_first() {
        let mut s = detached(json!({}));
        feed(&mut s, publish("file:///w/a.rs", None, &[(1, "old")]));
        let updates = feed(&mut s, publish("file:///w/a.rs", None, &[(2, "new")]));

        let Update::Diagnostics { diagnostics, .. } = &updates[0] else {
            panic!("expected diagnostics");
        };
        assert_eq!(diagnostics.len(), 1, "replaced, not appended");
        assert_eq!(diagnostics[0].message, "new");
    }

    #[test]
    fn an_empty_publication_is_reported_so_the_editor_clears() {
        // This is how a server says the errors are fixed. Skipping it because
        // the list is empty leaves stale squiggles on screen forever.
        let mut s = detached(json!({}));
        feed(&mut s, publish("file:///w/a.rs", None, &[(1, "boom")]));
        let updates = feed(&mut s, publish("file:///w/a.rs", None, &[]));

        let Update::Diagnostics { diagnostics, .. } = &updates[0] else {
            panic!("an empty set must still be reported, got {updates:?}");
        };
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostics_for_an_older_version_are_dropped_not_applied() {
        let mut s = detached(json!({
            "textDocumentSync": {"openClose": true, "change": 1}
        }));
        let path = Path::new("/w/a.rs");
        s.sync.open(s.uri_for(path).unwrap(), "rust", "x").unwrap();
        // Two changes take the document to version 3.
        for _ in 0..2 {
            s.sync
                .change(
                    &s.uri_for(path).unwrap(),
                    TextDocumentSyncKind::Full,
                    &[],
                    "x",
                )
                .unwrap();
        }

        let updates = feed(&mut s, publish("file:///w/a.rs", Some(1), &[(9, "stale")]));
        assert!(
            matches!(&updates[0], Update::Noted { .. }),
            "a stale publication must not become a Diagnostics update: {updates:?}"
        );
        assert!(s.diagnostics(&s.uri_for(path).unwrap()).is_empty());
    }

    #[test]
    fn a_publication_without_a_version_is_trusted() {
        // A server that does not stamp versions offers nothing better to go on.
        let mut s = detached(json!({"textDocumentSync": 1}));
        let path = Path::new("/w/a.rs");
        s.sync.open(s.uri_for(path).unwrap(), "rust", "x").unwrap();
        let updates = feed(&mut s, publish("file:///w/a.rs", None, &[(0, "boom")]));
        assert!(matches!(&updates[0], Update::Diagnostics { .. }));
    }

    #[test]
    fn a_malformed_publication_is_ignored_rather_than_fatal() {
        let mut s = detached(json!({}));
        for params in [json!({}), json!({"uri": 42}), json!({"uri": "file:///a"})] {
            let updates = feed(
                &mut s,
                Message::Notification(Notification {
                    method: "textDocument/publishDiagnostics".into(),
                    params: Some(params.clone()),
                }),
            );
            assert!(
                updates.is_empty(),
                "{params} should produce nothing, got {updates:?}"
            );
        }
    }

    #[test]
    fn a_diagnostic_without_a_range_is_skipped_but_its_siblings_survive() {
        let mut s = detached(json!({}));
        let updates = feed(
            &mut s,
            Message::Notification(Notification {
                method: "textDocument/publishDiagnostics".into(),
                params: Some(json!({
                    "uri": "file:///w/a.rs",
                    "diagnostics": [
                        {"message": "no range"},
                        {
                            "range": {
                                "start": {"line": 1, "character": 0},
                                "end": {"line": 1, "character": 2},
                            },
                            "message": "placed",
                        },
                    ],
                })),
            }),
        );
        let Update::Diagnostics { diagnostics, .. } = &updates[0] else {
            panic!("expected diagnostics");
        };
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "placed");
    }

    #[test]
    fn a_show_message_is_surfaced_and_a_log_message_is_not() {
        let mut s = detached(json!({}));

        let updates = feed(
            &mut s,
            Message::Notification(Notification {
                method: "window/showMessage".into(),
                params: Some(json!({"type": 1, "message": "cargo not found"})),
            }),
        );
        assert_eq!(
            updates[0],
            Update::Message {
                kind: 1,
                message: "cargo not found".into()
            }
        );

        let updates = feed(
            &mut s,
            Message::Notification(Notification {
                method: "window/logMessage".into(),
                params: Some(json!({"type": 4, "message": "indexing"})),
            }),
        );
        assert!(
            matches!(&updates[0], Update::Noted { .. }),
            "a log line must not interrupt the user: {updates:?}"
        );
    }

    #[test]
    fn open_and_change_are_skipped_when_the_server_does_not_want_them() {
        // The caller should not have to check capabilities before every edit.
        let mut s = detached(json!({"textDocumentSync": {"openClose": false}}));
        let path = Path::new("/w/a.rs");

        // No process attached, so anything actually sent would fail the write.
        assert!(s.did_open(path, "rust", "x").is_ok());
        assert!(s.did_change(path, &[], "y").is_ok());
        assert!(s.did_save(path, "y").is_ok());
        assert!(s.did_close(path).is_ok());
        assert!(!s.sync.is_open(&s.uri_for(path).unwrap()));
    }

    #[test]
    fn changing_a_document_that_was_never_opened_is_not_an_error() {
        // It happens whenever a file's language is not this server's.
        let mut s = detached(json!({"textDocumentSync": 2}));
        assert!(s.did_change(Path::new("/w/other.py"), &[], "x").is_ok());
    }

    #[test]
    fn save_is_skipped_when_the_server_did_not_ask_for_it() {
        let mut s = detached(json!({"textDocumentSync": {"openClose": true, "change": 1}}));
        let path = Path::new("/w/a.rs");
        s.sync.open(s.uri_for(path).unwrap(), "rust", "x").unwrap();
        assert!(s.capabilities().save.is_none());
        // Would fail on the write if it tried to send.
        assert!(s.did_save(path, "x").is_ok());
    }

    #[test]
    fn closing_a_document_drops_its_diagnostics() {
        // Nothing will ever retract them once the server stops tracking it.
        let mut s = detached(json!({"textDocumentSync": {"openClose": true, "change": 1}}));
        let path = Path::new("/w/a.rs");
        let uri = s.uri_for(path).unwrap();
        s.sync.open(uri.clone(), "rust", "x").unwrap();
        feed(&mut s, publish(uri.as_str(), None, &[(0, "boom")]));
        assert_eq!(s.diagnostics(&uri).len(), 1);

        // The notify write fails with no process, but the local state must
        // already have been cleared by then.
        let _ = s.did_close(path);
        assert!(s.diagnostics(&uri).is_empty());
    }

    #[test]
    fn a_relative_path_is_skipped_rather_than_guessed_at() {
        // LSP has no working directory, so there is no correct URI to invent.
        let mut s = detached(json!({"textDocumentSync": 1}));
        assert!(s.did_open(Path::new("relative.rs"), "rust", "x").is_ok());
        assert_eq!(s.sync.len(), 0);
    }

    #[test]
    fn stopping_reports_the_stderr_tail() {
        // When a server dies, its last words are the only explanation there is.
        let mut s = detached(json!({}));
        let update = s.stop_with("the server exited".into());
        let Update::Stopped { reason, .. } = &update else {
            panic!("expected Stopped");
        };
        assert!(reason.contains("the server exited"), "{reason}");
        assert!(!s.is_ready());
    }

    #[test]
    fn a_stopped_server_drops_its_diagnostics() {
        let mut s = detached(json!({}));
        feed(&mut s, publish("file:///w/a.rs", None, &[(0, "boom")]));
        s.stop_with("gone".into());
        assert!(s
            .diagnostics(&Uri::from_string("file:///w/a.rs"))
            .is_empty());
    }

    #[test]
    fn polling_a_stopped_server_yields_nothing_and_does_not_panic() {
        let mut s = detached(json!({}));
        s.stop_with("gone".into());
        assert!(s.poll().is_empty());
        assert!(s.poll().is_empty());
    }

    #[test]
    fn writing_to_a_stopped_server_is_a_named_error() {
        let mut s = detached(json!({"textDocumentSync": {"openClose": true, "change": 1}}));
        assert!(matches!(
            s.did_open(Path::new("/w/a.rs"), "rust", "x"),
            Err(SupervisorError::NotRunning { .. })
        ));
    }

    #[test]
    fn an_expected_request_failure_is_not_reported() {
        // Cancellation and content-modified arrive constantly during typing.
        let mut s = detached(json!({}));
        let (id, _) = s.client.request("textDocument/hover", json!({})).unwrap();
        let updates = feed(
            &mut s,
            Message::Response(Response::err(
                id,
                crate::jsonrpc::ErrorCode::ContentModified,
                "changed",
            )),
        );
        assert!(updates.is_empty(), "{updates:?}");
    }

    #[test]
    fn a_real_request_failure_is_noted() {
        let mut s = detached(json!({}));
        let (id, _) = s.client.request("textDocument/hover", json!({})).unwrap();
        let updates = feed(
            &mut s,
            Message::Response(Response::err(
                id,
                crate::jsonrpc::ErrorCode::InternalError,
                "panicked",
            )),
        );
        assert!(matches!(&updates[0], Update::Noted { .. }), "{updates:?}");
    }

    #[test]
    fn capabilities_are_readable_after_the_handshake() {
        let s = detached(json!({"hoverProvider": true, "textDocumentSync": 2}));
        assert!(s.capabilities().hover);
        assert_eq!(
            s.capabilities().sync_kind,
            TextDocumentSyncKind::Incremental
        );
        assert_ne!(s.capabilities(), &ServerCapabilities::default());
    }

    #[test]
    fn a_full_sync_server_gets_the_whole_text_and_an_incremental_one_gets_ranges() {
        // Both are driven through the same `did_change` call; only the
        // negotiated kind decides what goes on the wire. Asserted through the
        // sync layer, since there is no process to write to here.
        for (capabilities, expect_range) in [
            (
                json!({"textDocumentSync": {"openClose": true, "change": 1}}),
                false,
            ),
            (
                json!({"textDocumentSync": {"openClose": true, "change": 2}}),
                true,
            ),
        ] {
            let mut s = detached(capabilities);
            let path = Path::new("/w/a.rs");
            let uri = s.uri_for(path).unwrap();
            s.sync.open(uri.clone(), "rust", "old").unwrap();

            let changes = [ContentChange::Incremental {
                range: deco_core::position::Range::new(
                    deco_core::position::Position::new(0, 0),
                    deco_core::position::Position::new(0, 3),
                ),
                text: "new".into(),
            }];
            let params = s
                .sync
                .change(&uri, s.client.capabilities().sync_kind, &changes, "new")
                .unwrap()
                .unwrap();
            let first = &params["contentChanges"][0];
            assert_eq!(first.get("range").is_some(), expect_range, "{params}");
        }
    }
}
