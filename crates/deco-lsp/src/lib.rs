//! A Language Server Protocol client.
//!
//! This crate is the editor's side of a conversation with a language server:
//! what to say, in what order, and what to make of the answers. It deliberately
//! does not start processes, own threads or perform I/O beyond reading and
//! writing a stream someone else hands it — the protocol is a state machine,
//! and a state machine that does not spawn anything can be driven entirely from
//! tests.
//!
//! The pieces, roughly in the order a session uses them:
//!
//! - [`mod@uri`] — paths to `file:` URIs and back, spelled the way VS Code
//!   spells them, because the servers people run were tested against VS Code.
//! - [`mod@jsonrpc`] — JSON-RPC 2.0 and its `Content-Length` framing.
//! - [`mod@capabilities`] — what the editor claims it can do, what the server
//!   answers, and the position-encoding negotiation that keeps every subsequent
//!   coordinate meaningful.
//! - [`mod@server`] — which server to run for a language, as an argument
//!   vector rather than a shell string.
//! - [`mod@settings`] — reading those definitions out of layered settings while
//!   keeping track of which layer each came from, because a definition from a
//!   cloned repository must not be run unasked.
//! - [`mod@sync`] — keeping the server's copy of a document identical to the
//!   editor's.
//! - [`mod@requests`] — building the language-feature requests and reading the
//!   several shapes each answer can arrive in.
//! - [`mod@process`] — spawning that server and moving bytes to and from it.
//!   The one module here that owns a process and threads.
//! - [`mod@supervisor`] — all of the above driven end to end, which is the
//!   layer a frontend actually uses.
//! - [`mod@diagnostics`] — the errors a server pushes, and deciding which of
//!   them still apply.
//! - [`mod@client`] — the session lifecycle, request routing and cancellation.
//!
//! # Nothing here trusts the server
//!
//! A language server is a program the user installed, usually from a package
//! registry, running with their privileges — it is not part of the editor. So
//! frames are size-limited before they are allocated, malformed messages are
//! named errors rather than panics, a server that answers a question nobody
//! asked is ignored, and a server that picks a position encoding the client did
//! not offer is refused outright rather than silently misplacing every edit.

#![deny(missing_docs)]

pub mod capabilities;
pub mod client;
pub mod diagnostics;
pub mod jsonrpc;
pub mod process;
pub mod requests;
pub mod server;
pub mod settings;
pub mod supervisor;
pub mod sync;
pub mod uri;

pub use capabilities::{PositionEncoding, ServerCapabilities, TextDocumentSyncKind};
pub use client::{Client, ClientEvent, LspError, Outgoing};
pub use diagnostics::{Diagnostic, DiagnosticStore, Severity};
pub use jsonrpc::{Message, Notification, Request, RequestId, Response};
pub use process::{Consent, ServerProcess, SpawnError};
pub use requests::{FormattingOptions, Hover, Location, TextEdit};
pub use server::{ServerConfig, ServerRegistry, Trust};
pub use settings::{ENABLED_KEY, SERVERS_KEY};
pub use supervisor::{Supervisor, SupervisorError, Update};
pub use sync::{ContentChange, DocumentSync};
pub use uri::{PathStyle, Uri};
