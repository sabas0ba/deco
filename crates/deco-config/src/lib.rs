//! VS Code-compatible configuration for deco.
//!
//! Three layers, smallest to largest:
//!
//! 1. [`jsonc`] reads the JSON-with-comments dialect every VS Code
//!    configuration file uses.
//! 2. [`settings`] stacks those documents into scopes (default → user → remote
//!    → workspace → folder) and resolves a key against them, honouring
//!    `[language]` override sections.
//! 3. [`editor`] projects the result into typed fields for the hot paths.
//!
//! [`glob`] implements the small glob dialect `files.exclude` and an extension's
//! `workspaceContains` are written in.
//!
//! The goal is that an existing `settings.json` can be dropped in unchanged and
//! mean the same thing it does in VS Code.

pub mod defaults;
pub mod editor;
pub mod glob;
pub mod indent;
pub mod jsonc;
pub mod paths;
pub mod settings;

pub use editor::{
    CursorStyle, EditorSettings, EolSetting, LineNumbers, RenderWhitespace, WordWrap,
    WrappingIndent,
};
pub use jsonc::{parse as parse_jsonc, JsoncError};
pub use settings::{Scope, Settings, SettingsError};
