//! Semantic token colouring (`semanticTokenColors`).
//!
//! Semantic tokens come from a language server and carry a type plus a set of
//! modifiers — `variable.readonly`, `function.declaration.async`. A theme
//! selects them with `type.modifier…[:language]`, where `*` stands in for any
//! type.

use crate::tokens::TokenStyle;

/// A semantic token to be styled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticToken<'a> {
    /// The token type, e.g. `variable`.
    pub token_type: &'a str,
    /// The token's modifiers, e.g. `["readonly", "static"]`.
    pub modifiers: &'a [&'a str],
    /// The document's language id, if known.
    pub language: Option<&'a str>,
}

impl<'a> SemanticToken<'a> {
    /// Builds a token.
    pub fn new(token_type: &'a str, modifiers: &'a [&'a str], language: Option<&'a str>) -> Self {
        Self {
            token_type,
            modifiers,
            language,
        }
    }
}

/// How well a selector matched. Larger wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemanticSpecificity(pub u32);

/// The wildcard token type.
const WILDCARD: &str = "*";

/// A parsed `semanticTokenColors` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticSelector {
    /// `None` when the selector used `*`.
    token_type: Option<String>,
    modifiers: Vec<String>,
    language: Option<String>,
}

impl SemanticSelector {
    /// Parses `type.mod1.mod2:language`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let (head, language) = match text.split_once(':') {
            Some((head, lang)) => {
                let lang = lang.trim();
                // `type:` with an empty language is a malformed selector, not a
                // selector that matches every language.
                if lang.is_empty() {
                    return None;
                }
                (head, Some(lang.to_owned()))
            }
            None => (text, None),
        };

        let mut parts = head.split('.');
        let token_type = parts.next()?.trim();
        if token_type.is_empty() {
            return None;
        }
        let modifiers: Vec<String> = parts
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned)
            .collect();

        Some(Self {
            token_type: (token_type != WILDCARD).then(|| token_type.to_owned()),
            modifiers,
            language,
        })
    }

    /// Scores this selector against `token`, or `None` if it does not apply.
    ///
    /// The weights mirror VS Code's: a concrete type and each required modifier
    /// are worth the same and add up, so `variable.readonly` outranks both
    /// `variable` and `*.readonly`. A language constraint is a small tiebreak
    /// on top.
    pub fn matches(&self, token: &SemanticToken<'_>) -> Option<SemanticSpecificity> {
        let mut score = 0u32;

        if let Some(language) = &self.language {
            if token.language != Some(language.as_str()) {
                return None;
            }
            score += 10;
        }

        if let Some(token_type) = &self.token_type {
            if token_type != token.token_type {
                return None;
            }
            score += 100;
        }

        for modifier in &self.modifiers {
            if !token.modifiers.iter().any(|m| m == modifier) {
                return None;
            }
        }
        score += self.modifiers.len() as u32 * 100;

        Some(SemanticSpecificity(score))
    }

    /// The selector's token type, or `None` for `*`.
    pub fn token_type(&self) -> Option<&str> {
        self.token_type.as_deref()
    }

    /// The modifiers the token must carry.
    pub fn modifiers(&self) -> &[String] {
        &self.modifiers
    }

    /// The language the selector is restricted to.
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }
}

/// A selector paired with the style it applies.
pub type SemanticRule = (SemanticSelector, TokenStyle);

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(text: &str) -> SemanticSelector {
        SemanticSelector::parse(text).unwrap()
    }

    #[test]
    fn parses_a_bare_type() {
        let s = sel("variable");
        assert_eq!(s.token_type(), Some("variable"));
        assert!(s.modifiers().is_empty());
        assert_eq!(s.language(), None);
    }

    #[test]
    fn parses_modifiers() {
        let s = sel("variable.readonly.static");
        assert_eq!(s.token_type(), Some("variable"));
        assert_eq!(s.modifiers(), ["readonly", "static"]);
    }

    #[test]
    fn parses_a_language_constraint() {
        let s = sel("variable.readonly:rust");
        assert_eq!(s.token_type(), Some("variable"));
        assert_eq!(s.modifiers(), ["readonly"]);
        assert_eq!(s.language(), Some("rust"));
    }

    #[test]
    fn parses_the_wildcard_type() {
        let s = sel("*.declaration");
        assert_eq!(s.token_type(), None);
        assert_eq!(s.modifiers(), ["declaration"]);
    }

    #[test]
    fn rejects_malformed_selectors() {
        assert!(SemanticSelector::parse("").is_none());
        assert!(SemanticSelector::parse("   ").is_none());
        assert!(SemanticSelector::parse("variable:").is_none());
        assert!(SemanticSelector::parse(":rust").is_none());
    }

    #[test]
    fn a_type_selector_matches_only_that_type() {
        let s = sel("variable");
        assert!(s
            .matches(&SemanticToken::new("variable", &[], None))
            .is_some());
        assert!(s
            .matches(&SemanticToken::new("function", &[], None))
            .is_none());
    }

    #[test]
    fn extra_token_modifiers_do_not_prevent_a_match() {
        let s = sel("variable");
        assert!(s
            .matches(&SemanticToken::new("variable", &["readonly"], None))
            .is_some());
    }

    #[test]
    fn every_selector_modifier_must_be_present() {
        let s = sel("variable.readonly.static");
        assert!(s
            .matches(&SemanticToken::new(
                "variable",
                &["readonly", "static", "x"],
                None
            ))
            .is_some());
        assert!(s
            .matches(&SemanticToken::new("variable", &["readonly"], None))
            .is_none());
    }

    #[test]
    fn the_wildcard_matches_any_type() {
        let s = sel("*.declaration");
        assert!(s
            .matches(&SemanticToken::new("variable", &["declaration"], None))
            .is_some());
        assert!(s
            .matches(&SemanticToken::new("function", &["declaration"], None))
            .is_some());
        assert!(s
            .matches(&SemanticToken::new("function", &[], None))
            .is_none());
    }

    #[test]
    fn a_language_constraint_must_match() {
        let s = sel("variable:rust");
        assert!(s
            .matches(&SemanticToken::new("variable", &[], Some("rust")))
            .is_some());
        assert!(s
            .matches(&SemanticToken::new("variable", &[], Some("go")))
            .is_none());
        assert!(s
            .matches(&SemanticToken::new("variable", &[], None))
            .is_none());
    }

    #[test]
    fn type_and_modifier_specificity_add_up() {
        let token = SemanticToken::new("variable", &["readonly"], Some("rust"));
        let bare_type = sel("variable").matches(&token).unwrap();
        let wildcard_mod = sel("*.readonly").matches(&token).unwrap();
        let both = sel("variable.readonly").matches(&token).unwrap();

        assert_eq!(
            bare_type, wildcard_mod,
            "a type and a modifier weigh the same"
        );
        assert!(both > bare_type);
        assert!(both > wildcard_mod);
    }

    #[test]
    fn a_language_constraint_is_only_a_tiebreak() {
        let token = SemanticToken::new("variable", &["readonly"], Some("rust"));
        let with_language = sel("variable:rust").matches(&token).unwrap();
        let with_modifier = sel("variable.readonly").matches(&token).unwrap();
        assert!(
            with_modifier > with_language,
            "a modifier outweighs a language"
        );

        let without_language = sel("variable").matches(&token).unwrap();
        assert!(with_language > without_language);
    }
}
