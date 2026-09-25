//! Remote development for deco: SSH, WSL and containers.
//!
//! ```
//! use deco_remote::{command_for, Authority, TransportOptions};
//!
//! let (authority, path) =
//!     Authority::parse_uri("vscode-remote://ssh-remote+myhost/home/u/main.rs").unwrap();
//! assert_eq!(path, "/home/u/main.rs");
//!
//! let command = command_for(
//!     &authority,
//!     &deco_remote::server_command("deco", Some("/home/u")),
//!     &TransportOptions::default(),
//! )
//! .unwrap();
//! assert_eq!(command.program, "ssh");
//! ```
//!
//! The architecture follows VS Code: a headless server runs on the remote and
//! the frontend runs locally. Editor state is kept where the files are.
//!
//! - [`authority`] parses `ssh-remote+host`, `wsl+Distro` and
//!   `dev-container+id`, plus the `vscode-remote://` URIs they appear in.
//! - [`transport`] turns an authority into a command. Commands are argument
//!   vectors, never shell strings. A hostname can come from an untrusted URI, so
//!   a host such as `-oProxyCommand=…` is rejected rather than escaped. Over
//!   SSH, the remote arguments are single-quoted for the remote login shell,
//!   which must be POSIX-compatible.
//! - [`frame`] is the length-prefixed JSON framing used by client and server. A
//!   size limit prevents a hostile peer from requesting a 900GB allocation on
//!   the local machine.
//!
//! - [`forward`] reaches a port on the remote. It tunnels through the remote
//!   deco, so it works over every transport, not only SSH.
//! - [`install`] installs deco on a remote that has none. It runs only when
//!   requested, only when the binary can run there, and never replaces a file
//!   that is not deco.
//! - [`server`] is the remote side: `deco --server --stdio`, which answers those
//!   frames and confines all paths to one root directory.
//! - [`client`] is the local side: it starts the transport's command and calls
//!   the server's methods.
//!
//! Git status and writes also run through the server. Language servers use the
//! same transport directly. Running extension hosts on the remote is not
//! implemented yet.

pub mod authority;
pub mod client;
pub mod fetch;
pub mod forward;
pub mod frame;
pub mod install;
pub mod server;
pub mod transport;

pub use authority::{Authority, AuthorityError};
pub use client::{Client, ClientError, Match, Search};
pub use forward::{Forward, ForwardError, PortSpec, PortSpecError};
pub use frame::{Message, MAX_FRAME_BYTES};
pub use install::{InstallError, Installed, Runner, TransportRunner};
pub use server::{Server, ServerError};
pub use transport::{command_for, server_command, Command, TransportError, TransportOptions};
