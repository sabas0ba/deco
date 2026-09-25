//! deco's startup, as a library.
//!
//! The binary in `src/main.rs` is a thin wrapper over this: it reads the process
//! environment, calls [`startup::session`], and hands the result to a frontend.
//! Everything that determines the editor's state when its first frame is drawn
//! lives here: which settings files were read and in which order, which
//! keybindings took precedence, which files were opened and which one is shown.
//! A test can therefore run it against a directory it created, not only against
//! the machine deco is installed on.
//!
//! ```no_run
//! use deco::startup::{self, Boot};
//!
//! let cli = match deco::cli::parse(["notes.md"]).unwrap() {
//!     deco::cli::Outcome::Run(cli) => *cli,
//!     _ => return,
//! };
//! let boot = Boot::from_process();
//! // `None`: a local session has no remote machine settings to layer in.
//! let mut session = startup::session(&cli, &boot, None);
//! startup::open_local(&mut session, &cli.files, &boot).unwrap();
//! ```

pub mod cli;
pub mod config;
pub mod startup;
