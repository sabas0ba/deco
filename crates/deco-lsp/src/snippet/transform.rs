//! Snippet transforms: `/regex/format/options` after a tab stop or variable.
//!
//! The grammar is the one in the LSP 3.17 specification:
//!
//! ```text
//! transform ::= '/' regex '/' (format | text)+ '/' options
//! format    ::= '$' int | '${' int '}'
//!             | '${' int ':' '/upcase' | '/downcase' | '/capitalize'
//!                        | '/camelcase' | '/pascalcase' '}'
//!             | '${' int ':+' if '}'
//!             | '${' int ':?' if ':' else '}'
//!             | '${' int ':-' else '}' | '${' int ':' else '}'
//! ```
//!
//! The regular expression is compiled by the `regex` crate, which has no
//! look-around and no backreferences. A pattern using them does not compile,
//! and the snippet is refused.

/// A compiled transform.
#[derive(Debug, Clone)]
pub struct Transform {
    /// The transform as written, for comparison and debugging.
    source: String,
    regex: regex::Regex,
    format: Vec<Part>,
    /// The `g` option: replace every match rather than the first.
    global: bool,
}

impl PartialEq for Transform {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Eq for Transform {}

#[derive(Debug, Clone)]
enum Part {
    Text(String),
    Group { index: usize, how: How },
}

#[derive(Debug, Clone)]
enum How {
    /// The group's text as it is.
    Plain,
    Upcase,
    Downcase,
    /// The first character in upper case, the rest unchanged.
    Capitalize,
    /// Words of letters and digits, in any script, joined, every word after
    /// the first starting in upper case and the first in lower case.
    CamelCase,
    /// As `CamelCase`, with the first word in upper case too.
    PascalCase,
    /// `if_set` when the group matched non-empty text, else `otherwise`.
    Choose {
        if_set: String,
        otherwise: String,
    },
    /// The group's text, or `otherwise` when it is empty.
    OrElse {
        otherwise: String,
    },
}

impl Transform {
    /// Applies the transform to `value`.
    ///
    /// Text the regular expression does not match is kept, as in VS Code.
    /// When it matches nothing and the format has an `else` text, the format
    /// is expanded with every group empty instead, so that the `else` text is
    /// used: `${1:-fallback}` gives `fallback` for a value that does not
    /// match.
    pub fn apply(&self, value: &str) -> String {
        if !self.regex.is_match(value) {
            if !self.format.iter().any(Part::has_else) {
                return value.to_owned();
            }
            return self
                .format
                .iter()
                .map(|part| match part {
                    Part::Text(text) => text.clone(),
                    Part::Group { how, .. } => how.apply(""),
                })
                .collect();
        }
        let expand = |captures: &regex::Captures<'_>| {
            let mut out = String::new();
            for part in &self.format {
                match part {
                    Part::Text(text) => out.push_str(text),
                    Part::Group { index, how } => {
                        let group = captures.get(*index).map_or("", |m| m.as_str());
                        out.push_str(&how.apply(group));
                    }
                }
            }
            out
        };
        if self.global {
            self.regex.replace_all(value, expand).into_owned()
        } else {
            self.regex.replace(value, expand).into_owned()
        }
    }
}

impl Part {
    /// Whether this part has a non-empty text for a group that is empty.
    fn has_else(&self) -> bool {
        match self {
            Self::Group {
                how: How::Choose { otherwise, .. } | How::OrElse { otherwise },
                ..
            } => !otherwise.is_empty(),
            _ => false,
        }
    }
}

impl How {
    fn apply(&self, group: &str) -> String {
        match self {
            Self::Plain => group.to_owned(),
            Self::Upcase => group.to_uppercase(),
            Self::Downcase => group.to_lowercase(),
            Self::Capitalize => {
                let mut chars = group.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                }
            }
            Self::CamelCase | Self::PascalCase => {
                let words: Vec<&str> = group
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|word| !word.is_empty())
                    .collect();
                if words.is_empty() {
                    return group.to_owned();
                }
                let mut out = String::new();
                for (at, word) in words.iter().enumerate() {
                    let mut chars = word.chars();
                    let Some(first) = chars.next() else {
                        continue;
                    };
                    if at == 0 && matches!(self, Self::CamelCase) {
                        out.extend(first.to_lowercase());
                    } else {
                        out.extend(first.to_uppercase());
                    }
                    out.extend(chars);
                }
                out
            }
            Self::Choose { if_set, otherwise } => {
                if group.is_empty() {
                    otherwise.clone()
                } else {
                    if_set.clone()
                }
            }
            Self::OrElse { otherwise } => {
                if group.is_empty() {
                    otherwise.clone()
                } else {
                    group.to_owned()
                }
            }
        }
    }
}

/// Reads a transform from `chars`, starting just after its first `/` and
/// ending after the `}` that closes the enclosing `${…}`.
///
/// Returns `None` for malformed syntax, an unknown option or case
/// conversion, and a regular expression the `regex` crate cannot compile.
pub(super) fn parse(chars: &[char], at: &mut usize) -> Option<Transform> {
    let start = *at;
    let pattern = regex_source(chars, at)?;
    let format = format(chars, at)?;
    let mut builder = regex::RegexBuilder::new(&pattern);
    let mut global = false;
    loop {
        let c = *chars.get(*at)?;
        *at += 1;
        match c {
            '}' => break,
            'g' => global = true,
            'i' => {
                builder.case_insensitive(true);
            }
            'm' => {
                builder.multi_line(true);
            }
            's' => {
                builder.dot_matches_new_line(true);
            }
            // Unicode mode is always on in the `regex` crate.
            'u' => {}
            _ => return None,
        }
    }
    let regex = builder.build().ok()?;
    Some(Transform {
        source: chars[start..*at - 1].iter().collect(),
        regex,
        format,
        global,
    })
}

/// Reads the regular expression up to the `/` that ends it, which is
/// consumed. `\/` stands for `/`; every other escape is passed to the regular
/// expression unchanged.
fn regex_source(chars: &[char], at: &mut usize) -> Option<String> {
    let mut out = String::new();
    loop {
        let c = *chars.get(*at)?;
        *at += 1;
        match c {
            '/' => return Some(out),
            '\\' => {
                let escaped = *chars.get(*at)?;
                *at += 1;
                if escaped != '/' {
                    out.push('\\');
                }
                out.push(escaped);
            }
            c => out.push(c),
        }
    }
}

/// Reads the format up to the `/` that ends it, which is consumed.
fn format(chars: &[char], at: &mut usize) -> Option<Vec<Part>> {
    let mut parts = Vec::new();
    let mut text = String::new();
    loop {
        let c = *chars.get(*at)?;
        *at += 1;
        match c {
            '/' => break,
            '\\' => text.push(escaped(chars, at, &['$', '\\', '/', '}'])?),
            '$' => match group(chars, at)? {
                Some(part) => {
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(part);
                }
                None => text.push('$'),
            },
            c => text.push(c),
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Some(parts)
}

/// Reads what follows a `$` in a format. `Some(None)` means the `$` starts
/// nothing and is literal.
#[allow(clippy::option_option)]
fn group(chars: &[char], at: &mut usize) -> Option<Option<Part>> {
    let braced = chars.get(*at) == Some(&'{');
    if !braced {
        return Some(int(chars, at).map(|index| Part::Group {
            index,
            how: How::Plain,
        }));
    }
    *at += 1;
    let index = int(chars, at)?;
    let c = *chars.get(*at)?;
    *at += 1;
    let how = match c {
        '}' => How::Plain,
        ':' => match chars.get(*at)? {
            '/' => {
                *at += 1;
                let mut name = String::new();
                while let Some(c) = chars.get(*at).filter(|c| c.is_ascii_alphabetic()) {
                    name.push(*c);
                    *at += 1;
                }
                if chars.get(*at) != Some(&'}') {
                    return None;
                }
                *at += 1;
                match name.as_str() {
                    "upcase" => How::Upcase,
                    "downcase" => How::Downcase,
                    "capitalize" => How::Capitalize,
                    "camelcase" => How::CamelCase,
                    "pascalcase" => How::PascalCase,
                    _ => return None,
                }
            }
            '+' => {
                *at += 1;
                How::Choose {
                    if_set: until(chars, at, &['}'])?.0,
                    otherwise: String::new(),
                }
            }
            '?' => {
                *at += 1;
                let (if_set, end) = until(chars, at, &[':', '}'])?;
                if end != ':' {
                    return None;
                }
                How::Choose {
                    if_set,
                    otherwise: until(chars, at, &['}'])?.0,
                }
            }
            '-' => {
                *at += 1;
                How::OrElse {
                    otherwise: until(chars, at, &['}'])?.0,
                }
            }
            _ => How::OrElse {
                otherwise: until(chars, at, &['}'])?.0,
            },
        },
        _ => return None,
    };
    Some(Some(Part::Group { index, how }))
}

/// Reads text up to one of `ends`, which is consumed and returned. A
/// backslash escapes the next character when it is one of `ends`, `$` or `\`.
fn until(chars: &[char], at: &mut usize, ends: &[char]) -> Option<(String, char)> {
    let mut escapable = vec!['$', '\\'];
    escapable.extend_from_slice(ends);
    let mut out = String::new();
    loop {
        let c = *chars.get(*at)?;
        *at += 1;
        if ends.contains(&c) {
            return Some((out, c));
        }
        if c == '\\' {
            out.push(escaped(chars, at, &escapable)?);
            continue;
        }
        out.push(c);
    }
}

/// Reads the character after a backslash. One of `escapable` stands for
/// itself; any other character keeps the backslash, which is returned while
/// the character is read again as ordinary text.
fn escaped(chars: &[char], at: &mut usize, escapable: &[char]) -> Option<char> {
    let c = *chars.get(*at)?;
    if escapable.contains(&c) {
        *at += 1;
        Some(c)
    } else {
        Some('\\')
    }
}

fn int(chars: &[char], at: &mut usize) -> Option<usize> {
    let start = *at;
    while chars.get(*at).is_some_and(char::is_ascii_digit) {
        *at += 1;
    }
    chars[start..*at].iter().collect::<String>().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transform(source: &str) -> Option<Transform> {
        let chars: Vec<char> = source.chars().collect();
        let mut at = 0;
        let parsed = parse(&chars, &mut at)?;
        (at == chars.len()).then_some(parsed)
    }

    fn apply(source: &str, value: &str) -> String {
        transform(source)
            .unwrap_or_else(|| panic!("{source} should parse"))
            .apply(value)
    }

    #[test]
    fn a_group_is_inserted_and_unmatched_text_is_kept() {
        assert_eq!(apply("(\\w+)\\.rs/mod $1/}", "main.rs"), "mod main");
        assert_eq!(apply("a/b/}", "banana"), "bbnana");
        assert_eq!(apply("a/b/g}", "banana"), "bbnbnb");
    }

    #[test]
    fn case_conversions() {
        assert_eq!(apply("(.*)/${1:/upcase}/}", "héllo"), "HÉLLO");
        assert_eq!(apply("(.*)/${1:/downcase}/}", "HeLLo"), "hello");
        assert_eq!(
            apply("(.*)/${1:/capitalize}/}", "hello world"),
            "Hello world"
        );
        assert_eq!(
            apply("(.*)/${1:/camelcase}/}", "my-file_name"),
            "myFileName"
        );
        assert_eq!(
            apply("(.*)/${1:/pascalcase}/}", "my-file_name"),
            "MyFileName"
        );
        // Letters outside ASCII belong to words rather than separating them.
        assert_eq!(
            apply("(.*)/${1:/pascalcase}/}", "résumé_file"),
            "RésuméFile"
        );
        assert_eq!(apply("(.*)/${1:/camelcase}/}", "Été-2024"), "été2024");
    }

    #[test]
    fn an_else_text_is_used_when_nothing_matches() {
        assert_eq!(apply("^foo$/${1:-fallback}/}", "bar"), "fallback");
        assert_eq!(apply("^foo$/<${1:?yes:no}>/}", "bar"), "<no>");
        // Without an else text, a value that does not match is kept.
        assert_eq!(apply("^foo$/${1:/upcase}/}", "bar"), "bar");
    }

    #[test]
    fn conditionals_depend_on_whether_the_group_matched_text() {
        let source = "(a)?b/${1:+had a}${1:?yes:no}${1:-none}${1:else}/}";
        assert_eq!(apply(source, "ab"), "had ayesaa");
        assert_eq!(apply(source, "b"), "nononeelse");
    }

    #[test]
    fn options_and_escapes() {
        assert_eq!(apply("A/x/gi}", "aAa"), "xxx");
        // `\/` in the pattern is a slash; `\$` and `\/` in the format are
        // literal.
        assert_eq!(apply("\\//\\$\\//}", "a/b"), "a$/b");
        // Another escape in the pattern reaches the regular expression.
        assert_eq!(apply("\\d+/#/g}", "a1b22"), "a#b#");
    }

    #[test]
    fn unsupported_or_malformed_transforms_are_refused() {
        for source in [
            "(?=a)/x/}",
            "a/x/y}",
            "a/${1:/shout}/}",
            "a/${1:?only}/}",
            "a/x}",
            "(/x/}",
        ] {
            assert!(transform(source).is_none(), "{source}");
        }
    }
}
