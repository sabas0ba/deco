//! VS Code `when` clauses: parsing and evaluation.
//!
//! A `when` clause decides whether a keybinding, menu item or view is active.
//! The language is small but has real semantics that matter for compatibility:
//! `&&` binds tighter than `||`, comparisons are JavaScript's loose equality,
//! and a bare key is a truthiness test rather than an existence test.
//!
//! ```
//! use deco_keymap::when::{ContextKeys, WhenExpr};
//!
//! let expr = WhenExpr::parse("editorTextFocus && !editorReadonly").unwrap();
//! let mut ctx = ContextKeys::new();
//! ctx.set("editorTextFocus", true);
//! assert!(expr.evaluate(&ctx));
//! ctx.set("editorReadonly", true);
//! assert!(!expr.evaluate(&ctx));
//! ```

use std::collections::HashMap;
use std::fmt;

use regex::Regex;
use serde_json::Value;

/// The values a `when` clause is evaluated against.
///
/// Keys are set by the editor as state changes (focus moves, a selection
/// appears, a language is detected) and by extensions via `setContext`.
#[derive(Debug, Clone, Default)]
pub struct ContextKeys {
    values: HashMap<String, Value>,
}

impl ContextKeys {
    /// An empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// A context seeded with the platform keys VS Code always defines.
    pub fn with_platform_defaults() -> Self {
        let mut ctx = Self::new();
        ctx.set("isLinux", cfg!(target_os = "linux"));
        ctx.set("isMac", cfg!(target_os = "macos"));
        ctx.set("isWindows", cfg!(target_os = "windows"));
        ctx.set("isWeb", false);
        ctx
    }

    /// Sets a key.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.values.insert(key.into(), value.into());
    }

    /// Removes a key, making it undefined (and therefore falsy).
    pub fn remove(&mut self, key: &str) {
        self.values.remove(key);
    }

    /// Reads a key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// Number of defined keys.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether no keys are defined.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// JavaScript truthiness, which is what VS Code's bare-key tests use.
///
/// An undefined key, `null`, `false`, `0` and `""` are falsy; everything else,
/// including an empty array or object, is truthy.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// Coerces a value to a number the way JavaScript's `==` does.
fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) if s.trim().is_empty() => Some(0.0),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Null => Some(0.0),
        _ => None,
    }
}

/// JavaScript's `==` restricted to the types a context key can hold.
///
/// Loose rather than strict equality is required for compatibility: a clause
/// like `foo == 3` parses `3` as the string `"3"`, and must still match a
/// context key holding the number `3`.
fn loose_eq(left: Option<&Value>, right: &Value) -> bool {
    let left = match left {
        None => &Value::Null,
        Some(v) => v,
    };
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Array(_), _)
        | (Value::Object(_), _)
        | (_, Value::Array(_))
        | (_, Value::Object(_)) => false,
        _ => match (as_number(left), as_number(right)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        },
    }
}

/// Renders a value as the string a regex is matched against.
fn as_match_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// A compiled regex literal, comparable by its source so that expressions can
/// be compared for equality (which keybinding removal rules need).
#[derive(Debug, Clone)]
pub struct RegexLit {
    source: String,
    flags: String,
    regex: Regex,
}

impl RegexLit {
    /// The original `/pattern/flags` text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the pattern matches `text`.
    pub fn is_match(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }
}

impl PartialEq for RegexLit {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.flags == other.flags
    }
}

impl Eq for RegexLit {}

/// A numeric comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `<`
    Lt,
    /// `<=`
    Le,
}

impl CmpOp {
    fn apply(self, left: f64, right: f64) -> bool {
        match self {
            CmpOp::Gt => left > right,
            CmpOp::Ge => left >= right,
            CmpOp::Lt => left < right,
            CmpOp::Le => left <= right,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
        }
    }
}

/// A parsed `when` clause.
///
/// `PartialEq` but not `Eq`: numeric comparison operands are `f64`. Equality is
/// used to decide whether a `-command` removal entry targets a given binding,
/// which compares clauses that came from text, so this is exact in practice.
#[derive(Debug, Clone, PartialEq)]
pub enum WhenExpr {
    /// The literal `true` or `false`.
    Const(bool),
    /// A bare key: true when the key's value is truthy.
    Defined(String),
    /// `!expr`.
    Not(Box<WhenExpr>),
    /// `key == value`.
    Eq {
        /// Context key.
        key: String,
        /// Literal to compare against.
        value: Value,
    },
    /// `key != value`.
    Ne {
        /// Context key.
        key: String,
        /// Literal to compare against.
        value: Value,
    },
    /// `key =~ /pattern/`.
    Match {
        /// Context key.
        key: String,
        /// The regex literal.
        regex: RegexLit,
    },
    /// `key > value` and friends.
    Cmp {
        /// Context key.
        key: String,
        /// The operator.
        op: CmpOp,
        /// The numeric right-hand side.
        value: f64,
    },
    /// `key in collection`, where `collection` is another context key holding
    /// an array or object.
    In {
        /// The value to look for.
        key: String,
        /// The context key holding the collection.
        collection: String,
    },
    /// `key not in collection`.
    NotIn {
        /// The value to look for.
        key: String,
        /// The context key holding the collection.
        collection: String,
    },
    /// `a && b && …`.
    And(Vec<WhenExpr>),
    /// `a || b || …`.
    Or(Vec<WhenExpr>),
}

impl WhenExpr {
    /// Parses a `when` clause.
    pub fn parse(input: &str) -> Result<Self, WhenError> {
        let mut parser = Parser { input, pos: 0 };
        parser.skip_ws();
        if parser.at_end() {
            return Err(WhenError::Empty);
        }
        let expr = parser.parse_or()?;
        parser.skip_ws();
        if !parser.at_end() {
            return Err(WhenError::Trailing {
                input: input.to_owned(),
                position: parser.pos,
                rest: parser.input[parser.pos..].trim().to_owned(),
            });
        }
        Ok(expr)
    }

    /// Evaluates the clause against `ctx`.
    pub fn evaluate(&self, ctx: &ContextKeys) -> bool {
        match self {
            WhenExpr::Const(b) => *b,
            WhenExpr::Defined(key) => truthy(ctx.get(key)),
            WhenExpr::Not(inner) => !inner.evaluate(ctx),
            WhenExpr::Eq { key, value } => loose_eq(ctx.get(key), value),
            WhenExpr::Ne { key, value } => !loose_eq(ctx.get(key), value),
            WhenExpr::Match { key, regex } => regex.is_match(&as_match_text(ctx.get(key))),
            WhenExpr::Cmp { key, op, value } => ctx
                .get(key)
                .and_then(as_number)
                .map(|left| op.apply(left, *value))
                .unwrap_or(false),
            WhenExpr::In { key, collection } => in_collection(ctx, key, collection),
            WhenExpr::NotIn { key, collection } => !in_collection(ctx, key, collection),
            WhenExpr::And(terms) => terms.iter().all(|t| t.evaluate(ctx)),
            WhenExpr::Or(terms) => terms.iter().any(|t| t.evaluate(ctx)),
        }
    }

    /// Every context key this clause reads, which lets the editor recompute
    /// only the bindings a state change can affect.
    pub fn referenced_keys(&self, out: &mut Vec<String>) {
        match self {
            WhenExpr::Const(_) => {}
            WhenExpr::Defined(k)
            | WhenExpr::Eq { key: k, .. }
            | WhenExpr::Ne { key: k, .. }
            | WhenExpr::Match { key: k, .. }
            | WhenExpr::Cmp { key: k, .. } => out.push(k.clone()),
            WhenExpr::In { key, collection } | WhenExpr::NotIn { key, collection } => {
                out.push(key.clone());
                out.push(collection.clone());
            }
            WhenExpr::Not(inner) => inner.referenced_keys(out),
            WhenExpr::And(terms) | WhenExpr::Or(terms) => {
                for term in terms {
                    term.referenced_keys(out);
                }
            }
        }
    }
}

/// Resolves `key in collection`.
///
/// The left operand is looked up as a context key first (so `x in y` works when
/// `x` holds a value) and falls back to its own literal name, which is how
/// clauses like `resourceExtname in supportedExtensions` are written.
fn in_collection(ctx: &ContextKeys, key: &str, collection: &str) -> bool {
    let needle = match ctx.get(key) {
        Some(value) => scalar_text(value).into_owned(),
        None => key.to_owned(),
    };
    match ctx.get(collection) {
        // Members are compared by their scalar text, so a numeric member
        // matches a numeric needle the way loose equality would.
        Some(Value::Array(items)) => items
            .iter()
            .any(|item| scalar_text(item).as_ref() == needle.as_str()),
        Some(Value::Object(map)) => map.get(&needle).map(|v| truthy(Some(v))).unwrap_or(false),
        _ => false,
    }
}

/// A value's text form, borrowing when it is already a string.
fn scalar_text(value: &Value) -> std::borrow::Cow<'_, str> {
    match value {
        Value::String(s) => std::borrow::Cow::Borrowed(s),
        other => std::borrow::Cow::Owned(other.to_string()),
    }
}

impl fmt::Display for WhenExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WhenExpr::Const(b) => write!(f, "{b}"),
            WhenExpr::Defined(k) => f.write_str(k),
            WhenExpr::Not(inner) => match **inner {
                WhenExpr::And(_) | WhenExpr::Or(_) => write!(f, "!({inner})"),
                _ => write!(f, "!{inner}"),
            },
            WhenExpr::Eq { key, value } => write!(f, "{key} == {}", render_value(value)),
            WhenExpr::Ne { key, value } => write!(f, "{key} != {}", render_value(value)),
            WhenExpr::Match { key, regex } => write!(f, "{key} =~ {}", regex.source),
            WhenExpr::Cmp { key, op, value } => write!(f, "{key} {} {value}", op.as_str()),
            WhenExpr::In { key, collection } => write!(f, "{key} in {collection}"),
            WhenExpr::NotIn { key, collection } => write!(f, "{key} not in {collection}"),
            WhenExpr::And(terms) => write_joined(f, terms, " && "),
            WhenExpr::Or(terms) => write_joined(f, terms, " || "),
        }
    }
}

fn write_joined(f: &mut fmt::Formatter<'_>, terms: &[WhenExpr], sep: &str) -> fmt::Result {
    for (idx, term) in terms.iter().enumerate() {
        if idx > 0 {
            f.write_str(sep)?;
        }
        // Parenthesise only where precedence would otherwise be lost.
        let needs_parens = matches!((sep, term), (" && ", WhenExpr::Or(_)));
        if needs_parens {
            write!(f, "({term})")?;
        } else {
            write!(f, "{term}")?;
        }
    }
    Ok(())
}

fn render_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("'{s}'"),
        other => other.to_string(),
    }
}

/// Failure to parse a `when` clause.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WhenError {
    /// The clause was blank.
    #[error("empty when clause")]
    Empty,
    /// A context key was expected but something else was found.
    #[error("expected a context key at position {position} of `{input}`")]
    ExpectedKey {
        /// The clause.
        input: String,
        /// Byte offset of the problem.
        position: usize,
    },
    /// A closing parenthesis was missing.
    #[error("unclosed `(` at position {position} of `{input}`")]
    UnclosedParen {
        /// The clause.
        input: String,
        /// Byte offset of the `(`.
        position: usize,
    },
    /// Text remained after a complete expression.
    #[error("unexpected `{rest}` at position {position} of `{input}`")]
    Trailing {
        /// The clause.
        input: String,
        /// Byte offset of the leftover text.
        position: usize,
        /// The leftover text.
        rest: String,
    },
    /// A `=~` operand was not a `/pattern/` literal.
    #[error("expected a /regex/ after `=~` at position {position} of `{input}`")]
    ExpectedRegex {
        /// The clause.
        input: String,
        /// Byte offset of the problem.
        position: usize,
    },
    /// The regex literal did not compile.
    #[error("invalid regex `{literal}`: {message}")]
    InvalidRegex {
        /// The literal text.
        literal: String,
        /// The compiler's message.
        message: String,
    },
    /// A comparison operator was given a non-numeric operand.
    #[error("`{op}` needs a number, found `{found}` in `{input}`")]
    ExpectedNumber {
        /// The clause.
        input: String,
        /// The operator.
        op: String,
        /// What was found instead.
        found: String,
    },
    /// `not` was not followed by `in`.
    #[error("expected `in` after `not` at position {position} of `{input}`")]
    ExpectedIn {
        /// The clause.
        input: String,
        /// Byte offset of the problem.
        position: usize,
    },
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.rest().chars().next() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        if self.rest().starts_with(token) {
            self.pos += token.len();
            true
        } else {
            false
        }
    }

    /// Consumes `word` only when it stands alone, so `international` is not
    /// mistaken for the `in` operator.
    fn eat_word(&mut self, word: &str) -> bool {
        let rest = self.rest();
        if !rest.starts_with(word) {
            return false;
        }
        match rest[word.len()..].chars().next() {
            None => {}
            Some(c) if !is_key_char(c) => {}
            Some(_) => return false,
        }
        self.pos += word.len();
        true
    }

    fn parse_or(&mut self) -> Result<WhenExpr, WhenError> {
        let mut terms = vec![self.parse_and()?];
        loop {
            self.skip_ws();
            if self.eat("||") {
                terms.push(self.parse_and()?);
            } else {
                break;
            }
        }
        Ok(if terms.len() == 1 {
            terms.pop().expect("just checked")
        } else {
            WhenExpr::Or(terms)
        })
    }

    fn parse_and(&mut self) -> Result<WhenExpr, WhenError> {
        let mut terms = vec![self.parse_unary()?];
        loop {
            self.skip_ws();
            if self.eat("&&") {
                terms.push(self.parse_unary()?);
            } else {
                break;
            }
        }
        Ok(if terms.len() == 1 {
            terms.pop().expect("just checked")
        } else {
            WhenExpr::And(terms)
        })
    }

    fn parse_unary(&mut self) -> Result<WhenExpr, WhenError> {
        self.skip_ws();
        // `!` here is always negation: `!=` can only follow a key, and this
        // position never holds one.
        if self.rest().starts_with('!') && !self.rest().starts_with("!=") {
            self.pos += 1;
            return Ok(WhenExpr::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<WhenExpr, WhenError> {
        self.skip_ws();
        if self.rest().starts_with('(') {
            let open = self.pos;
            self.pos += 1;
            let inner = self.parse_or()?;
            self.skip_ws();
            if !self.eat(")") {
                return Err(WhenError::UnclosedParen {
                    input: self.input.to_owned(),
                    position: open,
                });
            }
            return Ok(inner);
        }

        let key_start = self.pos;
        let key = self.read_key();
        if key.is_empty() {
            return Err(WhenError::ExpectedKey {
                input: self.input.to_owned(),
                position: key_start,
            });
        }

        self.skip_ws();

        // Two-character operators must be tried before their one-character
        // prefixes.
        if self.eat("=~") {
            self.skip_ws();
            let regex = self.read_regex()?;
            return Ok(WhenExpr::Match { key, regex });
        }
        if self.eat("==") {
            return Ok(WhenExpr::Eq {
                key,
                value: self.read_value(),
            });
        }
        if self.eat("!=") {
            return Ok(WhenExpr::Ne {
                key,
                value: self.read_value(),
            });
        }
        for (token, op) in [
            (">=", CmpOp::Ge),
            ("<=", CmpOp::Le),
            (">", CmpOp::Gt),
            ("<", CmpOp::Lt),
        ] {
            if self.eat(token) {
                let raw = self.read_value();
                let value = as_number(&raw).ok_or_else(|| WhenError::ExpectedNumber {
                    input: self.input.to_owned(),
                    op: token.to_owned(),
                    found: render_value(&raw),
                })?;
                return Ok(WhenExpr::Cmp { key, op, value });
            }
        }
        if self.eat_word("not") {
            self.skip_ws();
            if !self.eat_word("in") {
                return Err(WhenError::ExpectedIn {
                    input: self.input.to_owned(),
                    position: self.pos,
                });
            }
            self.skip_ws();
            let collection = self.read_key();
            return Ok(WhenExpr::NotIn { key, collection });
        }
        if self.eat_word("in") {
            self.skip_ws();
            let collection = self.read_key();
            return Ok(WhenExpr::In { key, collection });
        }

        match key.as_str() {
            "true" => Ok(WhenExpr::Const(true)),
            "false" => Ok(WhenExpr::Const(false)),
            _ => Ok(WhenExpr::Defined(key)),
        }
    }

    fn read_key(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.rest().chars().next() {
            if is_key_char(c) {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        self.input[start..self.pos].to_owned()
    }

    /// Reads the right-hand side of `==`, `!=` or a comparison.
    ///
    /// VS Code reads everything up to the next `&&`, `||` or `)` and then
    /// deserializes it, which is why an unquoted value may contain spaces.
    fn read_value(&mut self) -> Value {
        self.skip_ws();
        let rest = self.rest();
        if let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') {
            if let Some(end) = rest[1..].find(quote) {
                let text = rest[1..1 + end].to_owned();
                self.pos += 1 + end + 1;
                return Value::String(text);
            }
        }

        let mut end = rest.len();
        for (idx, _) in rest.char_indices() {
            if rest[idx..].starts_with("&&") || rest[idx..].starts_with("||") {
                end = idx;
                break;
            }
            if rest[idx..].starts_with(')') {
                end = idx;
                break;
            }
        }
        let raw = rest[..end].trim();
        self.pos += end;

        match raw {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            // Numbers stay strings, exactly as VS Code's deserializer leaves
            // them; loose equality makes them compare correctly anyway.
            other => Value::String(other.to_owned()),
        }
    }

    fn read_regex(&mut self) -> Result<RegexLit, WhenError> {
        let start = self.pos;
        let rest = self.rest();
        if !rest.starts_with('/') {
            return Err(WhenError::ExpectedRegex {
                input: self.input.to_owned(),
                position: start,
            });
        }
        let bytes = rest.as_bytes();
        let mut idx = 1;
        let mut escaped = false;
        let mut close = None;
        while idx < bytes.len() {
            let b = bytes[idx];
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'/' {
                close = Some(idx);
                break;
            }
            idx += 1;
        }
        let Some(close) = close else {
            return Err(WhenError::ExpectedRegex {
                input: self.input.to_owned(),
                position: start,
            });
        };

        let pattern = &rest[1..close];
        let mut flags_end = close + 1;
        while flags_end < rest.len() && rest.as_bytes()[flags_end].is_ascii_alphabetic() {
            flags_end += 1;
        }
        let flags = &rest[close + 1..flags_end];
        self.pos += flags_end;

        // Rust's regex crate has no flags argument; `i` becomes an inline group.
        let compiled = if flags.contains('i') {
            format!("(?i){pattern}")
        } else {
            pattern.to_owned()
        };
        let regex = Regex::new(&compiled).map_err(|e| WhenError::InvalidRegex {
            literal: rest[..flags_end].to_owned(),
            message: e.to_string(),
        })?;
        Ok(RegexLit {
            source: rest[..flags_end].to_owned(),
            flags: flags.to_owned(),
            regex,
        })
    }
}

fn is_key_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(pairs: &[(&str, Value)]) -> ContextKeys {
        let mut ctx = ContextKeys::new();
        for (key, value) in pairs {
            ctx.set(*key, value.clone());
        }
        ctx
    }

    fn eval(expr: &str, pairs: &[(&str, Value)]) -> bool {
        WhenExpr::parse(expr).unwrap().evaluate(&ctx(pairs))
    }

    #[test]
    fn bare_key_is_a_truthiness_test() {
        assert!(eval("focus", &[("focus", json!(true))]));
        assert!(!eval("focus", &[("focus", json!(false))]));
        assert!(!eval("focus", &[]));
        assert!(!eval("focus", &[("focus", json!(null))]));
        assert!(!eval("focus", &[("focus", json!(0))]));
        assert!(!eval("focus", &[("focus", json!(""))]));
        assert!(eval("focus", &[("focus", json!("x"))]));
        assert!(eval("focus", &[("focus", json!([]))]));
    }

    #[test]
    fn negation_works() {
        assert!(eval("!focus", &[]));
        assert!(!eval("!focus", &[("focus", json!(true))]));
        assert!(eval("!!focus", &[("focus", json!(true))]));
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // Parsed as `a || (b && c)`; with only `a` set this must still be true.
        let expr = WhenExpr::parse("a || b && c").unwrap();
        assert!(matches!(expr, WhenExpr::Or(_)));
        assert!(expr.evaluate(&ctx(&[("a", json!(true))])));
        assert!(!expr.evaluate(&ctx(&[("b", json!(true))])));
        assert!(expr.evaluate(&ctx(&[("b", json!(true)), ("c", json!(true))])));
    }

    #[test]
    fn parentheses_override_precedence() {
        let expr = WhenExpr::parse("(a || b) && c").unwrap();
        assert!(matches!(expr, WhenExpr::And(_)));
        assert!(!expr.evaluate(&ctx(&[("a", json!(true))])));
        assert!(expr.evaluate(&ctx(&[("a", json!(true)), ("c", json!(true))])));
    }

    #[test]
    fn negated_parenthesised_groups_work() {
        assert!(eval("!(a && b)", &[("a", json!(true))]));
        assert!(!eval(
            "!(a && b)",
            &[("a", json!(true)), ("b", json!(true))]
        ));
    }

    #[test]
    fn equality_compares_strings() {
        assert!(eval(
            "editorLangId == rust",
            &[("editorLangId", json!("rust"))]
        ));
        assert!(!eval(
            "editorLangId == rust",
            &[("editorLangId", json!("go"))]
        ));
        assert!(!eval("editorLangId == rust", &[]));
    }

    #[test]
    fn equality_accepts_quoted_values() {
        assert!(eval(
            "view == 'my.view.id'",
            &[("view", json!("my.view.id"))]
        ));
        assert!(eval(
            r#"view == "my.view.id""#,
            &[("view", json!("my.view.id"))]
        ));
    }

    #[test]
    fn equality_is_loose_between_numbers_and_strings() {
        // The literal `3` is read as a string, so strict equality would fail
        // against a numeric context value.
        assert!(eval("count == 3", &[("count", json!(3))]));
        assert!(eval("count == 3", &[("count", json!("3"))]));
        assert!(!eval("count == 3", &[("count", json!(4))]));
    }

    #[test]
    fn equality_treats_true_and_false_as_booleans() {
        assert!(eval("flag == true", &[("flag", json!(true))]));
        assert!(eval("flag == false", &[("flag", json!(false))]));
        assert!(!eval("flag == true", &[("flag", json!(false))]));
    }

    #[test]
    fn inequality_is_the_negation_of_equality() {
        assert!(eval(
            "editorLangId != rust",
            &[("editorLangId", json!("go"))]
        ));
        assert!(!eval(
            "editorLangId != rust",
            &[("editorLangId", json!("rust"))]
        ));
        // An undefined key is not equal to a concrete value.
        assert!(eval("editorLangId != rust", &[]));
    }

    #[test]
    fn numeric_comparisons_work() {
        assert!(eval("count > 2", &[("count", json!(3))]));
        assert!(!eval("count > 3", &[("count", json!(3))]));
        assert!(eval("count >= 3", &[("count", json!(3))]));
        assert!(eval("count < 4", &[("count", json!(3))]));
        assert!(eval("count <= 3", &[("count", json!(3))]));
        // A missing or non-numeric key never satisfies a comparison.
        assert!(!eval("count > 2", &[]));
        assert!(!eval("count > 2", &[("count", json!("many"))]));
    }

    #[test]
    fn comparison_against_a_non_number_is_an_error() {
        assert!(matches!(
            WhenExpr::parse("count > lots"),
            Err(WhenError::ExpectedNumber { .. })
        ));
    }

    #[test]
    fn regex_match_works() {
        assert!(eval(
            "resourceFilename =~ /\\.tsx?$/",
            &[("resourceFilename", json!("main.ts"))]
        ));
        assert!(eval(
            "resourceFilename =~ /\\.tsx?$/",
            &[("resourceFilename", json!("main.tsx"))]
        ));
        assert!(!eval(
            "resourceFilename =~ /\\.tsx?$/",
            &[("resourceFilename", json!("main.rs"))]
        ));
    }

    #[test]
    fn regex_honours_the_i_flag() {
        assert!(eval("name =~ /abc/i", &[("name", json!("ABC"))]));
        assert!(!eval("name =~ /abc/", &[("name", json!("ABC"))]));
    }

    #[test]
    fn regex_matches_an_empty_string_for_a_missing_key() {
        assert!(eval("missing =~ /^$/", &[]));
        assert!(!eval("missing =~ /./", &[]));
    }

    #[test]
    fn an_unterminated_regex_is_an_error() {
        assert!(matches!(
            WhenExpr::parse("a =~ /abc"),
            Err(WhenError::ExpectedRegex { .. })
        ));
        assert!(matches!(
            WhenExpr::parse("a =~ abc"),
            Err(WhenError::ExpectedRegex { .. })
        ));
    }

    #[test]
    fn an_invalid_regex_is_reported() {
        assert!(matches!(
            WhenExpr::parse("a =~ /[/"),
            Err(WhenError::InvalidRegex { .. })
        ));
    }

    #[test]
    fn in_operator_searches_arrays() {
        let pairs = [("ext", json!(".ts")), ("supported", json!([".ts", ".js"]))];
        assert!(eval("ext in supported", &pairs));
        let pairs = [("ext", json!(".rs")), ("supported", json!([".ts", ".js"]))];
        assert!(!eval("ext in supported", &pairs));
    }

    #[test]
    fn in_operator_searches_object_keys() {
        let pairs = [
            ("ext", json!("ts")),
            ("supported", json!({"ts": true, "js": false})),
        ];
        assert!(eval("ext in supported", &pairs));
        let pairs = [
            ("ext", json!("js")),
            ("supported", json!({"ts": true, "js": false})),
        ];
        // A key mapped to a falsy value does not count as present.
        assert!(!eval("ext in supported", &pairs));
    }

    #[test]
    fn not_in_negates_in() {
        let pairs = [("ext", json!(".rs")), ("supported", json!([".ts"]))];
        assert!(eval("ext not in supported", &pairs));
        let pairs = [("ext", json!(".ts")), ("supported", json!([".ts"]))];
        assert!(!eval("ext not in supported", &pairs));
    }

    #[test]
    fn keys_beginning_with_operator_words_are_not_operators() {
        // `international` must not be read as `in` followed by `ternational`.
        let expr = WhenExpr::parse("international").unwrap();
        assert_eq!(expr, WhenExpr::Defined("international".into()));
        assert!(WhenExpr::parse("nothing")
            .unwrap()
            .evaluate(&ctx(&[("nothing", json!(true))])));
    }

    #[test]
    fn literal_true_and_false_are_constants() {
        assert!(eval("true", &[]));
        assert!(!eval("false", &[]));
        assert!(eval("true && a", &[("a", json!(true))]));
    }

    #[test]
    fn dotted_and_colon_keys_parse() {
        assert!(eval(
            "config.editor.wordWrap == on",
            &[("config.editor.wordWrap", json!("on"))]
        ));
        assert!(eval(
            "view:explorer.visible",
            &[("view:explorer.visible", json!(true))]
        ));
    }

    #[test]
    fn realistic_vscode_clauses_parse_and_evaluate() {
        let clause = "editorTextFocus && !editorReadonly && editorLangId == typescript";
        assert!(eval(
            clause,
            &[
                ("editorTextFocus", json!(true)),
                ("editorLangId", json!("typescript"))
            ]
        ));
        assert!(!eval(
            clause,
            &[
                ("editorTextFocus", json!(true)),
                ("editorReadonly", json!(true)),
                ("editorLangId", json!("typescript"))
            ]
        ));

        let clause = "!editorHasSelection && !editorHasMultipleSelections || terminalFocus";
        assert!(eval(clause, &[]));
        assert!(eval(
            clause,
            &[
                ("editorHasSelection", json!(true)),
                ("terminalFocus", json!(true))
            ]
        ));
        assert!(!eval(clause, &[("editorHasSelection", json!(true))]));
    }

    #[test]
    fn empty_clause_is_an_error() {
        assert_eq!(WhenExpr::parse("   "), Err(WhenError::Empty));
    }

    #[test]
    fn trailing_garbage_is_an_error() {
        assert!(matches!(
            WhenExpr::parse("a b"),
            Err(WhenError::Trailing { .. })
        ));
    }

    #[test]
    fn unclosed_parenthesis_is_an_error() {
        assert!(matches!(
            WhenExpr::parse("(a && b"),
            Err(WhenError::UnclosedParen { .. })
        ));
    }

    #[test]
    fn missing_key_is_an_error() {
        assert!(matches!(
            WhenExpr::parse("&& a"),
            Err(WhenError::ExpectedKey { .. })
        ));
    }

    #[test]
    fn referenced_keys_are_collected() {
        let expr = WhenExpr::parse("a && (b == 1 || c =~ /x/) && d in e").unwrap();
        let mut keys = Vec::new();
        expr.referenced_keys(&mut keys);
        keys.sort();
        assert_eq!(keys, ["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn display_round_trips_through_the_parser() {
        for clause in [
            "a",
            "!a",
            "a && b",
            "a || b",
            "a && (b || c)",
            "a == 'x'",
            "a != 'x'",
            "a > 3",
            "a =~ /x/i",
            "a in b",
            "a not in b",
            "!(a && b)",
        ] {
            let parsed = WhenExpr::parse(clause).unwrap();
            let printed = parsed.to_string();
            let reparsed = WhenExpr::parse(&printed)
                .unwrap_or_else(|e| panic!("`{clause}` printed as `{printed}` failed: {e}"));
            assert_eq!(parsed, reparsed, "`{clause}` printed as `{printed}`");
        }
    }

    #[test]
    fn platform_defaults_set_exactly_one_os_key() {
        let ctx = ContextKeys::with_platform_defaults();
        let set: Vec<_> = ["isLinux", "isMac", "isWindows"]
            .iter()
            .filter(|k| truthy(ctx.get(k)))
            .collect();
        assert_eq!(set.len(), 1, "expected exactly one OS key to be true");
    }
}
