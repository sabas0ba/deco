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
//!     TransportOptions::default(),
//! )
//! .unwrap();
//! assert_eq!(command.program, "ssh");
//! ```
//!
//! The shape is VS Code's: a headless server runs on the remote and the
//! frontend runs locally, with the editor's state living wherever the files do.
//!
//! - [`authority`] parses `ssh-remote+host`, `wsl+Distro` and
//!   `dev-container+id`, plus the `vscode-remote://` URIs they appear in.
//! - [`transport`] turns an authority into a command. Every one is an argument
//!   vector, never a shell string: a hostname can come from a URI someone else
//!   wrote, and a host of `-oProxyCommand=…` is rejected rather than escaped.
//! - [`frame`] is the length-prefixed JSON framing the two ends speak, with a
//!   size ceiling so a hostile peer cannot ask the local machine for 900GB.
//!
//! - [`forward`] reaches a port on the remote, using the remote's own deco as
//!   the tunnel so that it works over every transport rather than only SSH.
//! - [`install`] is what puts a deco on a remote that has none — only when
//!   asked, only when it can run there, and never over something that is not
//!   deco.
//! - [`server`] is the far end: `deco --server --stdio`, answering those frames
//!   against one directory it cannot be talked out of.
//! - [`client`] is the near end: it starts the transport's command and calls the
//!   server's methods.
//!
//! What is not here yet: running language servers or extensions over there.

pub mod authority;
pub mod client;
pub mod forward;
pub mod frame;
pub mod install;
pub mod server;
pub mod transport;

pub use authority::{Authority, AuthorityError};
pub use client::{Client, ClientError};
pub use forward::{Forward, ForwardError, PortSpec, PortSpecError};
pub use frame::{Message, MAX_FRAME_BYTES};
pub use install::{InstallError, Installed, Runner, TransportRunner};
pub use server::{Server, ServerError};
pub use transport::{command_for, server_command, Command, TransportError, TransportOptions};
