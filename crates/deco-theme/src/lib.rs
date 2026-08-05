//! VS Code colour themes for deco.
//!
//! ```
//! use deco_theme::{defaults, ColorTheme};
//!
//! let theme = defaults::builtin("Default Dark Modern").unwrap();
//! let style = theme.style_for_scopes(&["source.rust", "keyword.control.rust"]);
//! assert!(style.foreground.is_some());
//! assert!(theme.color("editor.background").is_some());
//! ```
//!
//! A theme file is JSONC with `colors` (workbench colours), `tokenColors`
//! (TextMate rules) and `semanticTokenColors`, optionally chained through
//! `include`. Both matching models are implemented here:
//!
//! - [`tokens`] scores TextMate selectors the way vscode-textmate does — the
//!   depth of the final scope pattern dominates, ancestor count breaks ties —
//!   and every matching rule is layered in ascending specificity so a broad
//!   `fontStyle` rule and a narrow `foreground` rule combine.
//! - [`semantic`] scores `type.modifier:language` selectors with type and
//!   modifiers weighted equally and additively, as VS Code does.
//!
//! Themes in the wild define only a fraction of the workbench colours, so
//! [`ColorTheme::color`] falls back through VS Code's derivation chain and then
//! to a built-in table before giving up.
//!
//! Not supported: `.tmTheme` (plist) token colours, and `-` exclusion in scope
//! selectors. Both are rare in published VS Code themes.

pub mod color;
pub mod defaults;
pub mod semantic;
pub mod theme;
pub mod tokens;

pub use color::{ColorParseError, Rgba};
pub use semantic::{SemanticSelector, SemanticToken};
pub use theme::{ColorTheme, ThemeError, ThemeKind};
pub use tokens::{FontStyle, ScopeSelector, TokenColorRule, TokenStyle};
