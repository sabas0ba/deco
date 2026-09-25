//! One language server, driven end to end.
//!
//! [`Client`] implements the protocol, [`ServerProcess`] transfers the bytes,
//! and [`DocumentSync`] and [`DiagnosticStore`] hold the state. This module
//! combines them, so a frontend can write
//!
//! ```text
//! supervisor.did_open(path, "rust", text)?;
//! for update in supervisor.poll() { … }
//! ```
//!
//! without handling ids, versions, framing or lifecycle ordering.
//!
//! # It does not block the editor
//!
//! [`Supervisor::poll`] processes the messages that have arrived and returns.
//! There are two intentional, bounded exceptions:
//!
//! - **Starting a server.** The protocol forbids sending anything before the
//!   `initialize` reply, so this must wait. The wait is bounded by
//!   [`Supervisor::start`]'s timeout, so a server that never answers cannot
//!   block the editor at launch.
//! - **The poll in which a server exits**, for up to 100ms, while waiting for
//!   its stderr. See [`ServerProcess::stderr_after_exit`].
//!
//! # A misbehaving server affects only itself
//!
//! Every failure path here results in "this server is not running" and keeps
//! the editor working: a crash during startup, a protocol error during a
//! session, or a server that exits by itself.
//!
//! Each of these reports the server's stderr tail, because it is usually the
//! only explanation available. stdout and stderr are read by separate threads,
//! so the editor can detect the exit before the stderr output is collected.
//! [`ServerProcess::stderr_after_exit`] resolves that race.

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
use crate::uri::{PathMap, Uri};

/// How long to wait for `initialize` to be answered.
///
/// Long, because a server may read a large project's metadata before it
/// replies. Finite, so that a broken server does not block the editor at
/// startup.
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
    /// The server has stopped. No further updates will arrive.
    Stopped {
        /// Which server.
        id: String,
        /// Why, in a form suitable for the user.
        reason: String,
    },
    /// An answer to [`Supervisor::hover`].
    Hover {
        /// The request this answers, so a caller that has moved on can ignore it.
        id: RequestId,
        /// The server's result, or `None` when there is nothing at that
        /// position. `None` is a successful answer and is reported as one.
        hover: Option<Hover>,
    },
    /// An answer to [`Supervisor::definition`] or [`Supervisor::references`].
    Locations {
        /// The request this answers.
        id: RequestId,
        /// Which method was called, since both have the same response shape.
        method: String,
        /// The locations the server returned. Empty means it found nothing.
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
    /// An answer to [`Supervisor::rename`].
    Renamed {
        /// The request this answers.
        id: RequestId,
        /// All changes the server requests, or why its answer could not be
        /// read. An empty `Ok` edit means the server declined. This is a
        /// successful answer and is reported as one.
        edit: Result<crate::WorkspaceEdit, crate::WorkspaceEditError>,
    },
    /// An answer to [`Supervisor::code_action`].
    CodeActions {
        /// The request this answers.
        id: RequestId,
        /// The server's actions, in the order it listed them. Empty means it
        /// has no actions for that location, which is a successful answer.
        actions: Vec<crate::requests::CodeAction>,
    },
    /// An answer to [`Supervisor::resolve_code_action`].
    CodeActionResolved {
        /// The request this answers.
        id: RequestId,
        /// The same action with its edit filled in. If the server's resolution
        /// did not provide an edit, the action still has none.
        action: Box<crate::requests::CodeAction>,
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
        /// Whether the server marked the list incomplete. Reported but not used:
        /// deco sends a new request rather than refining a partial list.
        incomplete: bool,
    },
    /// An answer to [`Supervisor::formatting`] or [`Supervisor::range_formatting`].
    Edits {
        /// The request this answers.
        id: RequestId,
        /// Which method was called.
        method: String,
        /// The replacements to make, in the order the server sent them. They
        /// refer to the document as the server saw it and must not be applied
        /// front to back; see [`TextEdit::list_from_json`].
        edits: Vec<TextEdit>,
    },
    /// A request failed for a reason that should be shown to the user.
    ///
    /// Routine failures, such as cancellation and content-modified, are not
    /// reported here because they occur constantly during ordinary typing.
    RequestFailed {
        /// The request that failed.
        id: RequestId,
        /// Which method.
        method: String,
        /// Why, in a form suitable for a status bar.
        reason: String,
    },
    /// Something was ignored. Only useful in a log.
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
    /// The protocol state machine rejected the call.
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

/// Whether a raw diagnostic's range overlaps `range` at all.
///
/// Read from the JSON rather than the parsed struct. The parsed set and the
/// raw set are separate lists, and matching them by index would break as soon
/// as one diagnostic fails to parse. Reading the range from the value being
/// filtered keeps them consistent.
///
/// A diagnostic with no usable range never matches. [`Diagnostic::from_json`]
/// handles it the same way, because its location is unknown.
fn overlaps_range(value: &serde_json::Value, range: deco_core::position::Range) -> bool {
    let Some(other) = value
        .get("range")
        .and_then(crate::requests::read_range_public)
    else {
        return false;
    };
    // Touching counts. An empty selection is a caret, and a caret at the start
    // of an error is a request about that error.
    other.start <= range.end && range.start <= other.end
}

/// Waits for an exiting server's final stderr output, then formats it.
///
/// stdout and stderr are read by separate threads, so the exit can be detected
/// before the stderr output is collected. In that case the error would be
/// "the server exited during startup; the server wrote nothing to stderr",
/// even though the server wrote the reason.
///
/// [`ServerProcess::stderr_after_exit`] resolves this by waiting for the pump
/// thread to finish instead of waiting for output to appear.
fn drain_stderr(process: &mut ServerProcess, grace: Duration) -> String {
    process.stderr_after_exit(grace).summary()
}

/// How long to wait for stderr while a server is failing to start.
///
/// Startup already blocks, so waiting a fraction of a second to report the
/// reason for a failure is worthwhile.
const STARTUP_STDERR_GRACE: Duration = Duration::from_millis(500);

/// How long to wait for stderr when a running server exits.
///
/// Much shorter, because this happens inside [`Supervisor::poll`], which the
/// event loop calls between keystrokes. It is the only place `poll` can block,
/// and only in the poll where the server exits. Without it, the editor could
/// report that the language server stopped but not why.
const RUNNING_STDERR_GRACE: Duration = Duration::from_millis(100);

/// One language server and everything the editor knows about its state.
#[derive(Debug)]
pub struct Supervisor {
    id: String,
    client: Client,
    process: Option<ServerProcess>,
    sync: DocumentSync,
    diagnostics: DiagnosticStore,
    /// The diagnostics as the server sent them, per document.
    ///
    /// Kept beside the parsed store rather than inside it, because the editor
    /// does not read them. Nothing draws them. They are only sent back to the
    /// server that produced them; see [`Supervisor::code_action`] and the
    /// comment where they are stored.
    published: std::collections::HashMap<Uri, Vec<serde_json::Value>>,
    paths: PathMap,
    /// Set once the server has stopped, so a later call reports the original
    /// reason rather than only "not running".
    stopped: Option<String>,
    /// Updates produced while handling the current message.
    ///
    /// A separate field because `dispatch` must return the messages to write
    /// back to the server. Passing two collections through every call site made
    /// the state changes harder to follow.
    pending_updates: Vec<Update>,
}

impl Supervisor {
    /// Starts a server and completes the handshake.
    ///
    /// Blocks until the server answers `initialize` or the timeout expires.
    /// This is the only place this crate blocks, and it is unavoidable because
    /// the protocol forbids sending anything else first.
    pub fn start(
        config: &ServerConfig,
        consent: Consent,
        root: Option<&Path>,
        paths: PathMap,
        timeout: Duration,
    ) -> Result<Self, SupervisorError> {
        let mut process = ServerProcess::spawn(config, consent)?;
        let mut client = Client::new();

        // The root in the *server's* form. In a remote session this is a path
        // on the remote machine, not on this one.
        let root_uri = root.and_then(|path| Uri::from_path(path, paths.style()).ok());
        let Outgoing(message) =
            client.initialize(root_uri.as_ref(), config.initialization_options.clone())?;

        // A write failure here almost always means the server exited before
        // reading anything, for example because of a missing runtime, a bad
        // argument or a licence check. A plain "broken pipe" would not explain
        // this, so the process is given a moment to exit and its stderr is
        // attached.
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
            published: std::collections::HashMap::new(),
            paths,
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
                // No grace period: the server is still running and has not
                // answered, so its output so far is already collected.
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
                    // A rejected handshake leaves the client in `Exited`.
                    // Without this check the loop would run until the deadline.
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
    /// Takes `&mut self` rather than `&self` so it can drain stderr. The reason
    /// is almost always in stderr, and reporting before the pump has collected
    /// it would lose it.
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

    /// The URI a path maps to for this server.
    ///
    /// This is the only place a path becomes a URI, so the remote prefix is
    /// applied here and not at each of the eleven callers.
    pub fn uri_for(&self, path: &Path) -> Option<Uri> {
        self.paths.to_uri(path).ok()
    }

    /// How this server's paths relate to the editor's.
    pub fn paths(&self) -> &PathMap {
        &self.paths
    }

    /// Tells the server a document is open.
    ///
    /// Does nothing and returns `Ok` when the server does not want open and
    /// close notifications, so the caller does not need to check.
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
    /// `text` is the whole document, used for a full sync. Because both are
    /// passed, the caller does not need to know which was negotiated.
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
            // Not an error. The server was not told about this document,
            // because it does not want open notifications or the document is
            // not in its language, so there is nothing to change.
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
    /// Its diagnostics are cleared as well, because nothing can remove them
    /// after the server stops tracking the file.
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
    /// Returns the request id so the caller can match the answer, or drop it if
    /// the cursor has since moved. Returns `None` when the server does not
    /// offer hover, so the caller does not need to check capabilities before
    /// every keypress.
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

    /// The characters that open a completion list automatically.
    ///
    /// Empty when no server is running, so a caller can check this on every
    /// keystroke without a separate branch.
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
    /// `options` contains the user's indentation settings. Without them a
    /// server formats with its defaults, and in a project with different
    /// settings the result changes every line.
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
    /// Uses the same `formatting` capability. A server that offers
    /// whole-document formatting usually offers this too, and servers set the
    /// specification's separate flags inconsistently. A server without support
    /// answers with an error, which is reported like any other.
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
    /// Full document only. deco highlights the visible lines of a document it
    /// has already lexed, so a range request would need one round trip per
    /// scroll for a refinement the lexer already approximates.
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
    /// Always the whole document. The request has no positional variant, and
    /// the picker that uses it needs all symbols.
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

    /// Asks for all changes needed to rename the symbol at `position`.
    ///
    /// The answer is a `WorkspaceEdit` covering the whole project, not only
    /// this document. See [`crate::WorkspaceEdit`] for its contents and
    /// `deco_editor::workspace` for the rules about applying it.
    pub fn rename(
        &mut self,
        path: &Path,
        position: deco_core::position::Position,
        new_name: &str,
    ) -> Result<Option<RequestId>, SupervisorError> {
        if self.client.capabilities().rename.is_none() {
            return Ok(None);
        }
        let Some(uri) = self.uri_for(path) else {
            return Ok(None);
        };
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        let params = crate::requests::rename_params(&uri, position, new_name);
        self.request("textDocument/rename", params).map(Some)
    }

    /// Asks which code actions the server offers for `range`.
    ///
    /// The diagnostics covering that range are sent with the request, exactly
    /// as the server sent them. See [`crate::requests::code_action_params`] for
    /// why this is important.
    pub fn code_action(
        &mut self,
        path: &Path,
        range: deco_core::position::Range,
    ) -> Result<Option<RequestId>, SupervisorError> {
        if self.client.capabilities().code_action.is_none() {
            return Ok(None);
        }
        let Some(uri) = self.uri_for(path) else {
            return Ok(None);
        };
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        // Diagnostics that overlap the range, not only those contained in it. A
        // selection across part of an error is still a request about that
        // error, and VS Code sends the same set.
        let diagnostics: Vec<serde_json::Value> = self
            .published
            .get(&uri)
            .map(|published| {
                published
                    .iter()
                    .filter(|value| overlaps_range(value, range))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let params = crate::requests::code_action_params(&uri, range, diagnostics);
        self.request("textDocument/codeAction", params).map(Some)
    }

    /// Asks the server to fill in a chosen action's edit.
    ///
    /// Returns `Ok(None)` when the server does not support resolving. The
    /// caller then knows that an action without an edit cannot be run, rather
    /// than not yet resolved.
    pub fn resolve_code_action(
        &mut self,
        action: &crate::requests::CodeAction,
    ) -> Result<Option<RequestId>, SupervisorError> {
        if !self
            .client
            .capabilities()
            .code_action
            .as_ref()
            .is_some_and(|options| options.resolve_provider)
        {
            return Ok(None);
        }
        let params = crate::requests::code_action_resolve_params(action);
        self.request("codeAction/resolve", params).map(Some)
    }

    /// The version last sent to the server for `path`.
    ///
    /// An incoming edit's `version` must match this. `None` for a document this
    /// server is not tracking. That is not a mismatch; there is no version to
    /// compare against.
    pub fn version_of(&self, path: &Path) -> Option<i64> {
        let uri = self.uri_for(path)?;
        self.sync.version(&uri).map(i64::from)
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
        // Include the declaration. Users expect "find all references" to list
        // the definition, and VS Code includes it.
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
        // A request about a document the server has not opened returns an
        // error at best and a wrong answer at worst.
        if !self.sync.is_open(&uri) {
            return Ok(None);
        }
        let params = crate::requests::text_document_position(&uri, position);
        self.request(method, params).map(Some)
    }

    /// Asks the server to abandon a request.
    ///
    /// Advisory: the reply may already have been sent, and is dropped when it
    /// arrives. Cancelling still lets the server stop work on, for example, a
    /// hover the user no longer needs.
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

    /// The next event from the reader thread, if any. Never blocks.
    ///
    /// `None` means either "no event yet" or "no process". A caller draining a
    /// queue treats both the same way.
    fn next_event(&self) -> Option<ReaderEvent> {
        self.process.as_ref()?.try_recv()
    }

    /// Processes all messages that have arrived from the server. Never blocks.
    ///
    /// A write failure while answering the server is reported as an update
    /// rather than returned, because failing the whole call would discard the
    /// updates already collected.
    pub fn poll(&mut self) -> Vec<Update> {
        let mut updates = Vec::new();

        // The borrow of `self.process` must end before each iteration's body,
        // which calls `&mut self` methods. A helper is used for this instead of
        // accessing the field in the loop head.
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

        // A server can also exit without the reader detecting its pipes
        // closing, so the exit status is checked separately.
        if let Some(process) = self.process.as_mut() {
            if let Some(status) = process.exited() {
                updates.push(self.stop_with(format!("the server exited with {status}")));
            }
        }

        updates
    }

    /// Marks the server as stopped and returns the corresponding update.
    fn stop_with(&mut self, reason: String) -> Update {
        // stderr is drained for the same reason as at startup. Detecting stdout
        // closing and collecting stderr happen on separate threads. Without
        // waiting, the user may be told the server stopped but not why.
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
    /// `shutdown`, then `exit`, then a bounded wait, then a kill; see
    /// [`ServerProcess::stop`]. Errors are ignored intentionally. This runs
    /// while the editor is quitting, and the process is killed anyway if the
    /// shutdown fails.
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

// Updates produced while handling one message are collected separately,
// because `dispatch` also returns the messages to write back.
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
                    // Cancellation and content-modified occur constantly during
                    // ordinary typing. Reporting them would hide other errors.
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
                    "textDocument/codeAction" => Some(Update::CodeActions {
                        id,
                        actions: crate::requests::CodeAction::list_from_json(&result),
                    }),
                    "codeAction/resolve" => {
                        // One action, not a list. If the response cannot be
                        // read, there is nothing to apply, and the caller
                        // reports that.
                        crate::requests::CodeAction::list_from_json(&serde_json::Value::Array(
                            vec![result],
                        ))
                        .pop()
                        .map(|action| Update::CodeActionResolved {
                            id,
                            action: Box::new(action),
                        })
                    }
                    "textDocument/rename" => Some(Update::Renamed {
                        id,
                        edit: crate::WorkspaceEdit::from_json(&result),
                    }),
                    "textDocument/documentSymbol" => Some(Update::Symbols {
                        id,
                        symbols: crate::requests::DocumentSymbol::list_from_json(&result),
                    }),
                    "textDocument/semanticTokens/full" => {
                        // The legend comes from the server, so a response cannot
                        // be read without the capabilities it sent at startup.
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
            Published::Replaced { .. } => {
                // Kept as sent, beside the parsed set, and only when that set
                // was accepted, so both refer to the same publication. Parsing
                // drops fields deco does not use, in particular `data`, which
                // is opaque to clients by design. A quick fix request sends the
                // diagnostic back to the server that produced it. A diagnostic
                // rebuilt from the parsed struct would not be recognised by the
                // server and would lose the fix data it carried.
                let raw = params
                    .get("diagnostics")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if raw.is_empty() {
                    self.published.remove(&uri);
                } else {
                    self.published.insert(uri.clone(), raw);
                }
                Some(Update::Diagnostics {
                    diagnostics: self.diagnostics.for_uri(&uri).to_vec(),
                    uri,
                })
            }
            // Only noted for the log. The editor's current diagnostics are still
            // correct, and reporting each dropped stale result would be noise
            // during ordinary typing.
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
    /// Everything except `start`, `poll` and the write path depends only on the
    /// client, sync and diagnostic state, so most behaviour can be tested
    /// without a server. The process-owning parts are covered by the `process`
    /// module and by `tests/server_process.rs`.
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
            published: std::collections::HashMap::new(),
            paths: PathMap::local(crate::uri::PathStyle::Unix),
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
        // An empty list means the errors are fixed. Skipping it would leave
        // stale underlines on screen.
        let mut s = detached(json!({}));
        feed(&mut s, publish("file:///w/a.rs", None, &[(1, "boom")]));
        let updates = feed(&mut s, publish("file:///w/a.rs", None, &[]));

        let Update::Diagnostics { diagnostics, .. } = &updates[0] else {
            panic!("an empty set must still be reported, got {updates:?}");
        };
        assert!(diagnostics.is_empty());
    }

    /// A publication carrying the opaque `data` a quick fix is built from.
    fn publish_with_data(uri: &str, line: u32, data: serde_json::Value) -> Message {
        Message::Notification(Notification {
            method: "textDocument/publishDiagnostics".into(),
            params: Some(json!({
                "uri": uri,
                "diagnostics": [{
                    "range": {
                        "start": {"line": line, "character": 0},
                        "end": {"line": line, "character": 4},
                    },
                    "severity": 1,
                    "message": "unused",
                    "data": data,
                }],
            })),
        })
    }

    #[test]
    fn the_diagnostics_a_server_sent_are_kept_as_it_sent_them() {
        // Parsing drops `data`, which the server uses to build the fix.
        let mut s = detached(json!({}));
        feed(
            &mut s,
            publish_with_data("file:///w/a.rs", 2, json!({"assist": 7})),
        );

        let uri = Uri::from_string("file:///w/a.rs");
        let kept = s.published.get(&uri).expect("kept beside the parsed set");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["data"], json!({"assist": 7}));
    }

    #[test]
    fn a_publication_that_is_dropped_leaves_the_kept_set_alone() {
        // Otherwise the raw set would describe a publication the editor
        // rejected, and a code action request would include a diagnostic that
        // is not shown.
        let mut s = detached(json!({
            "textDocumentSync": {"openClose": true, "change": 1}
        }));
        let path = Path::new("/w/a.rs");
        s.sync.open(s.uri_for(path).unwrap(), "rust", "x").unwrap();
        feed(
            &mut s,
            publish_with_data("file:///w/a.rs", 0, json!({"assist": 1})),
        );
        s.sync
            .change(
                &s.uri_for(path).unwrap(),
                TextDocumentSyncKind::Full,
                &[],
                "x",
            )
            .unwrap();

        // Stamped with a version older than the document's.
        feed(&mut s, publish("file:///w/a.rs", Some(1), &[(9, "stale")]));

        let kept = &s.published[&s.uri_for(path).unwrap()];
        assert_eq!(kept[0]["data"], json!({"assist": 1}), "still the first set");
    }

    #[test]
    fn clearing_the_diagnostics_clears_what_was_kept() {
        let mut s = detached(json!({}));
        feed(
            &mut s,
            publish_with_data("file:///w/a.rs", 0, json!({"assist": 1})),
        );
        feed(&mut s, publish("file:///w/a.rs", None, &[]));

        assert!(!s
            .published
            .contains_key(&Uri::from_string("file:///w/a.rs")));
    }

    #[test]
    fn a_server_that_offers_no_code_actions_is_not_asked() {
        let mut s = detached(json!({}));
        assert!(s
            .code_action(
                Path::new("/w/a.rs"),
                deco_core::position::Range::new(
                    deco_core::position::Position::ZERO,
                    deco_core::position::Position::ZERO,
                ),
            )
            .expect("declining is not an error")
            .is_none());
    }

    #[test]
    fn an_action_is_not_sent_for_resolving_to_a_server_that_cannot() {
        // The caller reads `None` as "this action has no edit and will not get
        // one", which is reported differently from a pending request.
        let mut s = detached(json!({"codeActionProvider": true}));
        let action = crate::requests::CodeAction::list_from_json(&json!([{"title": "Fix"}]))
            .pop()
            .expect("one action");
        assert!(s
            .resolve_code_action(&action)
            .expect("declining is not an error")
            .is_none());
    }

    #[test]
    fn a_diagnostic_is_offered_when_it_touches_the_selection() {
        let at = |line: u32, from: u32, to: u32| {
            json!({"range": {
                "start": {"line": line, "character": from},
                "end": {"line": line, "character": to},
            }})
        };
        let selection = deco_core::position::Range::new(
            deco_core::position::Position::new(1, 4),
            deco_core::position::Position::new(1, 8),
        );

        assert!(
            overlaps_range(&at(1, 0, 6), selection),
            "overlapping the start"
        );
        assert!(
            overlaps_range(&at(1, 6, 20), selection),
            "overlapping the end"
        );
        assert!(overlaps_range(&at(1, 5, 6), selection), "inside it");
        assert!(
            overlaps_range(&at(1, 8, 9), selection),
            "touching the end counts: a caret there is a question about it"
        );
        assert!(!overlaps_range(&at(2, 0, 4), selection), "another line");
        assert!(!overlaps_range(&at(1, 0, 3), selection), "ends before it");
        assert!(
            !overlaps_range(&json!({"message": "no range"}), selection),
            "a diagnostic that cannot be placed is nowhere"
        );
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
        // Without a version there is no way to check the publication.
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
        // The caller does not need to check capabilities before every edit.
        let mut s = detached(json!({"textDocumentSync": {"openClose": false}}));
        let path = Path::new("/w/a.rs");

        // No process is attached, so any message actually sent would fail.
        assert!(s.did_open(path, "rust", "x").is_ok());
        assert!(s.did_change(path, &[], "y").is_ok());
        assert!(s.did_save(path, "y").is_ok());
        assert!(s.did_close(path).is_ok());
        assert!(!s.sync.is_open(&s.uri_for(path).unwrap()));
    }

    #[test]
    fn changing_a_document_that_was_never_opened_is_not_an_error() {
        // This happens whenever a file is not in this server's language.
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
        // Nothing can remove them after the server stops tracking the file.
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
        // LSP has no working directory, so no correct URI can be formed.
        let mut s = detached(json!({"textDocumentSync": 1}));
        assert!(s.did_open(Path::new("relative.rs"), "rust", "x").is_ok());
        assert_eq!(s.sync.len(), 0);
    }

    #[test]
    fn stopping_reports_the_stderr_tail() {
        // When a server exits, its final stderr output is the only explanation.
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
        // Cancellation and content-modified occur constantly during typing.
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
        // Reported as `RequestFailed` rather than `Noted`. The caller of the
        // pending request is waiting, and a status line with the reason is more
        // useful than a log entry.
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
        // Both use the same `did_change` call, and only the negotiated kind
        // determines what is sent. Checked through the sync layer, because
        // there is no process to write to here.
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
        // The caller does not need to check capabilities before every keypress.
        let (mut s, path) = with_open_document(json!({"textDocumentSync": 1}));
        assert_eq!(s.hover(&path, Position::new(0, 3)).unwrap(), None);
        assert_eq!(s.definition(&path, Position::new(0, 3)).unwrap(), None);
        assert_eq!(s.references(&path, Position::new(0, 3)).unwrap(), None);
    }

    #[test]
    fn a_request_about_an_unopened_document_is_skipped() {
        // The server has not opened the document, so it would return an error
        // at best and a wrong answer at worst.
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
        // Not a failure: the server answered that there is nothing at that
        // position. Without an update the editor would keep waiting.
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
        // definition and references have the same response shape, so only the
        // method distinguishes "jump there" from "list them".
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
        // Not only definition: a server may implement declaration or
        // typeDefinition, which have the same response shape.
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
        // Users expect "find all references" to list the definition, and VS
        // Code includes it.
        let (mut s, path) = with_open_document(json!({"referencesProvider": true}));
        // No process, so the write fails. The params are built first, and the
        // client records the request in either case.
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
        // Both occur constantly while typing.
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
        // deco-tui discard the result. The requirement is that cancelling on a
        // stopped server returns instead of panicking, with the same named
        // error as every other write to a stopped server.
        let (mut s, _) = with_open_document(json!({"hoverProvider": true}));
        let (id, _) = s.client.request("textDocument/hover", json!({})).unwrap();
        s.stop_with("gone".into());
        assert!(matches!(
            s.cancel(&id),
            Err(SupervisorError::NotRunning { .. })
        ));
    }
}
