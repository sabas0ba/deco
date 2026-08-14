//! Driving deco the way it is used.
//!
//! Every other test in this repository is a unit test: it builds the one struct
//! it is about, calls the one function it is about, and asserts on the return
//! value. That is where most of the confidence in this codebase comes from, and
//! it is also what those tests cannot say — because the way an editor breaks in
//! practice is rarely one function returning the wrong value. It is a
//! `settings.json` that was read from the wrong directory, a keybinding that
//! resolved but never reached the command, a file that saved to a path nobody
//! meant, a screen that shows yesterday's status line. Each of the parts is
//! right and the editor is wrong.
//!
//! So a scenario here is deliberately built out of the things a user actually
//! has:
//!
//! - **A real configuration directory.** [`Scenario::user_settings`] and friends
//!   write JSON to a temporary home in the layout the platform really uses, and
//!   the session is built by [`deco::startup::session`] — the same call the
//!   binary makes. Nothing is handed a pre-built [`deco_config::Settings`].
//! - **A real workspace.** [`Scenario::file`] writes files to disk. Quick open
//!   walks them, search-in-files greps them, and saving overwrites them.
//! - **Real keystrokes.** [`Editor::press`] builds a crossterm [`KeyEvent`] and
//!   feeds it to [`deco_tui::keys::chord_from_event`] and then to
//!   [`deco_tui::Driver`], which is the editor's event loop with the terminal
//!   taken out of it. A scenario cannot reach a command except by pressing the
//!   keys that are bound to it.
//! - **A real screen.** [`Editor::screen`] renders a frame at the terminal size
//!   the scenario asked for and asserts against the characters in it, so "the
//!   editor did the right thing" has to be visible.
//!
//! What is left out is stated rather than hidden: there is no terminal, so
//! nothing here proves that crossterm writes what it is queued; there is no
//! language server unless a scenario provides one; and the process environment
//! is never touched, because it is shared by every test thread. Everything that
//! would otherwise come from the environment — home, the platform's
//! configuration layout, which platform's keybindings win, the working directory
//! — is data on [`Scenario`].
//!
//! ```no_run
//! use deco_e2e::Scenario;
//!
//! let mut editor = Scenario::new("readme")
//!     .user_settings(r#"{ "editor.tabSize": 2, "editor.insertSpaces": true }"#)
//!     .file("src/main.rs", "fn main() {}\n")
//!     .launch(&["src/main.rs"]);
//!
//! editor.press("ctrl+end");
//! editor.press("enter");
//! editor.press("tab");
//! editor.type_text("// hi");
//! editor.press("ctrl+s");
//!
//! assert!(editor.on_disk("src/main.rs").ends_with("  // hi\n"));
//! editor.screen().assert_shows("main.rs");
//! ```
//!
//! [`KeyEvent`]: crossterm::event::KeyEvent

mod editor;
mod screen;
mod world;

pub use editor::Editor;
pub use screen::Screen;
pub use world::Scenario;
