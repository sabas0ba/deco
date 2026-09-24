//! A Language Server Protocol client.
//!
//! This crate implements the editor side of the protocol: which messages to
//! send, in what order, and how to interpret the responses. Apart from
//! [`mod@process`], it does not start processes, own threads or perform I/O
//! beyond reading and writing a stream supplied by the caller. The protocol
//! logic is a state machine, so tests can drive it without spawning anything.
//!
//! The modules, roughly in the order a session uses them:
//!
//! - [`mod@uri`] — converts paths to `file:` URIs and back, using the same
//!   spelling as VS Code, because language servers are tested against VS Code.
//! - [`mod@jsonrpc`] — JSON-RPC 2.0 and its `Content-Length` framing.
//! - [`mod@capabilities`] — the client capabilities, the server's response,
//!   and position-encoding negotiation, which determines how every later
//!   coordinate is interpreted.
//! - [`mod@server`] — which server to run for a language, as an argument
//!   vector rather than a shell string.
//! - [`mod@settings`] — reads those definitions from layered settings and
//!   records which layer each came from. A definition from a cloned repository
//!   must not run without the user's consent.
//! - [`mod@sync`] — keeps the server's copy of a document identical to the
//!   editor's.
//! - [`mod@requests`] — builds language-feature requests and parses the
//!   different response shapes each one can have.
//! - [`mod@process`] — spawns the server and transfers bytes to and from it.
//!   This is the only module that owns a process and threads.
//! - [`mod@supervisor`] — combines the modules above end to end. This is the
//!   layer a frontend uses.
//! - [`mod@diagnostics`] — diagnostics published by a server, and which of
//!   them still apply.
//! - [`mod@client`] — the session lifecycle, request routing and cancellation.
//!
//! # Nothing here trusts the server
//!
//! A language server is a program the user installed, usually from a package
//! registry, and it runs with the user's privileges. It is not part of the
//! editor. Therefore frame sizes are checked before allocation, malformed
//! messages produce named errors instead of panics, responses to unknown
//! requests are ignored, and a server that selects a position encoding the
//! client did not offer is rejected, because every edit would otherwise be
//! applied at the wrong position.

#![deny(missing_docs)]

pub mod capabilities;
pub mod client;
pub mod diagnostics;
pub mod jsonrpc;
pub mod process;
pub mod requests;
pub mod server;
pub mod settings;
pub mod snippet;
pub mod supervisor;
pub mod sync;
pub mod uri;

pub use capabilities::{
    CodeActionOptions, PositionEncoding, ServerCapabilities, TextDocumentSyncKind,
};
pub use client::{Client, ClientEvent, LspError, Outgoing};
pub use diagnostics::{Diagnostic, DiagnosticStore, Severity};
pub use jsonrpc::{Message, Notification, Request, RequestId, Response};
pub use process::{Consent, ServerProcess, SpawnError};
pub use requests::{
    CodeAction, DocumentEdits, FormattingOptions, Hover, Location, TextEdit, WorkspaceEdit,
    WorkspaceEditError,
};
pub use server::{ServerConfig, ServerRegistry, Trust};
pub use settings::{ENABLED_KEY, SERVERS_KEY};
pub use supervisor::{Supervisor, SupervisorError, Update};
pub use sync::{ContentChange, DocumentSync};
pub use uri::{PathStyle, Uri};
