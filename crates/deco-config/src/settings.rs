//! Layered settings resolution with VS Code's precedence rules.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::jsonc;

/// Where a settings value came from. Ordering is precedence: a value in a later
/// variant wins over the same key in an earlier one.
///
/// This mirrors VS Code's scope chain. `Remote` sits between user and workspace
/// so that machine-specific settings pushed by a remote (`ssh-remote`, a dev
/// container) beat the user's local preferences but never override what the
/// project itself pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Built-in defaults shipped with deco.
    Default,
    /// The user's `settings.json`.
    User,
    /// Machine settings supplied by the connected remote.
    Remote,
    /// `.deco/settings.json` (or `.vscode/settings.json`) for the workspace.
    Workspace,
    /// A specific folder of a multi-root workspace.
    Folder,
}

impl Scope {
    /// All scopes, lowest precedence first.
    pub const ALL: [Scope; 5] = [
        Scope::Default,
        Scope::User,
        Scope::Remote,
        Scope::Workspace,
        Scope::Folder,
    ];
}

/// Splits a language override key such as `"[rust]"` or `"[javascript][jsx]"`
/// into its language identifiers, or returns `None` if it is an ordinary key.
fn language_override_ids(key: &str) -> Option<Vec<&str>> {
    if !key.starts_with('[') || !key.ends_with(']') {
        return None;
    }
    let mut ids = Vec::new();
    let mut rest = key;
    while let Some(stripped) = rest.strip_prefix('[') {
        let end = stripped.find(']')?;
        let id = stripped[..end].trim();
        if id.is_empty() {
            return None;
        }
        ids.push(id);
        rest = &stripped[end + 1..];
    }
    if rest.is_empty() && !ids.is_empty() {
        Some(ids)
    } else {
        None
    }
}

/// The full settings state: one value map per scope.
///
/// Values are never deep-merged across scopes. VS Code replaces object-valued
/// settings wholesale (only a short list of registered settings merge), and
/// silently merging would make it impossible for a workspace to *remove* an
/// entry the user set globally.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    layers: BTreeMap<Scope, Map<String, Value>>,
}

impl Settings {
    /// Empty settings with no layers at all.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Settings pre-populated with deco's built-in defaults.
    pub fn with_defaults() -> Self {
        let mut settings = Self::empty();
        settings.set_layer(Scope::Default, crate::defaults::default_settings());
        settings
    }

    /// Replaces an entire scope.
    pub fn set_layer(&mut self, scope: Scope, values: Map<String, Value>) {
        self.layers.insert(scope, values);
    }

    /// Parses `source` as JSONC and installs it as `scope`.
    ///
    /// A non-object document is rejected: a `settings.json` containing an array
    /// is a mistake worth surfacing, not something to silently ignore.
    pub fn load_layer(&mut self, scope: Scope, source: &str) -> Result<(), SettingsError> {
        match jsonc::parse(source)? {
            Value::Object(map) => {
                self.layers.insert(scope, map);
                Ok(())
            }
            other => Err(SettingsError::NotAnObject {
                found: value_kind(&other),
            }),
        }
    }

    /// Removes a scope entirely, e.g. on disconnecting from a remote.
    pub fn clear_layer(&mut self, scope: Scope) {
        self.layers.remove(&scope);
    }

    /// Sets a single key within a scope.
    pub fn set(&mut self, scope: Scope, key: &str, value: Value) {
        self.layers
            .entry(scope)
            .or_default()
            .insert(key.to_owned(), value);
    }

    /// The raw map for a scope, if present.
    pub fn layer(&self, scope: Scope) -> Option<&Map<String, Value>> {
        self.layers.get(&scope)
    }

    /// Resolves `key`, ignoring language-specific overrides.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.get_for_language(key, None)
    }

    /// Resolves `key` for `language`, applying VS Code's full precedence chain.
    ///
    /// Within each scope a `[language]` section wins over the plain key, and
    /// scopes are consulted from highest precedence down — so a workspace's
    /// plain `editor.tabSize` still loses to a folder's `[rust]` override, and
    /// beats the user's `[rust]` override.
    pub fn get_for_language(&self, key: &str, language: Option<&str>) -> Option<&Value> {
        for scope in Scope::ALL.iter().rev() {
            let Some(layer) = self.layers.get(scope) else {
                continue;
            };
            if let Some(language) = language {
                if let Some(value) = language_value(layer, key, language) {
                    return Some(value);
                }
            }
            if let Some(value) = layer.get(key) {
                return Some(value);
            }
        }
        None
    }

    /// The scope that currently supplies `key`, useful for "modified elsewhere"
    /// hints in the settings UI.
    pub fn source_of(&self, key: &str, language: Option<&str>) -> Option<Scope> {
        for scope in Scope::ALL.iter().rev() {
            let Some(layer) = self.layers.get(scope) else {
                continue;
            };
            if language.is_some_and(|l| language_value(layer, key, l).is_some())
                || layer.contains_key(key)
            {
                return Some(*scope);
            }
        }
        None
    }

    /// Every key visible after resolution, including keys only present in
    /// language sections (reported under their plain name).
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        for layer in self.layers.values() {
            for (key, value) in layer {
                if language_override_ids(key).is_some() {
                    if let Value::Object(inner) = value {
                        keys.extend(inner.keys().cloned());
                    }
                } else {
                    keys.push(key.clone());
                }
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }

    /// Resolves `key` as a bool.
    pub fn get_bool(&self, key: &str, language: Option<&str>) -> Option<bool> {
        self.get_for_language(key, language)?.as_bool()
    }

    /// Resolves `key` as an integer.
    pub fn get_u64(&self, key: &str, language: Option<&str>) -> Option<u64> {
        self.get_for_language(key, language)?.as_u64()
    }

    /// Resolves `key` as a float.
    pub fn get_f64(&self, key: &str, language: Option<&str>) -> Option<f64> {
        self.get_for_language(key, language)?.as_f64()
    }

    /// Resolves `key` as a string.
    pub fn get_str(&self, key: &str, language: Option<&str>) -> Option<&str> {
        self.get_for_language(key, language)?.as_str()
    }

    /// Resolves `key` and deserializes it into `T`.
    pub fn get_as<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
        language: Option<&str>,
    ) -> Option<T> {
        serde_json::from_value(self.get_for_language(key, language)?.clone()).ok()
    }
}

/// Looks up `key` inside any `[language]` section of `layer`.
fn language_value<'a>(
    layer: &'a Map<String, Value>,
    key: &str,
    language: &str,
) -> Option<&'a Value> {
    for (section, value) in layer {
        let Some(ids) = language_override_ids(section) else {
            continue;
        };
        if !ids.contains(&language) {
            continue;
        }
        if let Value::Object(inner) = value {
            if let Some(found) = inner.get(key) {
                return Some(found);
            }
        }
    }
    None
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Failure to load a settings layer.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The file could not be parsed.
    #[error(transparent)]
    Parse(#[from] jsonc::JsoncError),
    /// The file parsed but was not a JSON object.
    #[error("settings must be a JSON object, found {found}")]
    NotAnObject {
        /// The kind that was found instead.
        found: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings_with(pairs: &[(Scope, &str)]) -> Settings {
        let mut settings = Settings::empty();
        for (scope, src) in pairs {
            settings.load_layer(*scope, src).unwrap();
        }
        settings
    }

    #[test]
    fn recognises_language_override_sections() {
        assert_eq!(language_override_ids("[rust]"), Some(vec!["rust"]));
        assert_eq!(
            language_override_ids("[javascript][typescript]"),
            Some(vec!["javascript", "typescript"])
        );
        assert_eq!(language_override_ids("editor.tabSize"), None);
        assert_eq!(language_override_ids("[]"), None);
        assert_eq!(language_override_ids("[unclosed"), None);
        assert_eq!(language_override_ids("[a]trailing"), None);
    }

    #[test]
    fn higher_scopes_win() {
        let s = settings_with(&[
            (Scope::Default, r#"{"editor.tabSize": 4}"#),
            (Scope::User, r#"{"editor.tabSize": 2}"#),
            (Scope::Workspace, r#"{"editor.tabSize": 8}"#),
        ]);
        assert_eq!(s.get("editor.tabSize"), Some(&json!(8)));
        assert_eq!(s.source_of("editor.tabSize", None), Some(Scope::Workspace));
    }

    #[test]
    fn lower_scopes_fill_gaps() {
        let s = settings_with(&[
            (
                Scope::Default,
                r#"{"editor.tabSize": 4, "editor.fontSize": 14}"#,
            ),
            (Scope::User, r#"{"editor.tabSize": 2}"#),
        ]);
        assert_eq!(s.get("editor.fontSize"), Some(&json!(14)));
    }

    #[test]
    fn remote_sits_between_user_and_workspace() {
        let s = settings_with(&[
            (Scope::User, r#"{"a": "user", "b": "user"}"#),
            (Scope::Remote, r#"{"a": "remote"}"#),
            (Scope::Workspace, r#"{"b": "workspace"}"#),
        ]);
        assert_eq!(s.get("a"), Some(&json!("remote")));
        assert_eq!(s.get("b"), Some(&json!("workspace")));
    }

    #[test]
    fn language_overrides_beat_plain_keys_in_the_same_scope() {
        let s = settings_with(&[(
            Scope::User,
            r#"{"editor.tabSize": 2, "[rust]": {"editor.tabSize": 4}}"#,
        )]);
        assert_eq!(
            s.get_for_language("editor.tabSize", Some("rust")),
            Some(&json!(4))
        );
        assert_eq!(
            s.get_for_language("editor.tabSize", Some("python")),
            Some(&json!(2))
        );
        assert_eq!(s.get("editor.tabSize"), Some(&json!(2)));
    }

    #[test]
    fn a_higher_scope_plain_key_beats_a_lower_scope_language_override() {
        let s = settings_with(&[
            (Scope::User, r#"{"[rust]": {"editor.tabSize": 4}}"#),
            (Scope::Workspace, r#"{"editor.tabSize": 8}"#),
        ]);
        assert_eq!(
            s.get_for_language("editor.tabSize", Some("rust")),
            Some(&json!(8))
        );
    }

    #[test]
    fn a_higher_scope_language_override_beats_everything_below() {
        let s = settings_with(&[
            (Scope::User, r#"{"editor.tabSize": 2}"#),
            (Scope::Workspace, r#"{"editor.tabSize": 8}"#),
            (Scope::Folder, r#"{"[rust]": {"editor.tabSize": 3}}"#),
        ]);
        assert_eq!(
            s.get_for_language("editor.tabSize", Some("rust")),
            Some(&json!(3))
        );
        assert_eq!(
            s.get_for_language("editor.tabSize", Some("go")),
            Some(&json!(8))
        );
    }

    #[test]
    fn multi_language_sections_apply_to_each_listed_language() {
        let s = settings_with(&[(
            Scope::User,
            r#"{"editor.tabSize": 4, "[javascript][typescript]": {"editor.tabSize": 2}}"#,
        )]);
        assert_eq!(
            s.get_for_language("editor.tabSize", Some("javascript")),
            Some(&json!(2))
        );
        assert_eq!(
            s.get_for_language("editor.tabSize", Some("typescript")),
            Some(&json!(2))
        );
        assert_eq!(
            s.get_for_language("editor.tabSize", Some("json")),
            Some(&json!(4))
        );
    }

    #[test]
    fn object_values_replace_rather_than_merge() {
        let s = settings_with(&[
            (
                Scope::User,
                r#"{"files.exclude": {"**/.git": true, "**/.DS_Store": true}}"#,
            ),
            (
                Scope::Workspace,
                r#"{"files.exclude": {"**/target": true}}"#,
            ),
        ]);
        assert_eq!(s.get("files.exclude"), Some(&json!({"**/target": true})));
    }

    #[test]
    fn missing_keys_resolve_to_none() {
        let s = settings_with(&[(Scope::User, r#"{"a": 1}"#)]);
        assert_eq!(s.get("nope"), None);
        assert_eq!(s.source_of("nope", None), None);
    }

    #[test]
    fn clearing_a_layer_reveals_the_one_below() {
        let mut s = settings_with(&[
            (Scope::User, r#"{"a": "user"}"#),
            (Scope::Remote, r#"{"a": "remote"}"#),
        ]);
        assert_eq!(s.get("a"), Some(&json!("remote")));
        s.clear_layer(Scope::Remote);
        assert_eq!(s.get("a"), Some(&json!("user")));
    }

    #[test]
    fn typed_accessors_read_the_resolved_value() {
        let s = settings_with(&[(
            Scope::User,
            r#"{"b": true, "n": 3, "f": 1.5, "s": "x", "arr": [1, 2]}"#,
        )]);
        assert_eq!(s.get_bool("b", None), Some(true));
        assert_eq!(s.get_u64("n", None), Some(3));
        assert_eq!(s.get_f64("f", None), Some(1.5));
        assert_eq!(s.get_str("s", None), Some("x"));
        assert_eq!(s.get_as::<Vec<u8>>("arr", None), Some(vec![1, 2]));
    }

    #[test]
    fn typed_accessors_return_none_on_a_type_mismatch() {
        let s = settings_with(&[(Scope::User, r#"{"n": "not a number"}"#)]);
        assert_eq!(s.get_u64("n", None), None);
    }

    #[test]
    fn keys_lists_language_scoped_settings_under_their_plain_name() {
        let s = settings_with(&[(
            Scope::User,
            r#"{"editor.fontSize": 12, "[rust]": {"editor.tabSize": 4}}"#,
        )]);
        assert_eq!(s.keys(), vec!["editor.fontSize", "editor.tabSize"]);
    }

    #[test]
    fn set_writes_into_a_single_scope() {
        let mut s = Settings::empty();
        s.set(Scope::User, "editor.tabSize", json!(2));
        assert_eq!(s.get("editor.tabSize"), Some(&json!(2)));
        assert_eq!(s.source_of("editor.tabSize", None), Some(Scope::User));
    }

    #[test]
    fn settings_files_may_contain_comments() {
        let s = settings_with(&[(
            Scope::User,
            "{\n // my preference\n \"editor.tabSize\": 2,\n}",
        )]);
        assert_eq!(s.get("editor.tabSize"), Some(&json!(2)));
    }

    #[test]
    fn a_non_object_settings_file_is_rejected() {
        let mut s = Settings::empty();
        let err = s.load_layer(Scope::User, "[1, 2]").unwrap_err();
        assert!(matches!(err, SettingsError::NotAnObject { found: "array" }));
    }

    #[test]
    fn defaults_are_available_out_of_the_box() {
        let s = Settings::with_defaults();
        assert_eq!(s.get_u64("editor.tabSize", None), Some(4));
        assert_eq!(s.source_of("editor.tabSize", None), Some(Scope::Default));
    }
}
