//! User snippet files, in VS Code's format.
//!
//! A snippet file is JSON with comments. Each member is one snippet, named by
//! its key:
//!
//! ```jsonc
//! {
//!   "Print to console": {
//!     "prefix": ["log", "print"],
//!     "body": ["console.log('$1');", "$0"],
//!     "description": "Log output to the console",
//!     "scope": "javascript,typescript"
//!   }
//! }
//! ```
//!
//! `<language>.json` holds snippets for that language only, and its members'
//! `scope` is ignored, as in VS Code. `*.code-snippets` holds snippets for the
//! languages in each member's `scope`, or for every language when it has none.
//! Other files are not snippet files.
//!
//! This module only reads text. Finding the files is the caller's job, so the
//! same code serves user snippets and a workspace's `.vscode` snippets.

use serde_json::Value;

/// One snippet from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSnippet {
    /// The member's key, shown in the snippet list.
    pub name: String,
    /// What to type for the snippet to be offered as a completion. Empty when
    /// it can only be inserted by name.
    pub prefixes: Vec<String>,
    /// The snippet text; an array body is joined with line breaks.
    pub body: String,
    pub description: Option<String>,
    /// The language ids it applies to, or `None` for every language.
    pub languages: Option<Vec<String>>,
}

impl UserSnippet {
    /// Whether the snippet is offered in a document of `language`.
    ///
    /// A snippet limited to languages is not offered in a document with no
    /// language.
    pub fn applies_to(&self, language: Option<&str>) -> bool {
        match (&self.languages, language) {
            (None, _) => true,
            (Some(languages), Some(language)) => languages.iter().any(|l| l == language),
            (Some(_), None) => false,
        }
    }
}

/// What kind of snippet file a file name denotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    /// `<language>.json`.
    Language(String),
    /// `*.code-snippets`.
    Global,
}

impl FileKind {
    /// The kind of `file_name`, or `None` when it is not a snippet file.
    pub fn of(file_name: &str) -> Option<Self> {
        if let Some(stem) = file_name.strip_suffix(".code-snippets") {
            return (!stem.is_empty()).then_some(Self::Global);
        }
        let language = file_name.strip_suffix(".json")?;
        (!language.is_empty()).then(|| Self::Language(language.to_owned()))
    }
}

/// Reads the snippets in a file of `kind`.
///
/// Returns the snippets that could be read, in name order, and a message for
/// each that could not. A malformed member does not prevent the others from
/// loading.
pub fn parse(kind: &FileKind, text: &str) -> (Vec<UserSnippet>, Vec<String>) {
    let mut snippets = Vec::new();
    let mut problems = Vec::new();
    let root = match crate::jsonc::parse(text) {
        Ok(Value::Object(root)) => root,
        Ok(_) => return (snippets, vec!["the file is not a JSON object".to_owned()]),
        Err(error) => return (snippets, vec![error.to_string()]),
    };
    for (name, value) in root {
        match snippet(kind, &name, &value) {
            Ok(snippet) => snippets.push(snippet),
            Err(problem) => problems.push(format!("snippet `{name}`: {problem}")),
        }
    }
    (snippets, problems)
}

fn snippet(kind: &FileKind, name: &str, value: &Value) -> Result<UserSnippet, &'static str> {
    let Value::Object(fields) = value else {
        return Err("not an object");
    };
    let body = match fields.get("body") {
        Some(Value::String(body)) => body.clone(),
        Some(Value::Array(lines)) => lines
            .iter()
            .map(|line| {
                line.as_str()
                    .ok_or("`body` has a line that is not a string")
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("\n"),
        Some(_) => return Err("`body` is neither a string nor an array of strings"),
        None => return Err("no `body`"),
    };
    let prefixes = match fields.get("prefix") {
        None => Vec::new(),
        Some(Value::String(prefix)) => vec![prefix.clone()],
        Some(Value::Array(prefixes)) => prefixes
            .iter()
            .map(|prefix| {
                prefix
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("`prefix` has an entry that is not a string")
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("`prefix` is neither a string nor an array of strings"),
    };
    let languages = match kind {
        FileKind::Language(language) => Some(vec![language.clone()]),
        FileKind::Global => match fields.get("scope") {
            None => None,
            Some(Value::String(scope)) => {
                let languages: Vec<String> = scope
                    .split(',')
                    .map(str::trim)
                    .filter(|language| !language.is_empty())
                    .map(str::to_owned)
                    .collect();
                (!languages.is_empty()).then_some(languages)
            }
            Some(_) => return Err("`scope` is not a string"),
        },
    };
    let description = match fields.get("description") {
        None => None,
        Some(Value::String(description)) => Some(description.clone()),
        // VS Code accepts an array of lines here too.
        Some(Value::Array(lines)) => Some(
            lines
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Some(_) => return Err("`description` is not a string"),
    };
    Ok(UserSnippet {
        name: name.to_owned(),
        prefixes,
        body,
        description,
        languages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_decide_the_languages() {
        assert_eq!(
            FileKind::of("rust.json"),
            Some(FileKind::Language("rust".to_owned()))
        );
        assert_eq!(FileKind::of("web.code-snippets"), Some(FileKind::Global));
        for name in ["notes.txt", ".json", ".code-snippets", "rust.json.bak"] {
            assert_eq!(FileKind::of(name), None, "{name}");
        }
    }

    #[test]
    fn a_global_file_reads_scopes_bodies_and_prefixes() {
        let (snippets, problems) = parse(
            &FileKind::Global,
            r#"{
              // Comments are allowed, as in every VS Code JSON file.
              "Log": {
                "prefix": ["log", "print"],
                "body": ["console.log('$1');", "$0"],
                "description": "Log output",
                "scope": "javascript, typescript",
              },
              "Header": { "body": "// $TM_FILENAME" },
            }"#,
        );
        assert!(problems.is_empty(), "{problems:?}");
        // In name order: the JSON object does not keep the file's order.
        let [header, log] = &snippets[..] else {
            panic!("two snippets: {snippets:?}");
        };
        assert_eq!(log.name, "Log");
        assert_eq!(log.prefixes, ["log", "print"]);
        assert_eq!(log.body, "console.log('$1');\n$0");
        assert_eq!(log.description.as_deref(), Some("Log output"));
        assert!(log.applies_to(Some("typescript")));
        assert!(!log.applies_to(Some("rust")));
        assert!(!log.applies_to(None));
        assert!(header.prefixes.is_empty());
        assert!(header.applies_to(Some("rust")) && header.applies_to(None));
    }

    #[test]
    fn a_language_file_ignores_scope() {
        let (snippets, _) = parse(
            &FileKind::Language("rust".to_owned()),
            r#"{ "fn": { "prefix": "fn", "body": "fn $1() {}", "scope": "python" } }"#,
        );
        assert_eq!(snippets[0].languages, Some(vec!["rust".to_owned()]));
    }

    #[test]
    fn a_bad_member_is_reported_and_the_rest_still_load() {
        let (snippets, problems) = parse(
            &FileKind::Global,
            r#"{ "ok": { "body": "x" }, "no body": { "prefix": "a" }, "bad": 3 }"#,
        );
        assert_eq!(snippets.len(), 1);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems.iter().any(|p| p.contains("no body")),
            "{problems:?}"
        );

        let (snippets, problems) = parse(&FileKind::Global, "[1, 2]");
        assert!(snippets.is_empty());
        assert_eq!(problems.len(), 1);
    }
}
