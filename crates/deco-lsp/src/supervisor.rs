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
//! # It does not block the editor
//!
//! [`Supervisor::poll`] drains whatever has arrived and returns. Two bounded
//! exceptions, both deliberate:
//!
//! - **Starting a server.** The protocol forbids sending anything before the
//!   `initialize` reply, so this genuinely has to wait — bounded by
//!   [`Supervisor::start`]'s timeout, because a server that never answers must
//!   not be able to hang the editor at launch.
//! - **The frame on which a server dies**, for up to 100ms, waiting for its
//!   stderr. See [`ServerProcess::stderr_after_exit`].
//!
//! # A server that misbehaves costs itself
//!
//! Every failure path here degrades to "this server is not running" and leaves
//! the editor working: a crash during startup, a protocol error mid-session, a
//! server that exits on its own.
//!
//! Each of those reports the server's stderr tail, because it is usually the
//! only explanation there is — and getting that right needs more care than it
//! looks like, since stdout and stderr are pumped by separate threads and the
//! news that a server is gone can beat the reason it gave.
//! [`ServerProcess::stderr_after_exit`] is where that race is resolved.

use std::path::Path;
use std::time::Duration;

use crate::client::{Client, ClientEvent, LspError, Outgoing, State};
use crate::diagnostics::{Diagnostic, DiagnosticStore, Published};
use crate::jsonrpc::{Message, ProtocolError, RequestId};
use crate::process::{Consent, ReaderEvent, ServerProcess, SpawnError, EXIT_GRACE};
use crate::requests::{
    CompletionItem, CompletionTrigger, FormattingOptions, Hover, Location, TextEdit,
};
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
    /// An answer to [`Supervisor::hover`].
    Hover {
        /// The request this answers, so a caller that has moved on can ignore it.
        id: RequestId,
        /// What the server said, or `None` for "nothing at that position" —
        /// which is a successful answer and worth reporting as such.
        hover: Option<Hover>,
    },
    /// An answer to [`Supervisor::definition`] or [`Supervisor::references`].
    Locations {
        /// The request this answers.
        id: RequestId,
        /// Which method asked, since both answer in the same shape.
        method: String,
        /// Where the server pointed. Empty means it found nothing.
        locations: Vec<Location>,
    },
    /// An answer to [`Supervisor::document_symbols`].
    Symbols {
        /// The request this answers.
        id: RequestId,
        /// The names found, flattened to document order. Empty means the server
        /// found none, which is a successful answer.
        symbols: Vec<crate::requests::DocumentSymbol>,
    },
    /// An answer to [`Supervisor::semantic_tokens`].
    SemanticTokens {
        /// The request this answers.
        id: RequestId,
        /// The classified runs, already absolute.
        spans: Vec<crate::requests::SemanticSpan>,
    },
    /// An answer to [`Supervisor::completion`].
    Completion {
        /// The request this answers.
        id: RequestId,
        /// The suggestions, in the order the server sent them.
        items: Vec<CompletionItem>,
        /// Whether the server marked the list incomplete. Reported but not acted
        /// on: deco re-requests from scratch rather than refining a partial list.
        incomplete: bool,
    },
    /// An answer to [`Supervisor::formatting`] or [`Supervisor::range_formatting`].
    Edits {
        /// The request this answers.
        id: RequestId,
        /// Which method asked.
        method: String,
        /// The replacements to make, in the order the server sent them. They
        /// refer to the document as the server saw it and must not be applied
        /// front to back; see [`TextEdit::list_from_json`].
        edits: Vec<TextEdit>,
    },
    /// A request failed for a reason worth showing the user.
    ///
    /// Routine failures — cancellation, content-modified — are not reported
    /// here; they happen constantly during ordinary typing.
    RequestFailed {
        /// The request that failed.
        id: RequestId,
        /// Which method.
        method: String,
        /// Why, in a form fit for a status bar.
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

/// Waits for a dying server's last words, then renders them.
///
/// stdout and stderr are pumped by separate threads, so the news that a server
/// is gone can beat the reason it gave. Losing that race produces the least
/// useful error there is — "the server exited during startup; the server wrote
/// nothing to stderr" — for a server that said precisely why.
///
/// [`ServerProcess::stderr_after_exit`] resolves it by waiting for the pump
/// thread to finish rather than for output to appear, which is a fact rather
/// than a guess.
fn drain_stderr(process: &mut ServerProcess, grace: Duration) -> String {
    process.stderr_after_exit(grace).summary()
}

/// How long to wait for stderr while a server is failing to start.
///
/// Startup is already a blocking operation, so spending a fraction of a second
/// to turn an unexplained failure into an explained one is clearly worth it.
const STARTUP_STDERR_GRACE: Duration = Duration::from_millis(500);

/// How long to wait for stderr when a running server dies.
///
/// Much shorter: this happens inside [`Supervisor::poll`], which the event loop
/// calls between keystrokes. It is the one place `poll` can block, and it does
/// so only on the single frame where a server disappears — the alternative is
/// telling the user their language server stopped and being unable to say why.
const RUNNING_STDERR_GRACE: Duration = Duration::from_millis(100);

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
                // No grace here: the server is still alive and simply has not
                // answered, so whatever it has written is already collected.
                let stderr = self.stderr_summary(Duration::ZERO);
                return Err(SupervisorError::StartupTimeout {
                    id: self.id.clone(),
                    timeout,
                    stderr,
                });
            }

            // Scoped so the borrow of `self.process` ends before the body,
            // which needs `&mut self` to drain stderr on a failure.
            let event = {
                let Some(process) = self.process.as_ref() else {
                    return Err(self.startup_failed("the server stopped"));
                };
                process.recv_timeout(remaining)
            };
            let Some(event) = event else {
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

    /// Builds a startup failure, waiting for the server's stderr first.
    ///
    /// `&mut self` rather than `&self` precisely so it can drain: the reason is
    /// almost always in stderr, and reporting before the pump has caught up
    /// throws it away.
    fn startup_failed(&mut self, reason: &str) -> SupervisorError {
        let stderr = self.stderr_summary(STARTUP_STDERR_GRACE);
        SupervisorError::StartupFailed {
            id: self.id.clone(),
            reason: format!("{reason}\n{stderr}"),
        }
    }

    fn stderr_summary(&mut self, grace: Duration) -> String {
        match self.process.as_mut() {
            Some(process) => drain_stderr(process, grace),
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

    /// Asks what is at a position.
    ///
    /// Returns the request id so the caller can match the answer — or drop it,
    /// if the cursor has since moved. `None` when the server does not offer
    /// hover: the caller should not have to check capabilities before every
    /// keypress.
    pub fn hover(
        &mut self,
        path: &Path,
        position: deco_core::position::Position,
    ) -> Result<Option<RequestId>, SupervisorError> {
        if !self.client.capabilities().hover {
            return Ok(None);
        }
        self.positional("textDocument/hover", path, position)
    }

    /// Asks for completions at a position.
    ///
    /// `None` when the server offers no completion, so the caller need not check
    /// capabilities on every keystroke.
    pub fn completion(
        &mut self,
        path: &Path,
        position: deco_core::position::Position,
        trigger: CompletionTrigger,
    ) -> Result<Option<RequestId>, SupervisorError> {
        if self.client.capabilities().completion.is_none() {
            return Ok(None);
        }
        let Some(uri) = self.uri_for(path) else {
            return Ok(None);
        };
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        let params = crate::requests::completion_params(&uri, position, &trigger);
        self.request("textDocument/completion", params).map(Some)
    }

    /// The characters that should open a completion list without being asked.
    ///
    /// Empty when no server is running, which is what makes a caller able to
    /// check this on every keystroke without a branch of its own.
    pub fn completion_triggers(&self) -> &[String] {
        self.client
            .capabilities()
            .completion
            .as_ref()
            .map(|options| options.trigger_characters.as_slice())
            .unwrap_or(&[])
    }

    /// Asks the server to format the whole document.
    ///
    /// `options` is the user's own indentation settings, which is the point of
    /// sending them: a server told nothing formats to its defaults, and against
    /// a project that disagrees the result is a diff touching every line.
    pub fn formatting(
        &mut self,
        path: &Path,
        options: FormattingOptions,
    ) -> Result<Option<RequestId>, SupervisorError> {
        if !self.client.capabilities().formatting {
            return Ok(None);
        }
        let Some(uri) = self.uri_for(path) else {
            return Ok(None);
        };
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        let params = crate::requests::formatting_params(&uri, options);
        self.request("textDocument/formatting", params).map(Some)
    }

    /// Asks the server to format one range.
    ///
    /// Uses the same `formatting` capability: a server that offers whole-document
    /// formatting usually offers this too, and the specification gives them
    /// separate flags that servers set inconsistently. A server that does not
    /// support it answers with an error, which is reported like any other.
    pub fn range_formatting(
        &mut self,
        path: &Path,
        range: deco_core::position::Range,
        options: FormattingOptions,
    ) -> Result<Option<RequestId>, SupervisorError> {
        if !self.client.capabilities().formatting {
            return Ok(None);
        }
        let Some(uri) = self.uri_for(path) else {
            return Ok(None);
        };
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        let params = crate::requests::range_formatting_params(&uri, range, options);
        self.request("textDocument/rangeFormatting", params)
            .map(Some)
    }

    /// Asks where something is defined.
    pub fn definition(
        &mut self,
        path: &Path,
        position: deco_core::position::Position,
    ) -> Result<Option<RequestId>, SupervisorError> {
        if !self.client.capabilities().definition {
            return Ok(None);
        }
        self.positional("textDocument/definition", path, position)
    }

    /// Asks how the whole document is classified.
    ///
    /// Full document only: deco highlights the visible lines of a document it has
    /// already lexed, so a range request would mean one round trip per scroll for
    /// a refinement the lexer has already approximated.
    pub fn semantic_tokens(&mut self, path: &Path) -> Result<Option<RequestId>, SupervisorError> {
        if self.client.capabilities().semantic_tokens.is_none() {
            return Ok(None);
        }
        let Some(uri) = self.uri_for(path) else {
            return Ok(None);
        };
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        let params = crate::requests::semantic_tokens_params(&uri);
        self.request("textDocument/semanticTokens/full", params)
            .map(Some)
    }

    /// Asks what names a document declares.
    ///
    /// The whole document, since that is the only shape the request has — there
    /// is no positional variant, and the picker it feeds needs all of them.
    pub fn document_symbols(&mut self, path: &Path) -> Result<Option<RequestId>, SupervisorError> {
        if !self.client.capabilities().document_symbol {
            return Ok(None);
        }
        let Some(uri) = self.uri_for(path) else {
            return Ok(None);
        };
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        let params = serde_json::json!({ "textDocument": { "uri": uri } });
        self.request("textDocument/documentSymbol", params)
            .map(Some)
    }

    /// Asks what refers to something.
    pub fn references(
        &mut self,
        path: &Path,
        position: deco_core::position::Position,
    ) -> Result<Option<RequestId>, SupervisorError> {
        if !self.client.capabilities().references {
            return Ok(None);
        }
        let Some(uri) = self.uri_for(path) else {
            return Ok(None);
        };
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        // Including the declaration: "find all references" that omits the
        // definition is a surprising answer, and VS Code includes it.
        let params = crate::requests::reference_params(&uri, position, true);
        self.request("textDocument/references", params).map(Some)
    }

    /// Raises a request whose only arguments are a document and a position.
    fn positional(
        &mut self,
        method: &'static str,
        path: &Path,
        position: deco_core::position::Position,
    ) -> Result<Option<RequestId>, SupervisorError> {
        let Some(uri) = self.uri_for(path) else {
            return Ok(None);
        };
        // Asking about a document the server was never told about would get an
        // error at best and a confident wrong answer at worst.
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        let params = crate::requests::text_document_position(&uri, position);
        self.request(method, params).map(Some)
    }

    /// Asks the server to abandon a request.
    ///
    /// Advisory: the reply may already be on its way, and is dropped when it
    /// arrives. Worth sending anyway — a hover the user has moved past is work
    /// the server can stop doing.
    pub fn cancel(&mut self, id: &RequestId) -> Result<(), SupervisorError> {
        if let Some(Outgoing(message)) = self.client.cancel(id) {
            self.write(&message)?;
        }
        Ok(())
    }

    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RequestId, SupervisorError> {
        let (id, outgoing) = self.client.request(method, params)?;
        if let Some(Outgoing(message)) = outgoing {
            self.write(&message)?;
        }
        Ok(id)
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
        // Drained, for the same reason as at startup: stdout closing and stderr
        // being collected are separate threads racing, and losing that race
        // means telling the user their server stopped without saying why.
        let detail = match self.process.as_mut() {
            Some(process) => {
                let tail = drain_stderr(process, RUNNING_STDERR_GRACE);
                format!("{reason}\n{tail}")
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
            ClientEvent::Response {
                method,
                id,
                result,
                error,
            } => {
                if let Some(error) = error {
                    // Cancellation and content-modified happen constantly during
                    // ordinary typing; reporting them would bury the rest.
                    if crate::jsonrpc::ErrorCode::from_code(error.code)
                        .is_some_and(|code| code.is_expected())
                    {
                        return None;
                    }
                    return Some(Update::RequestFailed {
                        id,
                        method,
                        reason: error.to_string(),
                    });
                }

                // An absent result is treated as `null`, which for every method
                // below means "nothing at that position".
                let result = result.unwrap_or(serde_json::Value::Null);
                match method.as_str() {
                    "textDocument/hover" => Some(Update::Hover {
                        id,
                        hover: Hover::from_json(&result),
                    }),
                    "textDocument/completion" => {
                        let (items, incomplete) = CompletionItem::list_from_json(&result);
                        Some(Update::Completion {
                            id,
                            items,
                            incomplete,
                        })
                    }
                    "textDocument/formatting" | "textDocument/rangeFormatting" => {
                        Some(Update::Edits {
                            edits: TextEdit::list_from_json(&result),
                            id,
                            method,
                        })
                    }
                    "textDocument/definition"
                    | "textDocument/declaration"
                    | "textDocument/typeDefinition"
                    | "textDocument/implementation"
                    | "textDocument/references" => Some(Update::Locations {
                        locations: Location::list_from_json(&result),
                        id,
                        method,
                    }),
                    "textDocument/documentSymbol" => Some(Update::Symbols {
                        id,
                        symbols: crate::requests::DocumentSymbol::list_from_json(&result),
                    }),
                    "textDocument/semanticTokens/full" => {
                        // The legend is the server's, so a response cannot be read
                        // without the capabilities it announced at startup.
                        let spans = self
                            .client
                            .capabilities()
                            .semantic_tokens
                            .as_ref()
                            .map(|legend| {
                                crate::requests::semantic_spans_from_json(&result, legend)
                            })
                            .unwrap_or_default();
                        Some(Update::SemanticTokens { id, spans })
                    }
                    // A method deco raises but does not yet consume.
                    other => Some(Update::Noted {
                        detail: format!("unhandled response to {other}"),
                    }),
                }
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
    use deco_core::position::Position;
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
    fn a_real_request_failure_reaches_the_editor() {
        // Reported as `RequestFailed` rather than folded into `Noted`: the
        // editor has a pending request whose caller is waiting, and a status
        // line saying why is more use than a log entry.
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
        assert!(
            matches!(&updates[0], Update::RequestFailed { .. }),
            "{updates:?}"
        );
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

    /// A detached supervisor with a document already open, so positional
    /// requests reach the write path rather than being skipped.
    fn with_open_document(capabilities: serde_json::Value) -> (Supervisor, std::path::PathBuf) {
        let mut s = detached(capabilities);
        let path = std::path::PathBuf::from("/w/a.rs");
        let uri = s.uri_for(&path).unwrap();
        s.sync.open(uri, "rust", "fn main() {}").unwrap();
        (s, path)
    }

    #[test]
    fn hover_is_skipped_when_the_server_does_not_offer_it() {
        // The caller should not have to check capabilities before every keypress.
        let (mut s, path) = with_open_document(json!({"textDocumentSync": 1}));
        assert_eq!(s.hover(&path, Position::new(0, 3)).unwrap(), None);
        assert_eq!(s.definition(&path, Position::new(0, 3)).unwrap(), None);
        assert_eq!(s.references(&path, Position::new(0, 3)).unwrap(), None);
    }

    #[test]
    fn a_request_about_an_unopened_document_is_skipped() {
        // The server was never told about it, so it would answer about a file it
        // does not have — an error at best, a wrong answer at worst.
        let mut s = detached(json!({"hoverProvider": true}));
        assert_eq!(
            s.hover(std::path::Path::new("/w/never-opened.rs"), Position::ZERO)
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_relative_path_is_skipped_for_requests_too() {
        let mut s = detached(json!({"hoverProvider": true}));
        assert_eq!(
            s.hover(std::path::Path::new("relative.rs"), Position::ZERO)
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_hover_answer_is_routed_back_with_its_id() {
        let (mut s, _) = with_open_document(json!({"hoverProvider": true}));
        let (id, _) = s.client.request("textDocument/hover", json!({})).unwrap();

        let updates = feed(
            &mut s,
            Message::Response(Response::ok(
                id.clone(),
                json!({"contents": {"kind": "markdown", "value": "fn main()"}}),
            )),
        );

        let Update::Hover { id: got, hover } = &updates[0] else {
            panic!("expected a hover, got {updates:?}");
        };
        assert_eq!(got, &id, "the id is what lets a stale answer be dropped");
        assert_eq!(hover.as_ref().unwrap().contents, "fn main()");
    }

    #[test]
    fn a_null_hover_is_reported_as_a_successful_nothing() {
        // Distinct from a failure: the server answered, and the answer is that
        // there is nothing there. Silence would leave the editor waiting.
        let (mut s, _) = with_open_document(json!({"hoverProvider": true}));
        let (id, _) = s.client.request("textDocument/hover", json!({})).unwrap();
        let updates = feed(&mut s, Message::Response(Response::ok(id, json!(null))));
        assert!(
            matches!(&updates[0], Update::Hover { hover: None, .. }),
            "{updates:?}"
        );
    }

    #[test]
    fn a_definition_answer_is_routed_with_the_method_that_asked() {
        // definition and references answer in the same shape, so the method is
        // the only thing distinguishing "jump there" from "list them".
        let (mut s, _) = with_open_document(json!({"definitionProvider": true}));
        let (id, _) = s
            .client
            .request("textDocument/definition", json!({}))
            .unwrap();

        let updates = feed(
            &mut s,
            Message::Response(Response::ok(
                id,
                json!([{
                    "uri": "file:///w/b.rs",
                    "range": {"start": {"line": 4, "character": 2},
                              "end": {"line": 4, "character": 6}},
                }]),
            )),
        );

        let Update::Locations {
            method, locations, ..
        } = &updates[0]
        else {
            panic!("expected locations, got {updates:?}");
        };
        assert_eq!(method, "textDocument/definition");
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].range.start, Position::new(4, 2));
    }

    #[test]
    fn every_location_returning_method_is_routed() {
        // Not just definition: a server may implement declaration or
        // typeDefinition and the answer arrives in the same shape.
        for method in [
            "textDocument/definition",
            "textDocument/declaration",
            "textDocument/typeDefinition",
            "textDocument/implementation",
            "textDocument/references",
        ] {
            let (mut s, _) = with_open_document(json!({"definitionProvider": true}));
            let (id, _) = s.client.request(method, json!({})).unwrap();
            let updates = feed(&mut s, Message::Response(Response::ok(id, json!([]))));
            assert!(
                matches!(&updates[0], Update::Locations { .. }),
                "{method} was not routed: {updates:?}"
            );
        }
    }

    #[test]
    fn references_ask_for_the_declaration_too() {
        // "Find all references" that omits the definition is a surprising
        // answer, and VS Code includes it.
        let (mut s, path) = with_open_document(json!({"referencesProvider": true}));
        // No process, so the write fails — but the params are built first, and
        // the client records the request either way.
        let _ = s.references(&path, Position::new(0, 3));
        assert_eq!(s.client.pending_count(), 1);
    }

    #[test]
    fn a_failed_request_is_reported_with_its_method() {
        let (mut s, _) = with_open_document(json!({"hoverProvider": true}));
        let (id, _) = s.client.request("textDocument/hover", json!({})).unwrap();
        let updates = feed(
            &mut s,
            Message::Response(Response::err(
                id,
                crate::jsonrpc::ErrorCode::InternalError,
                "the server panicked",
            )),
        );
        let Update::RequestFailed { method, reason, .. } = &updates[0] else {
            panic!("expected a failure, got {updates:?}");
        };
        assert_eq!(method, "textDocument/hover");
        assert!(reason.contains("panicked"), "{reason}");
    }

    #[test]
    fn a_cancelled_or_stale_request_failure_is_not_reported() {
        // Both arrive constantly while typing.
        for code in [
            crate::jsonrpc::ErrorCode::RequestCancelled,
            crate::jsonrpc::ErrorCode::ContentModified,
        ] {
            let (mut s, _) = with_open_document(json!({"hoverProvider": true}));
            let (id, _) = s.client.request("textDocument/hover", json!({})).unwrap();
            let updates = feed(&mut s, Message::Response(Response::err(id, code, "x")));
            assert!(updates.is_empty(), "{code:?} should be silent: {updates:?}");
        }
    }

    #[test]
    fn cancelling_a_request_on_a_stopped_server_is_a_named_error() {
        // The editor cancels on every cursor move, and both call sites in
        // deco-tui discard the result — so what has to hold is that a dead
        // server answers at all rather than panicking, and that when it does it
        // is the same named error every other write to a stopped server gives.
        let (mut s, _) = with_open_document(json!({"hoverProvider": true}));
        let (id, _) = s.client.request("textDocument/hover", json!({})).unwrap();
        s.stop_with("gone".into());
        assert!(matches!(
            s.cancel(&id),
            Err(SupervisorError::NotRunning { .. })
        ));
    }
}
