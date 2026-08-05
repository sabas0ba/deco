//! The session state machine: lifecycle, request routing and cancellation.
//!
//! This is a pure state machine. It never spawns a process, never blocks and
//! never touches a socket: messages go in, messages and events come out, and
//! whoever owns the child process is responsible for moving bytes. That is what
//! makes the ordering rules below testable without a language server installed.
//!
//! # The lifecycle, and why it is enforced
//!
//! ```text
//! Uninitialized ──initialize──▶ Initializing ──response──▶ Ready
//!                                                            │
//!                                              shutdown ─────┤
//!                                                            ▼
//!                                                       ShuttingDown
//!                                                            │
//!                                                  exit ─────┤
//!                                                            ▼
//!                                                          Exited
//! ```
//!
//! Servers enforce this and respond badly when it is broken: a request before
//! the `initialize` response is answered with `ServerNotInitialized` at best,
//! and several implementations simply hang. So requests raised early are
//! **queued**, not sent and not rejected — the editor should not have to know
//! whether the server has finished starting before it can ask for a hover.
//!
//! `exit` without a preceding `shutdown` is the one that leaks: it tells the
//! server to die immediately, and a server that has not been asked to shut down
//! may not flush or clean up. [`Client::exit`] refuses it.
//!
//! # Server-to-client requests
//!
//! Traffic is bidirectional. A server request that never receives a response
//! blocks that server — many wait synchronously — so every inbound request is
//! answered, including the ones deco does not implement, which get an explicit
//! `MethodNotFound` rather than silence.

use std::collections::HashMap;

use crate::capabilities::{self, NegotiationError, PositionEncoding, ServerCapabilities};
use crate::jsonrpc::{ErrorCode, Message, Notification, Request, RequestId, Response};

/// Where a session is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing has been sent.
    Uninitialized,
    /// `initialize` is in flight.
    Initializing,
    /// The handshake finished. Requests flow.
    Ready,
    /// `shutdown` is in flight or answered; only `exit` may follow.
    ShuttingDown,
    /// `exit` was sent. The session is over.
    Exited,
}

/// Something the caller must send to the server.
#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing(pub Message);

/// Something the editor should act on.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// The handshake completed.
    Initialized {
        /// What the server said it can do.
        capabilities: Box<ServerCapabilities>,
    },
    /// A reply to a request the editor made.
    Response {
        /// The method that was called, recovered from the pending table — a
        /// bare response carries only an id, so without this the editor cannot
        /// tell what it is looking at.
        method: String,
        /// The id that was answered.
        id: RequestId,
        /// The result, on success.
        result: Option<serde_json::Value>,
        /// The failure, on error.
        error: Option<crate::jsonrpc::ResponseError>,
    },
    /// A notification from the server that the editor handles, e.g.
    /// `textDocument/publishDiagnostics`.
    Notification(Notification),
    /// The server asked to show a message.
    ShowMessage {
        /// 1 error, 2 warning, 3 info, 4 log.
        kind: i64,
        /// The text.
        message: String,
    },
    /// The server logged something. Usually only interesting when debugging.
    LogMessage {
        /// 1 error, 2 warning, 3 info, 4 log.
        kind: i64,
        /// The text.
        message: String,
    },
    /// Something arrived that the client could not use, described for a log.
    ///
    /// An event rather than an error: a misbehaving server should be visible
    /// without taking the editor down with it.
    Ignored {
        /// Why it was ignored.
        reason: String,
    },
}

/// Why a call was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LspError {
    /// The call does not make sense in the current state.
    #[error("cannot {action} while {state:?}")]
    WrongState {
        /// What was attempted.
        action: &'static str,
        /// The state it was attempted in.
        state: State,
    },
    /// `exit` was called without `shutdown`.
    #[error("exit without shutdown would kill the server before it can clean up")]
    ExitWithoutShutdown,
    /// The server chose a position encoding the client cannot honour.
    #[error(transparent)]
    Negotiation(#[from] NegotiationError),
}

/// A request that has been sent and not yet answered.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    method: String,
    /// Whether the editor has already asked for this to be cancelled. The
    /// response still arrives — cancellation is advisory — and is dropped.
    cancelled: bool,
}

/// One conversation with one language server.
#[derive(Debug)]
pub struct Client {
    state: State,
    next_id: i64,
    pending: HashMap<RequestId, Pending>,
    /// Requests raised before the handshake finished, replayed on `Ready`.
    queued: Vec<Request>,
    capabilities: ServerCapabilities,
    encoding: PositionEncoding,
    initialize_id: Option<RequestId>,
    shutdown_id: Option<RequestId>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// A session that has not started.
    pub fn new() -> Self {
        Self {
            state: State::Uninitialized,
            next_id: 1,
            pending: HashMap::new(),
            queued: Vec::new(),
            capabilities: ServerCapabilities::default(),
            encoding: PositionEncoding::Utf16,
            initialize_id: None,
            shutdown_id: None,
        }
    }

    /// Where the session is.
    pub fn state(&self) -> State {
        self.state
    }

    /// What the server said it can do. All false until the handshake finishes.
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    /// The negotiated position encoding.
    pub fn position_encoding(&self) -> PositionEncoding {
        self.encoding
    }

    /// How many requests are awaiting a reply.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// How many requests are waiting for the handshake to finish.
    pub fn queued_count(&self) -> usize {
        self.queued.len()
    }

    fn allocate_id(&mut self) -> RequestId {
        let id = RequestId::Number(self.next_id);
        // Saturating rather than wrapping: reusing an id would route a reply to
        // the wrong request, which is worse than never issuing another. In
        // practice a session ends long before 2^63 requests.
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Builds the `initialize` request.
    ///
    /// `root_uri` is the workspace root, or `None` for a single loose file —
    /// which is a legitimate configuration, not an error.
    pub fn initialize(
        &mut self,
        root_uri: Option<&crate::uri::Uri>,
        initialization_options: Option<serde_json::Value>,
    ) -> Result<Outgoing, LspError> {
        if self.state != State::Uninitialized {
            return Err(LspError::WrongState {
                action: "initialize",
                state: self.state,
            });
        }

        let id = self.allocate_id();
        let mut params = serde_json::json!({
            // Null rather than the real pid: it is only used so a server can
            // notice the editor died, and handing out a process id to a
            // subprocess that did not need one is a habit worth not having.
            "processId": serde_json::Value::Null,
            "clientInfo": { "name": "deco", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": capabilities::client_capabilities(),
            "rootUri": root_uri.map(|u| u.as_str()),
            "workspaceFolders": serde_json::Value::Null,
        });
        if let Some(options) = initialization_options {
            params["initializationOptions"] = options;
        }

        self.state = State::Initializing;
        self.initialize_id = Some(id.clone());
        self.pending.insert(
            id.clone(),
            Pending {
                method: "initialize".into(),
                cancelled: false,
            },
        );

        Ok(Outgoing(Message::Request(Request {
            id,
            method: "initialize".into(),
            params: Some(params),
        })))
    }

    /// Raises a request.
    ///
    /// Before the handshake finishes the request is queued rather than sent,
    /// and the returned `Outgoing` is `None`. Callers get the id either way, so
    /// a queued request can still be cancelled by the time it is flushed.
    pub fn request(
        &mut self,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<(RequestId, Option<Outgoing>), LspError> {
        let method = method.into();
        match self.state {
            State::Uninitialized | State::ShuttingDown | State::Exited => {
                return Err(LspError::WrongState {
                    action: "send a request",
                    state: self.state,
                })
            }
            State::Initializing | State::Ready => {}
        }

        let id = self.allocate_id();
        let request = Request {
            id: id.clone(),
            method: method.clone(),
            params: Some(params),
        };

        self.pending.insert(
            id.clone(),
            Pending {
                method,
                cancelled: false,
            },
        );

        if self.state == State::Initializing {
            self.queued.push(request);
            return Ok((id, None));
        }
        Ok((id, Some(Outgoing(Message::Request(request)))))
    }

    /// Builds a notification.
    ///
    /// Unlike a request, a notification raised before the handshake is refused
    /// rather than queued: notifications are almost always document
    /// synchronisation, and replaying `didChange` for a document the server was
    /// never told about is a protocol violation on arrival.
    pub fn notify(
        &mut self,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<Outgoing, LspError> {
        if self.state != State::Ready {
            return Err(LspError::WrongState {
                action: "send a notification",
                state: self.state,
            });
        }
        Ok(Outgoing(Message::Notification(Notification {
            method: method.into(),
            params: Some(params),
        })))
    }

    /// Asks the server to abandon a request.
    ///
    /// Advisory by design: the server may already have answered. The pending
    /// entry stays so the eventual response can be recognised and dropped
    /// rather than delivered as an answer nobody is waiting for.
    pub fn cancel(&mut self, id: &RequestId) -> Option<Outgoing> {
        // A queued request has not been sent, so there is nothing for the
        // server to cancel; drop it here instead.
        if let Some(index) = self.queued.iter().position(|r| &r.id == id) {
            self.queued.remove(index);
            self.pending.remove(id);
            return None;
        }

        let pending = self.pending.get_mut(id)?;
        if pending.cancelled {
            return None;
        }
        pending.cancelled = true;
        Some(Outgoing(Message::Notification(Notification {
            method: "$/cancelRequest".into(),
            params: Some(serde_json::json!({ "id": id })),
        })))
    }

    /// Builds the `shutdown` request.
    pub fn shutdown(&mut self) -> Result<Outgoing, LspError> {
        if self.state != State::Ready {
            return Err(LspError::WrongState {
                action: "shut down",
                state: self.state,
            });
        }
        let id = self.allocate_id();
        self.state = State::ShuttingDown;
        self.shutdown_id = Some(id.clone());
        self.pending.insert(
            id.clone(),
            Pending {
                method: "shutdown".into(),
                cancelled: false,
            },
        );
        Ok(Outgoing(Message::Request(Request {
            id,
            method: "shutdown".into(),
            params: None,
        })))
    }

    /// Builds the `exit` notification.
    ///
    /// Only valid after `shutdown`. Sending it first tells a server to die
    /// before it has been asked to stop, so anything it was flushing is lost.
    pub fn exit(&mut self) -> Result<Outgoing, LspError> {
        if self.state != State::ShuttingDown {
            return Err(LspError::ExitWithoutShutdown);
        }
        self.state = State::Exited;
        self.pending.clear();
        self.queued.clear();
        Ok(Outgoing(Message::Notification(Notification {
            method: "exit".into(),
            params: None,
        })))
    }

    /// Feeds one message from the server.
    ///
    /// Returns what to send back and what the editor should act on. Both may be
    /// empty; neither is an error.
    pub fn handle(
        &mut self,
        message: Message,
    ) -> Result<(Vec<Outgoing>, Vec<ClientEvent>), LspError> {
        match message {
            Message::Response(response) => self.handle_response(response),
            Message::Notification(notification) => {
                Ok((Vec::new(), vec![self.classify_notification(notification)]))
            }
            Message::Request(request) => Ok(self.handle_server_request(request)),
        }
    }

    fn handle_response(
        &mut self,
        response: Response,
    ) -> Result<(Vec<Outgoing>, Vec<ClientEvent>), LspError> {
        let Some(pending) = self.pending.remove(&response.id) else {
            // A reply to something never asked, or asked twice. Not fatal: a
            // confused server should not be able to stop the editor.
            return Ok((
                Vec::new(),
                vec![ClientEvent::Ignored {
                    reason: format!("response to unknown request {}", response.id),
                }],
            ));
        };

        if Some(&response.id) == self.initialize_id.as_ref() {
            return self.finish_initialize(response);
        }

        if Some(&response.id) == self.shutdown_id.as_ref() {
            // The server has agreed to stop. `exit` is the caller's to send,
            // so that it can decide how long to wait.
            return Ok((Vec::new(), Vec::new()));
        }

        if pending.cancelled {
            // The answer arrived anyway, which is normal — cancellation races
            // the reply. Delivering it would repopulate a UI the user closed.
            return Ok((
                Vec::new(),
                vec![ClientEvent::Ignored {
                    reason: format!("response to cancelled request {}", response.id),
                }],
            ));
        }

        Ok((
            Vec::new(),
            vec![ClientEvent::Response {
                method: pending.method,
                id: response.id,
                result: response.result,
                error: response.error,
            }],
        ))
    }

    fn finish_initialize(
        &mut self,
        response: Response,
    ) -> Result<(Vec<Outgoing>, Vec<ClientEvent>), LspError> {
        if let Some(error) = response.error {
            self.state = State::Exited;
            return Ok((
                Vec::new(),
                vec![ClientEvent::Ignored {
                    reason: format!("server refused to initialize: {error}"),
                }],
            ));
        }

        let result = response.result.unwrap_or(serde_json::Value::Null);
        let server_caps = result
            .get("capabilities")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // Checked before anything else is believed: an encoding mismatch makes
        // every position in both directions wrong, so the session must not
        // start rather than start subtly broken.
        self.encoding = capabilities::negotiate_encoding(
            server_caps.get("positionEncoding").and_then(|v| v.as_str()),
        )?;
        self.capabilities = ServerCapabilities::from_json(&server_caps);
        self.state = State::Ready;

        let mut outgoing = vec![Outgoing(Message::Notification(Notification {
            method: "initialized".into(),
            params: Some(serde_json::json!({})),
        }))];
        // Anything raised while the handshake was in flight goes out now, in
        // the order it was raised.
        for request in std::mem::take(&mut self.queued) {
            outgoing.push(Outgoing(Message::Request(request)));
        }

        Ok((
            outgoing,
            vec![ClientEvent::Initialized {
                capabilities: Box::new(self.capabilities.clone()),
            }],
        ))
    }

    fn classify_notification(&self, notification: Notification) -> ClientEvent {
        let params = notification.params.clone().unwrap_or_default();
        match notification.method.as_str() {
            "window/showMessage" => ClientEvent::ShowMessage {
                kind: params.get("type").and_then(|v| v.as_i64()).unwrap_or(3),
                message: params
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            },
            "window/logMessage" => ClientEvent::LogMessage {
                kind: params.get("type").and_then(|v| v.as_i64()).unwrap_or(4),
                message: params
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            },
            _ => ClientEvent::Notification(notification),
        }
    }

    /// Answers a request from the server.
    ///
    /// Every one is answered. A server waiting on a reply that never comes is
    /// stuck, and several implementations wait synchronously — so an
    /// unimplemented method gets an explicit `MethodNotFound` rather than
    /// silence.
    fn handle_server_request(&mut self, request: Request) -> (Vec<Outgoing>, Vec<ClientEvent>) {
        let response = match request.method.as_str() {
            // Accepted with a null result: deco does not draw progress, but
            // refusing the token makes some servers skip work entirely.
            "window/workDoneProgress/create" => Response::ok(request.id, serde_json::Value::Null),
            // Dynamic registration is declined everywhere in the client
            // capabilities, so a server asking anyway gets a success it can
            // proceed from rather than an error it may treat as fatal.
            "client/registerCapability" | "client/unregisterCapability" => {
                Response::ok(request.id, serde_json::Value::Null)
            }
            // Nothing here can apply an edit yet. Saying so honestly is better
            // than "applied: true" for a refactoring that never happened.
            "workspace/applyEdit" => Response::ok(
                request.id,
                serde_json::json!({
                    "applied": false,
                    "failureReason": "deco cannot apply workspace edits yet",
                }),
            ),
            "workspace/configuration" => {
                // One null per requested item; the shape matters more than the
                // content, since a mismatched array length confuses servers.
                let count = request
                    .params
                    .as_ref()
                    .and_then(|p| p.get("items"))
                    .and_then(|items| items.as_array())
                    .map(Vec::len)
                    .unwrap_or(0);
                Response::ok(
                    request.id,
                    serde_json::Value::Array(vec![serde_json::Value::Null; count]),
                )
            }
            other => {
                let method = other.to_owned();
                return (
                    vec![Outgoing(Message::Response(Response::err(
                        request.id,
                        ErrorCode::MethodNotFound,
                        format!("deco does not implement {method}"),
                    )))],
                    vec![ClientEvent::Ignored {
                        reason: format!("server request {method} is not implemented"),
                    }],
                );
            }
        };

        (vec![Outgoing(Message::Response(response))], Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Drives a client to `Ready` with the capabilities a server would send.
    fn ready(capabilities: serde_json::Value) -> (Client, Vec<Outgoing>) {
        let mut client = Client::new();
        let Outgoing(Message::Request(init)) = client.initialize(None, None).unwrap() else {
            panic!("initialize is a request");
        };
        let (outgoing, _) = client
            .handle(Message::Response(Response::ok(
                init.id,
                json!({ "capabilities": capabilities }),
            )))
            .unwrap();
        (client, outgoing)
    }

    fn method_of(outgoing: &Outgoing) -> Option<&str> {
        outgoing.0.method()
    }

    #[test]
    fn initialize_advertises_deco_and_starts_the_handshake() {
        let mut client = Client::new();
        assert_eq!(client.state(), State::Uninitialized);

        let Outgoing(Message::Request(request)) = client.initialize(None, None).unwrap() else {
            panic!("initialize is a request");
        };
        assert_eq!(request.method, "initialize");
        assert_eq!(client.state(), State::Initializing);

        let params = request.params.unwrap();
        assert_eq!(params["clientInfo"]["name"], json!("deco"));
        assert_eq!(
            params["processId"],
            json!(null),
            "no reason to hand a subprocess our pid"
        );
        assert!(params["capabilities"].is_object());
    }

    #[test]
    fn initialize_carries_the_root_and_the_server_options() {
        let mut client = Client::new();
        let root = crate::uri::Uri::from_string("file:///w");
        let Outgoing(Message::Request(request)) = client
            .initialize(Some(&root), Some(json!({"cargo": {"features": "all"}})))
            .unwrap()
        else {
            panic!("initialize is a request");
        };
        let params = request.params.unwrap();
        assert_eq!(params["rootUri"], json!("file:///w"));
        assert_eq!(
            params["initializationOptions"]["cargo"]["features"],
            json!("all")
        );
    }

    #[test]
    fn initializing_twice_is_refused() {
        let mut client = Client::new();
        client.initialize(None, None).unwrap();
        assert!(matches!(
            client.initialize(None, None),
            Err(LspError::WrongState { .. })
        ));
    }

    #[test]
    fn the_handshake_completes_with_initialized_and_the_capabilities() {
        let (client, outgoing) = ready(json!({"hoverProvider": true}));

        assert_eq!(client.state(), State::Ready);
        assert_eq!(outgoing.len(), 1);
        assert_eq!(method_of(&outgoing[0]), Some("initialized"));
        assert!(client.capabilities().hover);
        assert_eq!(client.pending_count(), 0, "initialize is no longer pending");
    }

    #[test]
    fn a_request_raised_during_the_handshake_is_queued_and_then_flushed() {
        // The editor should not have to know whether the server has finished
        // starting before it can ask for a hover.
        let mut client = Client::new();
        let Outgoing(Message::Request(init)) = client.initialize(None, None).unwrap() else {
            panic!("initialize is a request");
        };

        let (id, sent) = client.request("textDocument/hover", json!({})).unwrap();
        assert!(sent.is_none(), "not sent before the handshake finished");
        assert_eq!(client.queued_count(), 1);

        let (outgoing, _) = client
            .handle(Message::Response(Response::ok(
                init.id,
                json!({"capabilities": {}}),
            )))
            .unwrap();

        assert_eq!(method_of(&outgoing[0]), Some("initialized"));
        assert_eq!(
            method_of(&outgoing[1]),
            Some("textDocument/hover"),
            "the queued request follows the handshake"
        );
        assert_eq!(client.queued_count(), 0);
        assert_eq!(client.pending_count(), 1);

        // And its id was usable all along.
        let Outgoing(Message::Request(flushed)) = &outgoing[1] else {
            panic!("expected a request");
        };
        assert_eq!(flushed.id, id);
    }

    #[test]
    fn queued_requests_keep_their_order() {
        let mut client = Client::new();
        let Outgoing(Message::Request(init)) = client.initialize(None, None).unwrap() else {
            panic!("initialize is a request");
        };
        for method in ["a", "b", "c"] {
            client.request(method, json!({})).unwrap();
        }
        let (outgoing, _) = client
            .handle(Message::Response(Response::ok(
                init.id,
                json!({"capabilities": {}}),
            )))
            .unwrap();
        let methods: Vec<_> = outgoing.iter().filter_map(method_of).collect();
        assert_eq!(methods, vec!["initialized", "a", "b", "c"]);
    }

    #[test]
    fn a_request_before_initialize_is_refused() {
        let mut client = Client::new();
        assert!(matches!(
            client.request("textDocument/hover", json!({})),
            Err(LspError::WrongState { .. })
        ));
    }

    #[test]
    fn a_notification_before_the_handshake_is_refused_rather_than_queued() {
        // Replaying `didChange` for a document the server was never told about
        // is a protocol violation the moment it lands.
        let mut client = Client::new();
        client.initialize(None, None).unwrap();
        assert!(matches!(
            client.notify("textDocument/didChange", json!({})),
            Err(LspError::WrongState { .. })
        ));
    }

    #[test]
    fn a_response_is_delivered_with_the_method_that_asked() {
        // A bare response carries only an id, so without the pending table the
        // editor cannot tell a hover from a completion.
        let (mut client, _) = ready(json!({}));
        let (id, sent) = client.request("textDocument/hover", json!({})).unwrap();
        assert!(sent.is_some());

        let (_, events) = client
            .handle(Message::Response(Response::ok(
                id.clone(),
                json!({"contents": "docs"}),
            )))
            .unwrap();

        assert_eq!(
            events,
            vec![ClientEvent::Response {
                method: "textDocument/hover".into(),
                id,
                result: Some(json!({"contents": "docs"})),
                error: None,
            }]
        );
        assert_eq!(client.pending_count(), 0);
    }

    #[test]
    fn ids_are_never_reused() {
        // A reused id routes a reply to the wrong request.
        let (mut client, _) = ready(json!({}));
        let mut seen = Vec::new();
        for _ in 0..5 {
            let (id, _) = client.request("textDocument/hover", json!({})).unwrap();
            assert!(!seen.contains(&id), "{id} was issued twice");
            seen.push(id);
        }
    }

    #[test]
    fn a_response_to_an_unknown_id_is_ignored_not_fatal() {
        let (mut client, _) = ready(json!({}));
        let (_, events) = client
            .handle(Message::Response(Response::ok(999.into(), json!(null))))
            .unwrap();
        assert!(matches!(events[0], ClientEvent::Ignored { .. }));
        assert_eq!(
            client.state(),
            State::Ready,
            "a confused server is survivable"
        );
    }

    #[test]
    fn a_cancelled_requests_reply_is_dropped_rather_than_delivered() {
        // Cancellation races the answer; delivering it repopulates a UI the
        // user already dismissed.
        let (mut client, _) = ready(json!({}));
        let (id, _) = client
            .request("textDocument/completion", json!({}))
            .unwrap();

        let cancel = client.cancel(&id).expect("a sent request can be cancelled");
        assert_eq!(method_of(&cancel), Some("$/cancelRequest"));

        let (_, events) = client
            .handle(Message::Response(Response::ok(id, json!({"items": []}))))
            .unwrap();
        assert!(matches!(events[0], ClientEvent::Ignored { .. }));
    }

    #[test]
    fn cancelling_twice_sends_one_notification() {
        let (mut client, _) = ready(json!({}));
        let (id, _) = client
            .request("textDocument/completion", json!({}))
            .unwrap();
        assert!(client.cancel(&id).is_some());
        assert!(client.cancel(&id).is_none(), "already cancelled");
    }

    #[test]
    fn cancelling_a_queued_request_sends_nothing_and_drops_it() {
        // The server was never told about it, so there is nothing to cancel —
        // and flushing it after the user moved on would be pure waste.
        let mut client = Client::new();
        let Outgoing(Message::Request(init)) = client.initialize(None, None).unwrap() else {
            panic!("initialize is a request");
        };
        let (id, _) = client.request("textDocument/hover", json!({})).unwrap();
        assert!(client.cancel(&id).is_none());
        assert_eq!(client.queued_count(), 0);

        let (outgoing, _) = client
            .handle(Message::Response(Response::ok(
                init.id,
                json!({"capabilities": {}}),
            )))
            .unwrap();
        assert_eq!(outgoing.len(), 1, "only `initialized` goes out");
    }

    #[test]
    fn cancelling_an_unknown_id_is_harmless() {
        let (mut client, _) = ready(json!({}));
        assert!(client.cancel(&RequestId::Number(42)).is_none());
    }

    #[test]
    fn shutdown_then_exit_is_the_only_accepted_order() {
        let (mut client, _) = ready(json!({}));

        // exit first is refused: the server would die before it can clean up.
        assert_eq!(client.exit(), Err(LspError::ExitWithoutShutdown));

        let shutdown = client.shutdown().unwrap();
        assert_eq!(method_of(&shutdown), Some("shutdown"));
        assert_eq!(client.state(), State::ShuttingDown);

        let exit = client.exit().unwrap();
        assert_eq!(method_of(&exit), Some("exit"));
        assert_eq!(client.state(), State::Exited);
    }

    #[test]
    fn nothing_can_be_sent_after_shutdown_begins() {
        let (mut client, _) = ready(json!({}));
        client.shutdown().unwrap();
        assert!(client.request("textDocument/hover", json!({})).is_err());
        assert!(client.notify("textDocument/didChange", json!({})).is_err());
        assert!(client.shutdown().is_err());
    }

    #[test]
    fn the_shutdown_reply_does_not_send_exit_by_itself() {
        // Leaving `exit` to the caller is what lets it decide how long to wait
        // for the server to finish flushing.
        let (mut client, _) = ready(json!({}));
        let Outgoing(Message::Request(request)) = client.shutdown().unwrap() else {
            panic!("shutdown is a request");
        };
        let (outgoing, events) = client
            .handle(Message::Response(Response::ok(request.id, json!(null))))
            .unwrap();
        assert!(outgoing.is_empty());
        assert!(events.is_empty());
        assert_eq!(client.state(), State::ShuttingDown);
    }

    #[test]
    fn an_encoding_the_client_never_offered_ends_the_handshake() {
        // Accepting it would misplace every position on any line containing a
        // character outside the Basic Multilingual Plane.
        let mut client = Client::new();
        let Outgoing(Message::Request(init)) = client.initialize(None, None).unwrap() else {
            panic!("initialize is a request");
        };
        let result = client.handle(Message::Response(Response::ok(
            init.id,
            json!({"capabilities": {"positionEncoding": "utf-8"}}),
        )));
        assert!(matches!(result, Err(LspError::Negotiation(_))));
    }

    #[test]
    fn a_server_that_refuses_to_initialize_ends_the_session() {
        let mut client = Client::new();
        let Outgoing(Message::Request(init)) = client.initialize(None, None).unwrap() else {
            panic!("initialize is a request");
        };
        let (outgoing, events) = client
            .handle(Message::Response(Response::err(
                init.id,
                ErrorCode::InternalError,
                "no workspace",
            )))
            .unwrap();

        assert!(outgoing.is_empty(), "no `initialized` after a refusal");
        assert!(matches!(events[0], ClientEvent::Ignored { .. }));
        assert_eq!(client.state(), State::Exited);
    }

    #[test]
    fn every_server_request_is_answered() {
        // A server waiting on a reply that never comes is stuck, and several
        // wait synchronously.
        let (mut client, _) = ready(json!({}));
        for method in [
            "window/workDoneProgress/create",
            "client/registerCapability",
            "client/unregisterCapability",
            "workspace/applyEdit",
            "workspace/configuration",
            "something/nobodyImplements",
        ] {
            let (outgoing, _) = client
                .handle(Message::Request(Request {
                    id: 100.into(),
                    method: method.into(),
                    params: Some(json!({})),
                }))
                .unwrap();
            assert_eq!(outgoing.len(), 1, "{method} went unanswered");
            assert!(
                matches!(outgoing[0].0, Message::Response(_)),
                "{method} was not answered with a response"
            );
        }
    }

    #[test]
    fn an_unimplemented_server_request_gets_method_not_found() {
        let (mut client, _) = ready(json!({}));
        let (outgoing, events) = client
            .handle(Message::Request(Request {
                id: 5.into(),
                method: "window/showDocument".into(),
                params: None,
            }))
            .unwrap();

        let Outgoing(Message::Response(response)) = &outgoing[0] else {
            panic!("expected a response");
        };
        assert_eq!(
            response.error.as_ref().unwrap().code,
            ErrorCode::MethodNotFound as i64
        );
        assert!(matches!(events[0], ClientEvent::Ignored { .. }));
    }

    #[test]
    fn an_edit_the_editor_cannot_apply_is_reported_as_not_applied() {
        // "applied: true" for a refactoring that never happened would leave the
        // server believing the file changed.
        let (mut client, _) = ready(json!({}));
        let (outgoing, _) = client
            .handle(Message::Request(Request {
                id: 5.into(),
                method: "workspace/applyEdit".into(),
                params: Some(json!({"edit": {}})),
            }))
            .unwrap();

        let Outgoing(Message::Response(response)) = &outgoing[0] else {
            panic!("expected a response");
        };
        assert_eq!(response.result.as_ref().unwrap()["applied"], json!(false));
    }

    #[test]
    fn a_configuration_request_is_answered_with_one_entry_per_item() {
        // A mismatched array length is what confuses servers here, more than
        // the values themselves.
        let (mut client, _) = ready(json!({}));
        let (outgoing, _) = client
            .handle(Message::Request(Request {
                id: 5.into(),
                method: "workspace/configuration".into(),
                params: Some(json!({"items": [{"section": "a"}, {"section": "b"}]})),
            }))
            .unwrap();

        let Outgoing(Message::Response(response)) = &outgoing[0] else {
            panic!("expected a response");
        };
        assert_eq!(response.result.as_ref().unwrap(), &json!([null, null]));
    }

    #[test]
    fn window_messages_are_classified_apart_from_other_notifications() {
        let (mut client, _) = ready(json!({}));

        let (_, events) = client
            .handle(Message::Notification(Notification {
                method: "window/showMessage".into(),
                params: Some(json!({"type": 1, "message": "boom"})),
            }))
            .unwrap();
        assert_eq!(
            events[0],
            ClientEvent::ShowMessage {
                kind: 1,
                message: "boom".into()
            }
        );

        let (_, events) = client
            .handle(Message::Notification(Notification {
                method: "window/logMessage".into(),
                params: Some(json!({"type": 4, "message": "detail"})),
            }))
            .unwrap();
        assert!(matches!(events[0], ClientEvent::LogMessage { .. }));

        let (_, events) = client
            .handle(Message::Notification(Notification {
                method: "textDocument/publishDiagnostics".into(),
                params: Some(json!({"uri": "file:///a", "diagnostics": []})),
            }))
            .unwrap();
        let ClientEvent::Notification(notification) = &events[0] else {
            panic!("diagnostics pass through as a notification");
        };
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
    }

    #[test]
    fn the_negotiated_encoding_defaults_to_utf16() {
        let (client, _) = ready(json!({}));
        assert_eq!(client.position_encoding(), PositionEncoding::Utf16);
    }

    #[test]
    fn a_full_session_runs_end_to_end() {
        let (mut client, _) = ready(json!({
            "textDocumentSync": {"openClose": true, "change": 2},
            "hoverProvider": true,
        }));

        assert_eq!(
            client.capabilities().sync_kind,
            crate::capabilities::TextDocumentSyncKind::Incremental
        );

        client
            .notify("textDocument/didOpen", json!({"textDocument": {}}))
            .unwrap();
        let (id, sent) = client.request("textDocument/hover", json!({})).unwrap();
        assert!(sent.is_some());
        client
            .handle(Message::Response(Response::ok(id, json!(null))))
            .unwrap();

        client.shutdown().unwrap();
        client.exit().unwrap();
        assert_eq!(client.state(), State::Exited);
        assert_eq!(client.pending_count(), 0);
    }
}
