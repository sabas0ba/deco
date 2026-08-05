//! Loading and querying a VS Code colour theme.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::color::Rgba;
use crate::semantic::{SemanticSelector, SemanticToken};
use crate::tokens::{RawSettings, RawTokenColor, TokenColorRule, TokenStyle};

/// The `type` field of a theme, which decides which built-in defaults fill the
/// gaps a partial theme leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeKind {
    /// `"dark"`.
    #[default]
    Dark,
    /// `"light"`.
    Light,
    /// `"hc-black"`.
    HighContrastDark,
    /// `"hc-light"`.
    HighContrastLight,
}

impl ThemeKind {
    /// Parses the `type` field.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "dark" | "vs-dark" => ThemeKind::Dark,
            "light" | "vs" => ThemeKind::Light,
            "hc-black" | "hcDark" => ThemeKind::HighContrastDark,
            "hc-light" | "hcLight" => ThemeKind::HighContrastLight,
            _ => return None,
        })
    }

    /// Whether the theme is a dark variant.
    pub fn is_dark(self) -> bool {
        matches!(self, ThemeKind::Dark | ThemeKind::HighContrastDark)
    }
}

/// A loaded colour theme.
#[derive(Debug, Clone)]
pub struct ColorTheme {
    /// The theme's display name.
    pub name: String,
    /// Dark, light or high contrast.
    pub kind: ThemeKind,
    colors: BTreeMap<String, Rgba>,
    /// The style from a `tokenColors` entry that has no `scope` — the theme's
    /// baseline for all text.
    default_style: TokenStyle,
    rules: Vec<TokenColorRule>,
    semantic_highlighting: bool,
    semantic_rules: Vec<(SemanticSelector, TokenStyle)>,
}

/// Failure to load a theme.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    /// The document was not valid JSONC.
    #[error("{path}: {source}")]
    Parse {
        /// The file that failed.
        path: PathBuf,
        /// The parse failure.
        source: deco_config::JsoncError,
    },
    /// The file could not be read.
    #[error("cannot read {path}: {message}")]
    Read {
        /// The file that failed.
        path: PathBuf,
        /// The I/O error.
        message: String,
    },
    /// The document was valid JSON but not a theme object.
    #[error("{path} is not a theme object")]
    NotATheme {
        /// The file that failed.
        path: PathBuf,
    },
    /// `include` chains nested too deeply.
    #[error("`include` chain from {path} is more than {limit} deep")]
    IncludeTooDeep {
        /// Where the chain started.
        path: PathBuf,
        /// The limit that was exceeded.
        limit: usize,
    },
    /// A theme includes itself, directly or through other files.
    #[error("`include` cycle: {path} is included more than once")]
    IncludeCycle {
        /// The file that repeated.
        path: PathBuf,
    },
}

/// Resolves `.` and `..` lexically.
///
/// `PathBuf::join` does not, so `themes/x.json` including `../shared/y.json`
/// would otherwise produce `themes/../shared/y.json` — which works on a real
/// filesystem but defeats the cycle check, since the same file can then be
/// spelled many ways.
fn normalize(path: PathBuf) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Popping past the root leaves the `..` in place, which is the
                // best that can be done without touching the filesystem.
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// How many `include` hops are followed before giving up.
const MAX_INCLUDE_DEPTH: usize = 16;

#[derive(Debug, Default, Deserialize)]
struct RawTheme {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    colors: BTreeMap<String, Value>,
    #[serde(default, rename = "tokenColors")]
    token_colors: Vec<RawTokenColor>,
    #[serde(default, rename = "semanticHighlighting")]
    semantic_highlighting: Option<bool>,
    #[serde(default, rename = "semanticTokenColors")]
    semantic_token_colors: BTreeMap<String, Value>,
}

impl ColorTheme {
    /// Loads a theme that has no `include` (or whose include should be ignored).
    pub fn from_json(source: &str) -> Result<Self, ThemeError> {
        let path = PathBuf::from("<memory>");
        let raw = parse_raw(source, &path)?;
        let mut theme = ColorTheme::empty();
        theme.overlay(raw);
        Ok(theme)
    }

    /// Loads a theme from `path`, following `include` chains.
    ///
    /// `read` supplies file contents so that include resolution can be tested
    /// without touching the filesystem; [`ColorTheme::load_from_file`] wraps it
    /// with real I/O.
    pub fn load<F>(path: &Path, read: &mut F) -> Result<Self, ThemeError>
    where
        F: FnMut(&Path) -> std::io::Result<String>,
    {
        // Collect the chain first, then apply it base-first, so that the file
        // the user actually named wins every conflict.
        let mut chain: Vec<RawTheme> = Vec::new();
        let mut visited: Vec<PathBuf> = Vec::new();
        let mut current = normalize(path.to_path_buf());

        loop {
            if visited.contains(&current) {
                return Err(ThemeError::IncludeCycle { path: current });
            }
            if chain.len() >= MAX_INCLUDE_DEPTH {
                return Err(ThemeError::IncludeTooDeep {
                    path: path.to_path_buf(),
                    limit: MAX_INCLUDE_DEPTH,
                });
            }
            let source = read(&current).map_err(|e| ThemeError::Read {
                path: current.clone(),
                message: e.to_string(),
            })?;
            let raw = parse_raw(&source, &current)?;
            let include = raw.include.clone();
            let dir = current.parent().map(Path::to_path_buf).unwrap_or_default();
            visited.push(current.clone());
            chain.push(raw);

            match include {
                Some(relative) => current = normalize(dir.join(relative)),
                None => break,
            }
        }

        let mut theme = ColorTheme::empty();
        for raw in chain.into_iter().rev() {
            theme.overlay(raw);
        }
        Ok(theme)
    }

    /// Loads a theme from disk, following `include` chains.
    pub fn load_from_file(path: &Path) -> Result<Self, ThemeError> {
        ColorTheme::load(path, &mut |p| std::fs::read_to_string(p))
    }

    fn empty() -> Self {
        Self {
            name: String::new(),
            kind: ThemeKind::Dark,
            colors: BTreeMap::new(),
            default_style: TokenStyle::default(),
            rules: Vec::new(),
            semantic_highlighting: false,
            semantic_rules: Vec::new(),
        }
    }

    /// Merges `raw` on top of this theme, as an including file does to the file
    /// it includes.
    fn overlay(&mut self, raw: RawTheme) {
        if let Some(name) = raw.name {
            self.name = name;
        }
        if let Some(kind) = raw.kind.as_deref().and_then(ThemeKind::parse) {
            self.kind = kind;
        }
        for (key, value) in raw.colors {
            match value {
                // A `null` entry removes an inherited colour rather than
                // setting one, matching how VS Code treats it.
                Value::Null => {
                    self.colors.remove(&key);
                }
                Value::String(text) => {
                    if let Ok(color) = text.parse::<Rgba>() {
                        self.colors.insert(key, color);
                    }
                }
                _ => {}
            }
        }
        for entry in raw.token_colors {
            if entry.scope.is_none() {
                // The scope-less entry is the theme's baseline style.
                let style = entry.settings.into_style();
                self.default_style.apply(&style);
                continue;
            }
            if let Some(rule) = entry.into_rule() {
                self.rules.push(rule);
            }
        }
        if let Some(enabled) = raw.semantic_highlighting {
            self.semantic_highlighting = enabled;
        }
        for (selector, value) in raw.semantic_token_colors {
            let Some(parsed) = SemanticSelector::parse(&selector) else {
                continue;
            };
            let style = match value {
                Value::String(text) => match text.parse::<Rgba>() {
                    Ok(color) => TokenStyle {
                        foreground: Some(color),
                        ..TokenStyle::default()
                    },
                    Err(_) => continue,
                },
                other => match serde_json::from_value::<RawSettings>(other) {
                    Ok(settings) => settings.into_style(),
                    Err(_) => continue,
                },
            };
            // Later definitions replace earlier ones for the same selector.
            self.semantic_rules
                .retain(|(existing, _)| *existing != parsed);
            self.semantic_rules.push((parsed, style));
        }
    }

    /// A workbench colour as the theme defines it, without defaults.
    pub fn raw_color(&self, key: &str) -> Option<Rgba> {
        self.colors.get(key).copied()
    }

    /// A workbench colour, falling back through VS Code's derivation chain and
    /// then to this theme kind's built-in default.
    ///
    /// Themes routinely define only a handful of the several hundred workbench
    /// colours, so unconditionally returning something is what keeps a partial
    /// theme from rendering an unreadable editor.
    pub fn color(&self, key: &str) -> Option<Rgba> {
        if let Some(color) = self.colors.get(key) {
            return Some(*color);
        }
        for fallback in crate::defaults::fallback_chain(key) {
            if let Some(color) = self.colors.get(*fallback) {
                return Some(*color);
            }
        }
        crate::defaults::default_color(self.kind, key)
    }

    /// Every workbench colour the theme defines.
    pub fn colors(&self) -> &BTreeMap<String, Rgba> {
        &self.colors
    }

    /// The theme's baseline text style.
    pub fn default_style(&self) -> TokenStyle {
        let mut style = self.default_style;
        if style.foreground.is_none() {
            style.foreground = self.color("editor.foreground");
        }
        style
    }

    /// The parsed `tokenColors` rules, in file order.
    pub fn rules(&self) -> &[TokenColorRule] {
        &self.rules
    }

    /// Whether the theme opted into semantic highlighting.
    pub fn semantic_highlighting(&self) -> bool {
        self.semantic_highlighting
    }

    /// The style for a token whose scope stack is `stack`, outermost first.
    ///
    /// Every matching rule is applied in ascending specificity, so a broad rule
    /// setting `fontStyle` and a narrow one setting `foreground` combine — this
    /// is the inheritance vscode-textmate gets from its scope trie.
    pub fn style_for_scopes(&self, stack: &[&str]) -> TokenStyle {
        let mut matches: Vec<(crate::tokens::Specificity, usize)> = self
            .rules
            .iter()
            .enumerate()
            .filter_map(|(index, rule)| rule.matches(stack).map(|score| (score, index)))
            .collect();
        matches.sort();

        let mut style = self.default_style();
        for (_, index) in matches {
            style.apply(&self.rules[index].style);
        }
        style
    }

    /// The style for a semantic token, or `None` when no rule applies.
    ///
    /// Returns `None` rather than the default style so the caller can fall back
    /// to the TextMate result, which is what VS Code does when semantic
    /// highlighting has nothing to say about a token.
    pub fn style_for_semantic(&self, token: &SemanticToken<'_>) -> Option<TokenStyle> {
        let mut matches: Vec<(crate::semantic::SemanticSpecificity, usize)> = self
            .semantic_rules
            .iter()
            .enumerate()
            .filter_map(|(index, (selector, _))| {
                selector.matches(token).map(|score| (score, index))
            })
            .collect();
        if matches.is_empty() {
            return None;
        }
        matches.sort();

        let mut style = TokenStyle::default();
        for (_, index) in matches {
            style.apply(&self.semantic_rules[index].1);
        }
        Some(style)
    }
}

fn parse_raw(source: &str, path: &Path) -> Result<RawTheme, ThemeError> {
    let value = deco_config::parse_jsonc(source).map_err(|source| ThemeError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    if !value.is_object() {
        return Err(ThemeError::NotATheme {
            path: path.to_path_buf(),
        });
    }
    serde_json::from_value(value).map_err(|_| ThemeError::NotATheme {
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::FontStyle;
    use std::collections::HashMap;

    fn theme(source: &str) -> ColorTheme {
        ColorTheme::from_json(source).unwrap()
    }

    /// A reader backed by an in-memory file tree.
    fn reader(files: Vec<(&str, &str)>) -> impl FnMut(&Path) -> std::io::Result<String> {
        let map: HashMap<PathBuf, String> = files
            .into_iter()
            .map(|(k, v)| (PathBuf::from(k), v.to_owned()))
            .collect();
        move |path: &Path| {
            map.get(path).cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, path.display().to_string())
            })
        }
    }

    #[test]
    fn parses_name_kind_and_colours() {
        let t = theme(
            r##"{
                "name": "My Theme",
                "type": "light",
                "colors": { "editor.background": "#ffffff", "editor.foreground": "#000000" }
            }"##,
        );
        assert_eq!(t.name, "My Theme");
        assert_eq!(t.kind, ThemeKind::Light);
        assert_eq!(t.raw_color("editor.background"), Some(Rgba::WHITE));
    }

    #[test]
    fn theme_kind_accepts_every_spelling() {
        assert_eq!(ThemeKind::parse("dark"), Some(ThemeKind::Dark));
        assert_eq!(ThemeKind::parse("vs-dark"), Some(ThemeKind::Dark));
        assert_eq!(ThemeKind::parse("vs"), Some(ThemeKind::Light));
        assert_eq!(
            ThemeKind::parse("hc-black"),
            Some(ThemeKind::HighContrastDark)
        );
        assert_eq!(
            ThemeKind::parse("hc-light"),
            Some(ThemeKind::HighContrastLight)
        );
        assert_eq!(ThemeKind::parse("mauve"), None);
        assert!(ThemeKind::HighContrastDark.is_dark());
        assert!(!ThemeKind::HighContrastLight.is_dark());
    }

    #[test]
    fn theme_files_may_contain_comments_and_trailing_commas() {
        let t = theme("{\n // my theme\n \"name\": \"X\",\n}");
        assert_eq!(t.name, "X");
    }

    #[test]
    fn unparseable_colours_are_skipped_not_fatal() {
        let t =
            theme(r##"{"colors": {"editor.background": "#123456", "bad.key": "not-a-colour"}}"##);
        assert_eq!(
            t.raw_color("editor.background"),
            Some(Rgba::rgb(0x12, 0x34, 0x56))
        );
        assert_eq!(t.raw_color("bad.key"), None);
    }

    #[test]
    fn missing_colours_fall_back_to_the_kind_default() {
        let t = theme(r##"{"type": "dark", "colors": {}}"##);
        let bg = t
            .color("editor.background")
            .expect("dark themes have a default background");
        assert!(!bg.luminance().is_nan());
        assert!(
            bg.luminance() < 0.5,
            "a dark theme should default to a dark background"
        );

        let t = theme(r##"{"type": "light", "colors": {}}"##);
        let bg = t.color("editor.background").unwrap();
        assert!(
            bg.luminance() > 0.5,
            "a light theme should default to a light background"
        );
    }

    #[test]
    fn colours_fall_back_through_the_derivation_chain() {
        let t = theme(r##"{"colors": {"editor.foreground": "#abcdef"}}"##);
        // Not defined by the theme, but derived from editor.foreground.
        assert_eq!(
            t.color("editorLineNumber.activeForeground"),
            Some(Rgba::rgb(0xab, 0xcd, 0xef))
        );
    }

    #[test]
    fn an_unknown_colour_key_has_no_value() {
        let t = theme(r##"{"colors": {}}"##);
        assert_eq!(t.color("nonsense.key"), None);
    }

    #[test]
    fn token_rules_style_matching_scopes() {
        let t = theme(
            r##"{
                "tokenColors": [
                    { "scope": "keyword", "settings": { "foreground": "#ff0000" } }
                ]
            }"##,
        );
        let style = t.style_for_scopes(&["source.rust", "keyword.control.rust"]);
        assert_eq!(style.foreground, Some(Rgba::rgb(255, 0, 0)));
    }

    #[test]
    fn a_scopeless_rule_is_the_baseline_style() {
        let t = theme(
            r##"{
                "tokenColors": [
                    { "settings": { "foreground": "#cccccc" } },
                    { "scope": "keyword", "settings": { "fontStyle": "bold" } }
                ]
            }"##,
        );
        let style = t.style_for_scopes(&["source.rust", "keyword.control"]);
        assert_eq!(style.foreground, Some(Rgba::rgb(0xcc, 0xcc, 0xcc)));
        assert_eq!(
            style.font_style,
            Some(FontStyle {
                bold: true,
                ..FontStyle::NONE
            })
        );
    }

    #[test]
    fn broad_and_narrow_rules_combine_by_specificity() {
        let t = theme(
            r##"{
                "tokenColors": [
                    { "scope": "entity", "settings": { "fontStyle": "italic" } },
                    { "scope": "entity.name.function", "settings": { "foreground": "#00ff00" } }
                ]
            }"##,
        );
        let style = t.style_for_scopes(&["source.rust", "entity.name.function.rust"]);
        assert_eq!(style.foreground, Some(Rgba::rgb(0, 255, 0)));
        assert_eq!(
            style.font_style,
            Some(FontStyle {
                italic: true,
                ..FontStyle::NONE
            })
        );
    }

    #[test]
    fn a_more_specific_rule_wins_a_conflict_regardless_of_order() {
        let source = r##"{
            "tokenColors": [
                { "scope": "entity.name.function", "settings": { "foreground": "#00ff00" } },
                { "scope": "entity", "settings": { "foreground": "#ff0000" } }
            ]
        }"##;
        let style = theme(source).style_for_scopes(&["source.rust", "entity.name.function"]);
        assert_eq!(style.foreground, Some(Rgba::rgb(0, 255, 0)));
    }

    #[test]
    fn the_later_of_two_equally_specific_rules_wins() {
        let t = theme(
            r##"{
                "tokenColors": [
                    { "scope": "keyword", "settings": { "foreground": "#ff0000" } },
                    { "scope": "keyword", "settings": { "foreground": "#0000ff" } }
                ]
            }"##,
        );
        let style = t.style_for_scopes(&["source", "keyword.control"]);
        assert_eq!(style.foreground, Some(Rgba::rgb(0, 0, 255)));
    }

    #[test]
    fn a_non_matching_scope_gets_the_default_style() {
        let t = theme(
            r##"{
                "colors": { "editor.foreground": "#d4d4d4" },
                "tokenColors": [{ "scope": "keyword", "settings": { "foreground": "#ff0000" } }]
            }"##,
        );
        let style = t.style_for_scopes(&["source.rust", "variable.other"]);
        assert_eq!(style.foreground, Some(Rgba::rgb(0xd4, 0xd4, 0xd4)));
    }

    #[test]
    fn include_chains_load_base_first() {
        let mut read = reader(vec![
            (
                "/themes/base.json",
                r##"{
                    "name": "Base",
                    "type": "dark",
                    "colors": { "editor.background": "#000000", "editor.foreground": "#ffffff" },
                    "tokenColors": [{ "scope": "keyword", "settings": { "foreground": "#ff0000" } }]
                }"##,
            ),
            (
                "/themes/derived.json",
                r##"{
                    "name": "Derived",
                    "include": "./base.json",
                    "colors": { "editor.background": "#111111" }
                }"##,
            ),
        ]);
        let t = ColorTheme::load(Path::new("/themes/derived.json"), &mut read).unwrap();

        assert_eq!(t.name, "Derived");
        // Overridden by the including file...
        assert_eq!(
            t.raw_color("editor.background"),
            Some(Rgba::rgb(0x11, 0x11, 0x11))
        );
        // ...but everything it did not mention comes from the base.
        assert_eq!(t.raw_color("editor.foreground"), Some(Rgba::WHITE));
        assert_eq!(t.kind, ThemeKind::Dark);
        let style = t.style_for_scopes(&["source", "keyword"]);
        assert_eq!(style.foreground, Some(Rgba::rgb(255, 0, 0)));
    }

    #[test]
    fn include_paths_resolve_relative_to_the_including_file() {
        let mut read = reader(vec![
            (
                "/a/shared/base.json",
                r##"{"colors": {"editor.foreground": "#010203"}}"##,
            ),
            (
                "/a/themes/leaf.json",
                r##"{"include": "../shared/base.json"}"##,
            ),
        ]);
        let t = ColorTheme::load(Path::new("/a/themes/leaf.json"), &mut read).unwrap();
        assert_eq!(t.raw_color("editor.foreground"), Some(Rgba::rgb(1, 2, 3)));
    }

    #[test]
    fn a_null_colour_removes_an_inherited_one() {
        let mut read = reader(vec![
            (
                "/t/base.json",
                r##"{"colors": {"editor.background": "#123456"}}"##,
            ),
            (
                "/t/leaf.json",
                r##"{"include": "./base.json", "colors": {"editor.background": null}}"##,
            ),
        ]);
        let t = ColorTheme::load(Path::new("/t/leaf.json"), &mut read).unwrap();
        assert_eq!(t.raw_color("editor.background"), None);
    }

    #[test]
    fn an_include_cycle_is_reported_rather_than_hanging() {
        let mut read = reader(vec![
            ("/t/a.json", r##"{"include": "./b.json"}"##),
            ("/t/b.json", r##"{"include": "./a.json"}"##),
        ]);
        let err = ColorTheme::load(Path::new("/t/a.json"), &mut read).unwrap_err();
        assert!(
            matches!(err, ThemeError::IncludeCycle { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_self_include_is_a_cycle() {
        let mut read = reader(vec![("/t/a.json", r##"{"include": "./a.json"}"##)]);
        let err = ColorTheme::load(Path::new("/t/a.json"), &mut read).unwrap_err();
        assert!(
            matches!(err, ThemeError::IncludeCycle { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn include_paths_are_normalised_before_the_cycle_check() {
        // The same file reached by two spellings must still be seen as one.
        let mut read = reader(vec![
            ("/t/a.json", r##"{"include": "./sub/../b.json"}"##),
            ("/t/b.json", r##"{"include": "./a.json"}"##),
        ]);
        let err = ColorTheme::load(Path::new("/t/a.json"), &mut read).unwrap_err();
        assert!(
            matches!(err, ThemeError::IncludeCycle { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_missing_include_is_reported_with_its_path() {
        let mut read = reader(vec![("/t/a.json", r##"{"include": "./missing.json"}"##)]);
        let err = ColorTheme::load(Path::new("/t/a.json"), &mut read).unwrap_err();
        match err {
            ThemeError::Read { path, .. } => assert_eq!(path, PathBuf::from("/t/missing.json")),
            other => panic!("expected a read error, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_theme_is_reported() {
        assert!(matches!(
            ColorTheme::from_json("{ this is not json }"),
            Err(ThemeError::Parse { .. })
        ));
        assert!(matches!(
            ColorTheme::from_json("[1, 2]"),
            Err(ThemeError::NotATheme { .. })
        ));
    }

    #[test]
    fn semantic_colours_accept_both_shorthand_and_object_form() {
        let t = theme(
            r##"{
                "semanticHighlighting": true,
                "semanticTokenColors": {
                    "variable": "#aabbcc",
                    "function.declaration": { "foreground": "#ddeeff", "fontStyle": "bold" }
                }
            }"##,
        );
        assert!(t.semantic_highlighting());

        let style = t
            .style_for_semantic(&SemanticToken::new("variable", &[], None))
            .unwrap();
        assert_eq!(style.foreground, Some(Rgba::rgb(0xaa, 0xbb, 0xcc)));

        let style = t
            .style_for_semantic(&SemanticToken::new("function", &["declaration"], None))
            .unwrap();
        assert_eq!(style.foreground, Some(Rgba::rgb(0xdd, 0xee, 0xff)));
        assert_eq!(
            style.font_style,
            Some(FontStyle {
                bold: true,
                ..FontStyle::NONE
            })
        );
    }

    #[test]
    fn an_unmatched_semantic_token_returns_none() {
        let t = theme(r##"{"semanticTokenColors": {"variable": "#aabbcc"}}"##);
        assert!(t
            .style_for_semantic(&SemanticToken::new("keyword", &[], None))
            .is_none());
    }
}
