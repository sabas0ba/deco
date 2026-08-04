//! TextMate scope selectors and the token styles themes attach to them.

use serde::Deserialize;

use crate::color::Rgba;

/// Which text decorations a token carries.
///
/// A theme rule may set `"fontStyle": ""`, which explicitly clears inherited
/// styles rather than leaving them alone — hence [`TokenStyle::font_style`]
/// being an `Option<FontStyle>` rather than a bare `FontStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FontStyle {
    /// Bold weight.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Underlined.
    pub underline: bool,
    /// Struck through.
    pub strikethrough: bool,
}

impl FontStyle {
    /// No decorations.
    pub const NONE: FontStyle = FontStyle {
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
    };

    /// Parses a space-separated `fontStyle` value.
    ///
    /// Unknown words are ignored: themes in the wild contain typos, and losing
    /// the whole rule over one would be worse than losing one decoration.
    pub fn parse(text: &str) -> Self {
        let mut style = FontStyle::NONE;
        for word in text.split_whitespace() {
            match word.to_ascii_lowercase().as_str() {
                "bold" => style.bold = true,
                "italic" => style.italic = true,
                "underline" => style.underline = true,
                "strikethrough" => style.strikethrough = true,
                _ => {}
            }
        }
        style
    }

    /// Whether any decoration is set.
    pub fn is_none(self) -> bool {
        self == FontStyle::NONE
    }
}

/// How a token should be drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenStyle {
    /// Text colour.
    pub foreground: Option<Rgba>,
    /// Background colour, which few themes set.
    pub background: Option<Rgba>,
    /// Decorations, or `None` to inherit.
    pub font_style: Option<FontStyle>,
}

impl TokenStyle {
    /// Whether the style says nothing at all.
    pub fn is_empty(&self) -> bool {
        self.foreground.is_none() && self.background.is_none() && self.font_style.is_none()
    }

    /// Overlays `other` on top of `self`, with `other` winning where it has an
    /// opinion.
    pub fn apply(&mut self, other: &TokenStyle) {
        if other.foreground.is_some() {
            self.foreground = other.foreground;
        }
        if other.background.is_some() {
            self.background = other.background;
        }
        if other.font_style.is_some() {
            self.font_style = other.font_style;
        }
    }
}

/// One dot-separated scope pattern, e.g. `entity.name.function`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopePattern {
    text: String,
    /// Number of dot-separated segments, which is the primary specificity term.
    depth: usize,
}

impl ScopePattern {
    /// Builds a pattern from its text.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            depth: text.split('.').count(),
        }
    }

    /// Whether `scope` is this pattern or a descendant of it.
    ///
    /// Matching is by whole segments: `string` matches `string.quoted` but not
    /// `stringly.typed`.
    pub fn matches(&self, scope: &str) -> bool {
        if self.text.is_empty() {
            return true;
        }
        if !scope.starts_with(&self.text) {
            return false;
        }
        matches!(scope.as_bytes().get(self.text.len()), None | Some(b'.'))
    }

    /// The pattern text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Number of dot-separated segments.
    pub fn depth(&self) -> usize {
        self.depth
    }
}

/// A descendant selector such as `meta.function entity.name`.
///
/// The final pattern must match the token's own scope; the preceding patterns
/// must match ancestors, in order, with gaps allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSelector {
    parents: Vec<ScopePattern>,
    target: ScopePattern,
}

/// How well a selector matched, ordered so that a larger value wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Specificity {
    /// Segments in the final pattern — the dominant term, as in vscode-textmate.
    pub depth: usize,
    /// Number of ancestor patterns that had to match.
    pub parents: usize,
}

impl ScopeSelector {
    /// Parses a single selector.
    pub fn parse(text: &str) -> Option<Self> {
        let mut patterns: Vec<ScopePattern> =
            text.split_whitespace().map(ScopePattern::new).collect();
        let target = patterns.pop()?;
        Some(Self {
            parents: patterns,
            target,
        })
    }

    /// Parses a comma-separated list of selectors.
    pub fn parse_list(text: &str) -> Vec<Self> {
        text.split(',')
            .filter_map(|part| Self::parse(part.trim()))
            .collect()
    }

    /// Matches against a scope stack, outermost first.
    ///
    /// Returns the specificity of the match, or `None` if the selector does not
    /// apply. Only the innermost scope is eligible to satisfy the final
    /// pattern, which is what keeps a `string` rule from colouring a
    /// `punctuation.definition.string` token.
    pub fn matches(&self, stack: &[&str]) -> Option<Specificity> {
        let (&innermost, ancestors) = stack.split_last()?;
        if !self.target.matches(innermost) {
            return None;
        }

        // Walk the ancestors from nearest to furthest, consuming parent
        // patterns in the same direction. Gaps are allowed, so
        // `source.rust entity.name` matches through any intervening scopes.
        let mut remaining = self.parents.iter().rev();
        let mut wanted = remaining.next();
        for ancestor in ancestors.iter().rev() {
            let Some(pattern) = wanted else { break };
            if pattern.matches(ancestor) {
                wanted = remaining.next();
            }
        }
        if wanted.is_some() {
            return None;
        }
        Some(Specificity {
            depth: self.target.depth(),
            parents: self.parents.len(),
        })
    }

    /// The final pattern.
    pub fn target(&self) -> &ScopePattern {
        &self.target
    }

    /// The ancestor patterns, outermost first.
    pub fn parents(&self) -> &[ScopePattern] {
        &self.parents
    }
}

/// One entry of a theme's `tokenColors`.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenColorRule {
    /// The rule's human-readable name, if it had one.
    pub name: Option<String>,
    /// The selectors this rule applies to.
    pub scopes: Vec<ScopeSelector>,
    /// The style to apply.
    pub style: TokenStyle,
}

impl TokenColorRule {
    /// The best specificity with which this rule matches `stack`.
    pub fn matches(&self, stack: &[&str]) -> Option<Specificity> {
        self.scopes
            .iter()
            .filter_map(|selector| selector.matches(stack))
            .max()
    }
}

/// The raw shape of a `tokenColors` entry as themes write it.
#[derive(Debug, Deserialize)]
pub(crate) struct RawTokenColor {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub scope: Option<RawScope>,
    #[serde(default)]
    pub settings: RawSettings,
}

/// `scope` may be a single string, a comma-separated string, or an array.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum RawScope {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawSettings {
    #[serde(default)]
    pub foreground: Option<Rgba>,
    #[serde(default)]
    pub background: Option<Rgba>,
    #[serde(default, rename = "fontStyle")]
    pub font_style: Option<String>,
}

impl RawTokenColor {
    /// Converts to a rule, or `None` if it selects nothing.
    ///
    /// A rule with no `scope` is the theme's default style for all text; the
    /// caller handles that case, so it is reported as `None` here.
    pub(crate) fn into_rule(self) -> Option<TokenColorRule> {
        let scopes = match self.scope? {
            RawScope::One(text) => ScopeSelector::parse_list(&text),
            RawScope::Many(items) => items
                .iter()
                .flat_map(|item| ScopeSelector::parse_list(item))
                .collect(),
        };
        if scopes.is_empty() {
            return None;
        }
        Some(TokenColorRule {
            name: self.name,
            scopes,
            style: self.settings.into_style(),
        })
    }
}

impl RawSettings {
    pub(crate) fn into_style(self) -> TokenStyle {
        TokenStyle {
            foreground: self.foreground,
            background: self.background,
            font_style: self.font_style.as_deref().map(FontStyle::parse),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(text: &str) -> ScopeSelector {
        ScopeSelector::parse(text).unwrap()
    }

    #[test]
    fn font_style_parses_each_word() {
        let style = FontStyle::parse("bold italic");
        assert!(style.bold && style.italic);
        assert!(!style.underline && !style.strikethrough);
        assert_eq!(
            FontStyle::parse("underline strikethrough"),
            FontStyle {
                underline: true,
                strikethrough: true,
                ..FontStyle::NONE
            }
        );
    }

    #[test]
    fn an_empty_font_style_clears_decorations() {
        assert_eq!(FontStyle::parse(""), FontStyle::NONE);
        assert!(FontStyle::parse("").is_none());
    }

    #[test]
    fn unknown_font_style_words_are_ignored() {
        assert_eq!(
            FontStyle::parse("bold wobbly"),
            FontStyle {
                bold: true,
                ..FontStyle::NONE
            }
        );
    }

    #[test]
    fn scope_patterns_match_whole_segments() {
        let pattern = ScopePattern::new("string");
        assert!(pattern.matches("string"));
        assert!(pattern.matches("string.quoted.double"));
        assert!(!pattern.matches("stringly.typed"));
        assert!(!pattern.matches("meta.string"));
    }

    #[test]
    fn a_selector_matches_the_innermost_scope() {
        let stack = [
            "source.rust",
            "meta.function.rust",
            "entity.name.function.rust",
        ];
        assert!(sel("entity.name.function").matches(&stack).is_some());
        assert!(sel("entity.name").matches(&stack).is_some());
        assert!(sel("entity").matches(&stack).is_some());
        // An ancestor scope is not eligible to satisfy the final pattern.
        assert!(sel("meta.function").matches(&stack).is_none());
    }

    #[test]
    fn descendant_selectors_check_ancestors() {
        let stack = [
            "source.rust",
            "meta.function.rust",
            "entity.name.function.rust",
        ];
        assert!(sel("meta.function entity.name").matches(&stack).is_some());
        assert!(sel("source.rust entity.name").matches(&stack).is_some());
        assert!(sel("source.js entity.name").matches(&stack).is_none());
    }

    #[test]
    fn descendant_selectors_allow_gaps() {
        let stack = ["source.rust", "meta.block", "meta.function", "entity.name"];
        // `source.rust` and `entity.name` are not adjacent.
        assert!(sel("source.rust entity.name").matches(&stack).is_some());
    }

    #[test]
    fn descendant_selectors_require_ancestor_order() {
        let stack = ["source.rust", "meta.function", "entity.name"];
        assert!(sel("source.rust meta.function entity.name")
            .matches(&stack)
            .is_some());
        // Reversing the ancestors must not match.
        assert!(sel("meta.function source.rust entity.name")
            .matches(&stack)
            .is_none());
    }

    #[test]
    fn specificity_prefers_a_deeper_target() {
        let stack = ["source.rust", "entity.name.function.rust"];
        let shallow = sel("entity").matches(&stack).unwrap();
        let deep = sel("entity.name.function").matches(&stack).unwrap();
        assert!(deep > shallow);
    }

    #[test]
    fn specificity_prefers_more_parents_at_equal_depth() {
        let stack = ["source.rust", "meta.function", "entity.name"];
        let bare = sel("entity.name").matches(&stack).unwrap();
        let parented = sel("meta.function entity.name").matches(&stack).unwrap();
        assert!(parented > bare);
    }

    #[test]
    fn depth_outranks_parent_count() {
        // vscode-textmate scores target depth first, so a deeper bare selector
        // beats a shallower one with a parent constraint.
        let stack = ["source.rust", "meta.function", "entity.name.function"];
        let deep_bare = sel("entity.name.function").matches(&stack).unwrap();
        let shallow_parented = sel("meta.function entity.name").matches(&stack).unwrap();
        assert!(deep_bare > shallow_parented);
    }

    #[test]
    fn comma_separated_selectors_parse_into_a_list() {
        let list = ScopeSelector::parse_list("keyword, storage.type , entity.name");
        assert_eq!(list.len(), 3);
        assert_eq!(list[1].target().text(), "storage.type");
    }

    #[test]
    fn an_empty_selector_list_yields_nothing() {
        assert!(ScopeSelector::parse_list("  ,  ").is_empty());
        assert!(ScopeSelector::parse("   ").is_none());
    }

    #[test]
    fn a_rule_reports_its_best_matching_selector() {
        let rule = TokenColorRule {
            name: None,
            scopes: ScopeSelector::parse_list("entity, entity.name.function"),
            style: TokenStyle::default(),
        };
        let score = rule
            .matches(&["source.rust", "entity.name.function"])
            .unwrap();
        assert_eq!(score.depth, 3);
    }

    #[test]
    fn styles_overlay_only_what_they_set() {
        let mut base = TokenStyle {
            foreground: Some(Rgba::WHITE),
            background: None,
            font_style: Some(FontStyle {
                bold: true,
                ..FontStyle::NONE
            }),
        };
        base.apply(&TokenStyle {
            foreground: Some(Rgba::BLACK),
            ..TokenStyle::default()
        });
        assert_eq!(base.foreground, Some(Rgba::BLACK));
        // The font style had no opinion in the overlay, so it survives.
        assert_eq!(
            base.font_style,
            Some(FontStyle {
                bold: true,
                ..FontStyle::NONE
            })
        );
    }

    #[test]
    fn an_explicit_empty_font_style_overrides_an_inherited_one() {
        let mut base = TokenStyle {
            font_style: Some(FontStyle {
                italic: true,
                ..FontStyle::NONE
            }),
            ..TokenStyle::default()
        };
        base.apply(&TokenStyle {
            font_style: Some(FontStyle::NONE),
            ..TokenStyle::default()
        });
        assert_eq!(base.font_style, Some(FontStyle::NONE));
    }

    #[test]
    fn raw_token_colors_accept_all_three_scope_spellings() {
        let single: RawTokenColor = serde_json::from_str(
            r##"{"scope": "keyword", "settings": {"foreground": "#ff0000"}}"##,
        )
        .unwrap();
        assert_eq!(single.into_rule().unwrap().scopes.len(), 1);

        let comma: RawTokenColor =
            serde_json::from_str(r##"{"scope": "keyword, storage", "settings": {}}"##).unwrap();
        assert_eq!(comma.into_rule().unwrap().scopes.len(), 2);

        let array: RawTokenColor =
            serde_json::from_str(r##"{"scope": ["keyword", "storage, entity"], "settings": {}}"##)
                .unwrap();
        assert_eq!(array.into_rule().unwrap().scopes.len(), 3);
    }

    #[test]
    fn a_rule_without_a_scope_is_not_a_selector_rule() {
        let raw: RawTokenColor =
            serde_json::from_str(r##"{"settings": {"foreground": "#ffffff"}}"##).unwrap();
        assert!(raw.into_rule().is_none());
    }
}
