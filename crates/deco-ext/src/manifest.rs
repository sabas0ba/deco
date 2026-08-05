//! Extension manifests (`package.json`) and their contribution points.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::Capability;

/// A parsed extension manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// The unqualified extension name.
    pub name: String,
    /// The publisher id. Absent in unpublished local extensions.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Semver version string.
    #[serde(default)]
    pub version: String,
    /// Human-readable name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Short description.
    #[serde(default)]
    pub description: Option<String>,
    /// Entry point for the Node extension host.
    #[serde(default)]
    pub main: Option<String>,
    /// Events that cause the extension to be activated.
    #[serde(default)]
    pub activation_events: Vec<String>,
    /// Engine constraints, e.g. `{"vscode": "^1.75.0"}`.
    #[serde(default)]
    pub engines: std::collections::BTreeMap<String, String>,
    /// Everything the extension contributes to the editor.
    #[serde(default)]
    pub contributes: Contributes,
    /// deco-specific manifest section, ignored by VS Code.
    #[serde(default)]
    pub deco: Option<DecoSection>,
}

/// The `deco` section of a manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecoSection {
    /// The capabilities the extension asks for.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

/// The `contributes` block.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contributes {
    /// Commands the extension registers.
    #[serde(default)]
    pub commands: Vec<CommandContribution>,
    /// Default keybindings for those commands.
    #[serde(default)]
    pub keybindings: Vec<Value>,
    /// Languages the extension defines.
    #[serde(default)]
    pub languages: Vec<LanguageContribution>,
    /// TextMate grammars.
    #[serde(default)]
    pub grammars: Vec<GrammarContribution>,
    /// Colour themes.
    #[serde(default)]
    pub themes: Vec<ThemeContribution>,
    /// Settings the extension defines. Kept as raw JSON because the schema is
    /// open-ended and deco only needs the defaults out of it.
    #[serde(default)]
    pub configuration: Option<Value>,
}

/// A `contributes.commands` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct CommandContribution {
    /// The command id.
    pub command: String,
    /// The label shown in the command palette.
    pub title: String,
    /// An optional grouping prefix.
    #[serde(default)]
    pub category: Option<String>,
    /// When the command is available.
    #[serde(default)]
    pub when: Option<String>,
}

/// A `contributes.languages` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct LanguageContribution {
    /// The language id, e.g. `rust`.
    pub id: String,
    /// File extensions, including the dot.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Exact filenames, e.g. `Makefile`.
    #[serde(default)]
    pub filenames: Vec<String>,
    /// Display names.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Path to a language-configuration JSON file.
    #[serde(default)]
    pub configuration: Option<String>,
}

/// A `contributes.grammars` entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrammarContribution {
    /// The language this grammar tokenises.
    #[serde(default)]
    pub language: Option<String>,
    /// The grammar's root scope name.
    pub scope_name: String,
    /// Path to the grammar file, relative to the extension root.
    pub path: String,
}

/// A `contributes.themes` entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeContribution {
    /// The name shown in the theme picker.
    pub label: String,
    /// `vs`, `vs-dark`, `hc-black` or `hc-light`.
    #[serde(default)]
    pub ui_theme: Option<String>,
    /// Path to the theme file, relative to the extension root.
    pub path: String,
}

/// Where an extension's capability declaration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationSource {
    /// The manifest's `deco.capabilities`.
    Manifest,
    /// The extension predates deco's capability model and declared nothing.
    ///
    /// deco does not guess on its behalf: an undeclared extension starts with
    /// no capabilities at all, and the user grants them explicitly. That will
    /// break extensions written for VS Code's ambient-authority model, which is
    /// the point — the alternative is granting everything silently.
    Undeclared,
}

/// Failure to read a manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The file was not valid JSONC.
    #[error(transparent)]
    Parse(#[from] deco_config::JsoncError),
    /// A required field was missing or of the wrong type.
    #[error("invalid manifest: {0}")]
    Invalid(String),
}

impl Manifest {
    /// Parses a `package.json`.
    pub fn parse(source: &str) -> Result<Self, ManifestError> {
        let value = deco_config::parse_jsonc(source)?;
        serde_json::from_value(value).map_err(|e| ManifestError::Invalid(e.to_string()))
    }

    /// The fully qualified `publisher.name` identifier.
    ///
    /// A manifest with no publisher gets the `local.` prefix rather than a bare
    /// name, so an unpublished extension can never collide with a marketplace
    /// one.
    pub fn identifier(&self) -> String {
        match &self.publisher {
            Some(publisher) => format!("{publisher}.{}", self.name),
            None => format!("local.{}", self.name),
        }
    }

    /// The name to show in UI.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// The capabilities the manifest declares, and where they came from.
    pub fn capabilities(&self) -> (Vec<Capability>, DeclarationSource) {
        match &self.deco {
            Some(section) => (section.capabilities.clone(), DeclarationSource::Manifest),
            None => (Vec::new(), DeclarationSource::Undeclared),
        }
    }

    /// Whether the extension has code to run, as opposed to being purely
    /// declarative (a theme or grammar pack).
    ///
    /// Declarative extensions never start a host process, so they never need a
    /// capability at all — which is why themes from the marketplace work in
    /// deco with no consent prompt.
    pub fn has_code(&self) -> bool {
        self.main.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::PathScope;

    const FULL: &str = r#"{
        "name": "my-ext",
        "publisher": "acme",
        "version": "1.2.3",
        "displayName": "My Extension",
        "main": "./out/extension.js",
        "engines": { "vscode": "^1.75.0" },
        "activationEvents": ["onLanguage:rust", "onCommand:my.cmd"],
        "contributes": {
            "commands": [{ "command": "my.cmd", "title": "Do It", "category": "My" }],
            "languages": [{ "id": "rust", "extensions": [".rs"], "filenames": ["Cargo.lock"] }],
            "grammars": [{ "language": "rust", "scopeName": "source.rust", "path": "./r.json" }],
            "themes": [{ "label": "My Dark", "uiTheme": "vs-dark", "path": "./themes/d.json" }],
            "configuration": { "title": "My", "properties": {} }
        },
        "deco": {
            "capabilities": [
                { "capability": "readFile", "scope": { "kind": "workspace" } },
                { "capability": "network", "host": "*.acme.com" }
            ]
        }
    }"#;

    #[test]
    fn parses_a_full_manifest() {
        let m = Manifest::parse(FULL).unwrap();
        assert_eq!(m.identifier(), "acme.my-ext");
        assert_eq!(m.label(), "My Extension");
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.activation_events.len(), 2);
        assert_eq!(m.engines.get("vscode").map(String::as_str), Some("^1.75.0"));
        assert!(m.has_code());
    }

    #[test]
    fn parses_contribution_points() {
        let c = Manifest::parse(FULL).unwrap().contributes;
        assert_eq!(c.commands[0].command, "my.cmd");
        assert_eq!(c.commands[0].category.as_deref(), Some("My"));
        assert_eq!(c.languages[0].extensions, [".rs"]);
        assert_eq!(c.languages[0].filenames, ["Cargo.lock"]);
        assert_eq!(c.grammars[0].scope_name, "source.rust");
        assert_eq!(c.themes[0].label, "My Dark");
        assert!(c.configuration.is_some());
    }

    #[test]
    fn parses_the_deco_capability_declaration() {
        let (caps, source) = Manifest::parse(FULL).unwrap().capabilities();
        assert_eq!(source, DeclarationSource::Manifest);
        assert_eq!(caps.len(), 2);
        assert_eq!(
            caps[0],
            Capability::ReadFile {
                scope: PathScope::Workspace
            }
        );
    }

    #[test]
    fn an_extension_without_a_deco_section_declares_nothing() {
        let m = Manifest::parse(r#"{"name": "x", "main": "./x.js"}"#).unwrap();
        let (caps, source) = m.capabilities();
        assert!(
            caps.is_empty(),
            "nothing may be inferred on the extension's behalf"
        );
        assert_eq!(source, DeclarationSource::Undeclared);
    }

    #[test]
    fn an_unpublished_extension_gets_a_local_identifier() {
        let m = Manifest::parse(r#"{"name": "scratch"}"#).unwrap();
        assert_eq!(m.identifier(), "local.scratch");
    }

    #[test]
    fn a_declarative_extension_has_no_code() {
        let m = Manifest::parse(
            r#"{"name": "theme-pack", "contributes": {"themes": [{"label": "T", "path": "./t.json"}]}}"#,
        )
        .unwrap();
        assert!(!m.has_code());
        assert_eq!(m.contributes.themes.len(), 1);
    }

    #[test]
    fn manifests_may_contain_comments() {
        let m = Manifest::parse("{\n // a comment\n \"name\": \"x\",\n}").unwrap();
        assert_eq!(m.name, "x");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let m = Manifest::parse(r#"{"name": "x", "somethingNew": {"a": 1}}"#).unwrap();
        assert_eq!(m.name, "x");
    }

    #[test]
    fn a_manifest_without_a_name_is_rejected() {
        assert!(matches!(
            Manifest::parse(r#"{"version": "1"}"#),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn a_malformed_manifest_is_reported_as_a_parse_error() {
        assert!(matches!(
            Manifest::parse("{ nope"),
            Err(ManifestError::Parse(_))
        ));
    }
}
