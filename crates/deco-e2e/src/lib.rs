//! End-to-end tests that drive deco as a user does.
//!
//! The other tests in this repository are unit tests: each builds one struct,
//! calls one function and asserts on the return value. They cover most of the
//! codebase, but they do not detect failures in how the parts are connected.
//! Examples are a `settings.json` read from the wrong directory, a keybinding
//! that resolves but does not reach the command, a file saved to the wrong
//! path, or a stale status line on screen. In these cases each part works but
//! the editor does not.
//!
//! A scenario is therefore built from the same inputs a user has:
//!
//! - **A real configuration directory.** [`Scenario::user_settings`] and similar
//!   methods write JSON to a temporary home in the platform's real layout, and
//!   the session is built by [`deco::startup::session`], the same call the
//!   binary makes. No test receives a pre-built [`deco_config::Settings`].
//! - **A real workspace.** [`Scenario::file`] writes files to disk. Quick open
//!   lists them, search in files searches them, and saving overwrites them.
//! - **Real keystrokes.** [`Editor::press`] builds a crossterm [`KeyEvent`] and
//!   passes it to [`deco_tui::keys::chord_from_event`] and then to
//!   [`deco_tui::Driver`], which is the editor's event loop without the
//!   terminal. A scenario can run a command only by pressing its bound keys.
//! - **A real screen.** [`Editor::screen`] renders a frame at the scenario's
//!   terminal size, and assertions check the characters in it, so the result
//!   must be visible on screen.
//!
//! Limitations: there is no terminal, so these tests do not verify that
//! crossterm writes what is queued. There is no language server unless a
//! scenario provides one. The process environment is never modified, because
//! all test threads share it. Values that would otherwise come from the
//! environment (home, the platform's configuration layout, which platform's
//! keybindings apply, the working directory) are fields of [`Scenario`].
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
