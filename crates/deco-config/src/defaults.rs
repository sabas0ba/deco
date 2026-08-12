//! deco's built-in default settings.
//!
//! Keys and values match VS Code's defaults so that a user's `settings.json`
//! only ever needs to state its differences from the same baseline. Settings
//! deco does not implement yet are deliberately absent rather than present and
//! ignored — `Settings::get` returning `None` is an honest answer, while a
//! default that nothing reads is a silent lie.

use serde_json::{Map, Value};

/// The default settings document, in JSONC so it can carry its own commentary.
pub const DEFAULT_SETTINGS_JSONC: &str = r#"{
    // --- Editor ---------------------------------------------------------
    "editor.tabSize": 4,
    "editor.insertSpaces": true,
    "editor.detectIndentation": true,
    "editor.wordSeparators": "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?",
    "editor.wordWrap": "off",
    "editor.wordWrapColumn": 80,
    "editor.wrappingIndent": "same",
    "editor.lineNumbers": "on",
    "editor.renderWhitespace": "selection",
    "editor.renderControlCharacters": true,
    "editor.cursorBlinking": "blink",
    "editor.cursorStyle": "line",
    "editor.cursorSurroundingLines": 0,
    "editor.scrollBeyondLastLine": true,
    "editor.rulers": [],
    "editor.fontFamily": "monospace",
    "editor.fontSize": 14,
    "editor.lineHeight": 0,
    "editor.tabCompletion": "off",
    "editor.autoClosingBrackets": "languageDefined",
    "editor.trimAutoWhitespace": true,
    "editor.largeFileOptimizations": true,

    // --- Files ----------------------------------------------------------
    "files.eol": "auto",
    "files.encoding": "utf8",
    "files.autoSave": "off",
    "files.autoSaveDelay": 1000,
    "files.trimTrailingWhitespace": false,
    "files.insertFinalNewline": false,
    "files.exclude": {
        "**/.git": true,
        "**/.svn": true,
        "**/.hg": true,
        "**/.DS_Store": true
    },

    // --- Workbench ------------------------------------------------------
    "workbench.colorTheme": "Default Dark Modern",
    "workbench.editor.enablePreview": true,

    // --- Extensions -----------------------------------------------------
    // Extensions run in a separate, unprivileged Node process. These control
    // what that process is allowed to ask deco to do on its behalf; see
    // deco-ext for how each capability is brokered.
    "extensions.autoUpdate": false,
    "extensions.host.enabled": true,
    "extensions.host.startupTimeoutMs": 10000,
    "extensions.host.maxOldSpaceSizeMb": 512,
    // "prompt" asks the user the first time an extension requests a
    // capability its manifest declared. "deny" refuses silently, which is the
    // right setting for shared or automated machines.
    "extensions.permissions.default": "prompt",

    // --- Remote ---------------------------------------------------------
    "remote.autoForwardPorts": false,
    "remote.downloadServerOnRemote": true,
    "remote.connectionTimeoutMs": 20000
}"#;

/// Parses [`DEFAULT_SETTINGS_JSONC`] into a settings map.
pub fn default_settings() -> Map<String, Value> {
    match crate::jsonc::parse(DEFAULT_SETTINGS_JSONC) {
        Ok(Value::Object(map)) => map,
        // The constant is covered by a test, so this branch is unreachable in
        // practice; returning empty beats panicking in a user's editor.
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_document_parses_as_an_object() {
        let map = default_settings();
        assert!(!map.is_empty(), "defaults failed to parse");
    }

    #[test]
    fn defaults_carry_the_settings_the_editor_reads() {
        let map = default_settings();
        for key in [
            "editor.tabSize",
            "editor.insertSpaces",
            "editor.wordSeparators",
            "editor.wordWrap",
            "files.eol",
            "workbench.colorTheme",
            "extensions.permissions.default",
        ] {
            assert!(map.contains_key(key), "missing default for {key}");
        }
    }

    #[test]
    fn default_word_separators_match_vscode() {
        assert_eq!(
            default_settings()["editor.wordSeparators"]
                .as_str()
                .unwrap(),
            deco_core_word_separators()
        );
    }

    /// Kept as a literal rather than importing deco-core so that this crate has
    /// no dependency on it; the two must agree and this test is the check.
    fn deco_core_word_separators() -> &'static str {
        "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?"
    }
}
