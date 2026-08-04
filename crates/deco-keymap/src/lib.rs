//! VS Code-compatible keybindings for deco.
//!
//! ```
//! use deco_keymap::{binding::Platform, keys::Chord, resolver, when::ContextKeys, Resolution};
//!
//! let user = r#"[{ "key": "ctrl+alt+t", "command": "my.command" }]"#;
//! let (keymap, problems) = resolver::build(Platform::Linux, Some(user));
//! assert!(problems.is_empty());
//!
//! let mut state = resolver::ChordState::new();
//! let ctx = ContextKeys::with_platform_defaults();
//! let resolved = keymap.resolve(&mut state, Chord::parse("ctrl+alt+t").unwrap(), &ctx);
//! assert_eq!(resolved, Resolution::Match { command: "my.command".into(), args: None });
//! ```
//!
//! The pieces:
//!
//! - [`keys`] parses `keybindings.json` key spellings into chords.
//! - [`when`] parses and evaluates `when` clauses against a [`when::ContextKeys`]
//!   store.
//! - [`binding`] reads a `keybindings.json` document entry by entry, so one bad
//!   line does not cost the user the rest of the file.
//! - [`resolver`] stacks the defaults and the user's bindings and turns a
//!   keypress into a command, including two-chord sequences.
//! - [`defaults`] is deco's built-in keymap, written in the same format.

pub mod binding;
pub mod defaults;
pub mod keys;
pub mod resolver;
pub mod when;

pub use binding::{Keybinding, Platform, Rule, Source};
pub use keys::{Chord, Key, KeySequence, Modifiers, NamedKey};
pub use resolver::{ChordState, Keymap, Resolution};
pub use when::{ContextKeys, WhenExpr};
