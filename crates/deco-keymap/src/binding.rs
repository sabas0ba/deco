//! Reading `keybindings.json`.

use serde::Deserialize;
use serde_json::Value;

use crate::keys::{KeyParseError, KeySequence};
use crate::when::{WhenError, WhenExpr};

/// The platform a keymap is being resolved for.
///
/// `keybindings.json` entries may carry `mac`, `linux` and `win` fields that
/// override `key` on that platform; this selects which one applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Linux and other Unixes.
    Linux,
    /// macOS.
    Mac,
    /// Windows.
    Windows,
}

impl Platform {
    /// The platform this binary was built for.
    pub const fn host() -> Self {
        if cfg!(target_os = "macos") {
            Platform::Mac
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }
}

/// Where a binding came from, which decides precedence when two bindings
/// collide on the same key and `when` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// deco's built-in keymap.
    Default,
    /// Contributed by an extension.
    Extension,
    /// The user's `keybindings.json`.
    User,
}

/// A resolved keybinding.
#[derive(Debug, Clone, PartialEq)]
pub struct Keybinding {
    /// The key or chord sequence.
    pub key: KeySequence,
    /// The command to run.
    pub command: String,
    /// Arguments passed to the command.
    pub args: Option<Value>,
    /// The condition under which the binding applies.
    pub when: Option<WhenExpr>,
    /// Where the binding came from.
    pub source: Source,
}

/// One entry from a `keybindings.json` file.
#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    /// Adds a binding.
    Bind(Keybinding),
    /// Removes an earlier binding — written as `"command": "-some.command"`.
    Unbind(Keybinding),
}

impl Rule {
    /// The binding this rule concerns.
    pub fn binding(&self) -> &Keybinding {
        match self {
            Rule::Bind(b) | Rule::Unbind(b) => b,
        }
    }
}

/// A problem with a single entry, reported without abandoning the rest of
/// the file.
///
/// A typo in one binding must not cost the user every other binding in the
/// file, so parsing is entry-by-entry and collects problems as it goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    /// Zero-based index of the offending entry.
    pub index: usize,
    /// What was wrong.
    pub message: String,
}

/// The result of reading a `keybindings.json`.
#[derive(Debug, Clone, Default)]
pub struct ParsedKeybindings {
    /// The entries that were understood, in file order.
    pub rules: Vec<Rule>,
    /// Entries that were skipped, with the reason.
    pub problems: Vec<Problem>,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    mac: Option<String>,
    #[serde(default)]
    linux: Option<String>,
    #[serde(default)]
    win: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Value>,
    #[serde(default)]
    when: Option<String>,
}

/// Failure to read a `keybindings.json` at all.
#[derive(Debug, thiserror::Error)]
pub enum KeybindingsError {
    /// The document was not valid JSONC.
    #[error(transparent)]
    Parse(#[from] deco_config::JsoncError),
    /// The document was valid but was not an array of entries.
    #[error("keybindings.json must contain an array of entries")]
    NotAnArray,
}

/// Parses a `keybindings.json` document.
pub fn parse(
    source: &str,
    platform: Platform,
    binding_source: Source,
) -> Result<ParsedKeybindings, KeybindingsError> {
    let value = deco_config::parse_jsonc(source)?;
    let entries = match value {
        Value::Array(entries) => entries,
        // An empty file parses as `{}` — treat that as "no bindings" rather
        // than as a malformed document.
        Value::Object(map) if map.is_empty() => Vec::new(),
        _ => return Err(KeybindingsError::NotAnArray),
    };

    let mut parsed = ParsedKeybindings::default();
    for (index, entry) in entries.into_iter().enumerate() {
        match parse_entry(entry, platform, binding_source) {
            Ok(Some(rule)) => parsed.rules.push(rule),
            Ok(None) => {}
            Err(message) => parsed.problems.push(Problem { index, message }),
        }
    }
    Ok(parsed)
}

fn parse_entry(
    entry: Value,
    platform: Platform,
    binding_source: Source,
) -> Result<Option<Rule>, String> {
    let raw: RawEntry =
        serde_json::from_value(entry).map_err(|e| format!("not a keybinding object: {e}"))?;

    // A platform-specific spelling replaces `key` outright; VS Code does not
    // merge them.
    let key_text = match platform {
        Platform::Mac => raw.mac.or(raw.key),
        Platform::Linux => raw.linux.or(raw.key),
        Platform::Windows => raw.win.or(raw.key),
    };
    let Some(key_text) = key_text else {
        return Err("entry has no `key`".to_owned());
    };
    let key = KeySequence::parse(&key_text).map_err(|e: KeyParseError| e.to_string())?;

    let Some(command) = raw.command else {
        return Err("entry has no `command`".to_owned());
    };

    let when = match raw.when.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => Some(WhenExpr::parse(text).map_err(|e: WhenError| e.to_string())?),
        None => None,
    };

    let (command, remove) = match command.strip_prefix('-') {
        Some(rest) => (rest.to_owned(), true),
        None => (command, false),
    };

    let binding = Keybinding {
        key,
        command,
        args: raw.args,
        when,
        source: binding_source,
    };
    Ok(Some(if remove {
        Rule::Unbind(binding)
    } else {
        Rule::Bind(binding)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Chord;
    use serde_json::json;

    fn parse_ok(source: &str, platform: Platform) -> ParsedKeybindings {
        let parsed = parse(source, platform, Source::User).unwrap();
        assert!(
            parsed.problems.is_empty(),
            "unexpected problems: {:?}",
            parsed.problems
        );
        parsed
    }

    #[test]
    fn parses_a_simple_binding() {
        let parsed = parse_ok(
            r#"[{ "key": "ctrl+s", "command": "workbench.action.files.save" }]"#,
            Platform::Linux,
        );
        assert_eq!(parsed.rules.len(), 1);
        let Rule::Bind(b) = &parsed.rules[0] else {
            panic!("expected a bind")
        };
        assert_eq!(b.key, KeySequence::single(Chord::parse("ctrl+s").unwrap()));
        assert_eq!(b.command, "workbench.action.files.save");
        assert!(b.when.is_none());
        assert_eq!(b.source, Source::User);
    }

    #[test]
    fn parses_when_clauses() {
        let parsed = parse_ok(
            r#"[{ "key": "tab", "command": "acceptSuggestion", "when": "suggestWidgetVisible" }]"#,
            Platform::Linux,
        );
        let when = parsed.rules[0].binding().when.as_ref().unwrap();
        assert_eq!(*when, WhenExpr::Defined("suggestWidgetVisible".into()));
    }

    #[test]
    fn an_empty_when_is_treated_as_absent() {
        let parsed = parse_ok(
            r#"[{ "key": "tab", "command": "x", "when": "  " }]"#,
            Platform::Linux,
        );
        assert!(parsed.rules[0].binding().when.is_none());
    }

    #[test]
    fn parses_args() {
        let parsed = parse_ok(
            r#"[{ "key": "ctrl+k", "command": "type", "args": { "text": "hi" } }]"#,
            Platform::Linux,
        );
        assert_eq!(parsed.rules[0].binding().args, Some(json!({"text": "hi"})));
    }

    #[test]
    fn platform_overrides_replace_the_default_key() {
        let src = r#"[{ "key": "ctrl+p", "mac": "cmd+p", "command": "quickOpen" }]"#;
        let linux = parse_ok(src, Platform::Linux);
        assert_eq!(linux.rules[0].binding().key.to_string(), "ctrl+p");
        let mac = parse_ok(src, Platform::Mac);
        assert_eq!(mac.rules[0].binding().key.to_string(), "cmd+p");
    }

    #[test]
    fn a_platform_only_entry_is_skipped_elsewhere() {
        // No `key` fallback means the entry simply does not exist on Linux.
        let src = r#"[{ "mac": "cmd+p", "command": "quickOpen" }]"#;
        let linux = parse(src, Platform::Linux, Source::User).unwrap();
        assert!(linux.rules.is_empty());
        assert_eq!(linux.problems.len(), 1);
    }

    #[test]
    fn a_leading_minus_marks_a_removal() {
        let parsed = parse_ok(
            r#"[{ "key": "ctrl+s", "command": "-workbench.action.files.save" }]"#,
            Platform::Linux,
        );
        let Rule::Unbind(b) = &parsed.rules[0] else {
            panic!("expected an unbind")
        };
        assert_eq!(b.command, "workbench.action.files.save");
    }

    #[test]
    fn parses_chord_sequences() {
        let parsed = parse_ok(
            r#"[{ "key": "ctrl+k ctrl+c", "command": "editor.action.addCommentLine" }]"#,
            Platform::Linux,
        );
        assert!(parsed.rules[0].binding().key.is_chord());
    }

    #[test]
    fn comments_and_trailing_commas_are_accepted() {
        let src = r#"[
            // save
            { "key": "ctrl+s", "command": "save" },
        ]"#;
        assert_eq!(parse_ok(src, Platform::Linux).rules.len(), 1);
    }

    #[test]
    fn a_bad_entry_does_not_discard_the_good_ones() {
        let src = r#"[
            { "key": "ctrl+s", "command": "save" },
            { "key": "ctrl+frobnicate", "command": "bad.key" },
            { "command": "no.key" },
            { "key": "ctrl+w", "command": "close", "when": "a &&" },
            { "key": "ctrl+q", "command": "quit" }
        ]"#;
        let parsed = parse(src, Platform::Linux, Source::User).unwrap();
        let commands: Vec<_> = parsed
            .rules
            .iter()
            .map(|r| r.binding().command.as_str())
            .collect();
        assert_eq!(commands, ["save", "quit"]);
        assert_eq!(parsed.problems.len(), 3);
        assert_eq!(
            parsed.problems.iter().map(|p| p.index).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn an_empty_document_yields_no_bindings() {
        assert!(parse_ok("[]", Platform::Linux).rules.is_empty());
        assert!(parse_ok("", Platform::Linux).rules.is_empty());
        assert!(parse_ok("// just a comment\n", Platform::Linux)
            .rules
            .is_empty());
    }

    #[test]
    fn a_non_array_document_is_rejected() {
        assert!(matches!(
            parse(r#"{"key": "ctrl+s"}"#, Platform::Linux, Source::User),
            Err(KeybindingsError::NotAnArray)
        ));
    }

    #[test]
    fn host_platform_matches_the_build_target() {
        let expected = if cfg!(target_os = "macos") {
            Platform::Mac
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        };
        assert_eq!(Platform::host(), expected);
    }
}
