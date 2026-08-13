//! Extensions for deco: manifests, activation, and a capability model that
//! replaces VS Code's ambient authority.
//!
//! VS Code extensions are JavaScript, so deco runs them in a Node process — but
//! not a privileged one. The host starts with no filesystem, network or
//! process access; every privileged operation is an RPC that deco checks
//! against what the extension's manifest declared and what the user agreed to.
//!
//! ```
//! use deco_ext::capability::{Broker, Capability, CheckResult, DefaultPolicy,
//!     GrantStore, PathScope, ResolutionContext};
//! use std::path::PathBuf;
//!
//! let context = ResolutionContext {
//!     workspace_roots: vec![PathBuf::from("/home/u/project")],
//!     ..Default::default()
//! };
//! let broker = Broker::new(
//!     vec![Capability::ReadFile { scope: PathScope::Workspace }],
//!     GrantStore::default(),
//!     DefaultPolicy::Allow,
//!     context,
//! );
//!
//! // Inside the workspace: fine.
//! assert_eq!(
//!     broker.check_resolved_path(false, std::path::Path::new("/home/u/project/src/main.rs")),
//!     CheckResult::Allowed
//! );
//! // The classic target, reached by walking out of it: refused.
//! assert!(matches!(
//!     broker.check_resolved_path(false, std::path::Path::new("/home/u/project/../.ssh/id_ed25519")),
//!     CheckResult::Denied { .. }
//! ));
//! ```
//!
//! The modules:
//!
//! - [`capability`] — the model itself: deny by default, manifest declaration
//!   as a ceiling, scopes checked on resolved paths.
//! - [`protocol`] — the host wire format, and the method-to-capability table
//!   that fails closed on anything it does not recognise.
//! - [`manifest`] — `package.json` and its contribution points.
//! - [`activation`] — when an extension is allowed to start at all.
//! - [`host`] — the Node command line, built with a scrubbed environment and
//!   Node's own permission model.
//! - [`connection`] — starting that command line and talking to it, with
//!   [`connection::dispatch`] as the one way an inbound request reaches the editor.

pub mod activation;
pub mod capability;
pub mod connection;
pub mod host;
pub mod manifest;
pub mod protocol;

pub use capability::{
    Broker, Capability, CheckResult, Decision, DefaultPolicy, DenyReason, GrantStore, PathScope,
    ResolutionContext,
};
pub use manifest::{DeclarationSource, Manifest, ManifestError};
pub use protocol::{ErrorCode, Message, Notification, Request, Response};
