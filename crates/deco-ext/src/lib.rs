//! Extensions for deco: manifests, activation, and a capability model. In VS
//! Code, an extension has every permission of the extension host process; in
//! deco, each privileged operation needs a capability.
//!
//! VS Code extensions are JavaScript, so deco runs them in an unprivileged Node
//! process. The host starts with no filesystem, network or process access. Every
//! privileged operation is an RPC that deco checks against the extension's
//! manifest declarations and the user's grants.
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
//! - [`protocol`] — the host wire format, and the method-to-capability table.
//!   Unrecognised methods are denied.
//! - [`manifest`] — `package.json` and its contribution points.
//! - [`activation`] — when an extension is allowed to start.
//! - [`host`] — the Node command line, built with a scrubbed environment and
//!   Node's permission model.
//! - [`connection`] — starting that command line and exchanging messages with it.
//!   [`connection::dispatch`] is the only path from an inbound request to the editor.

pub mod activation;
pub mod capability;
pub mod catalogue;
pub mod connection;
pub mod host;
pub mod manifest;
pub mod permissions;
pub mod protocol;
pub mod sandbox;

pub use capability::{
    Broker, Capability, CheckResult, Decision, DefaultPolicy, DenyReason, GrantStore, PathScope,
    ResolutionContext,
};
pub use manifest::{DeclarationSource, Manifest, ManifestError};
pub use protocol::{ErrorCode, Message, Notification, Request, Response};
