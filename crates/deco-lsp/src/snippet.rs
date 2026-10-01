//! LSP snippets: tab stops, placeholders, choices and variables.
//!
//! The grammar is the one in the LSP 3.17 specification ("Snippet Syntax"):
//!
//! ```text
//! any         ::= tabstop | placeholder | choice | variable | text
//! tabstop     ::= '$' int | '${' int '}'
//! placeholder ::= '${' int ':' any '}'
//! choice      ::= '${' int '|' text (',' text)* '|}'
//! variable    ::= '$' var | '${' var '}' | '${' var ':' any '}'
//! ```
//!
//! Transforms (`${1/regex/format/}` and `${VAR/regex/format/}`) are not
//! supported, and a snippet containing one is refused as a whole. So is a
//! variable the caller cannot resolve. A refused snippet is reported by
//! returning `None`, before anything is inserted, so the caller can fall back
//! to plain text.

use deco_core::{Position, Range};

/// Expanded text and its fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// Text to insert. All ranges are relative to its beginning, in UTF-16.
    pub text: String,
    /// Every field in the text, in document order. A parent precedes the
    /// fields nested in it.
    pub fields: Vec<Field>,
    /// Navigation order: one group per tab stop, in numeric order, with the
    /// final stop (`$0`, or the end of the text when absent) last. A group holds
    /// the positions in `fields` of every occurrence of that tab stop, in
    /// document order. Occurrences of one tab stop are edited together.
    pub stops: Vec<Vec<usize>>,
}

/// One occurrence of a tab stop in the expanded text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The tab stop's number. The final stop is 0.
    pub index: u32,
    /// Where the field is, relative to the beginning of the text.
    pub range: Range,
    /// The field this one is nested in, as a position in [`Snippet::fields`].
    pub parent: Option<usize>,
    /// The options of a choice, first one inserted. Empty for other fields.
    pub choices: Vec<String>,
}

impl Snippet {
    /// Parses a snippet that uses no variables.
    pub fn parse(source: &str) -> Option<Self> {
        Self::parse_with_variables(source, |_| None)
    }

    /// Parses a snippet, expanding variables with values from the insertion
    /// context.
    ///
    /// `None` from the resolver means the variable is unsupported, which
    /// refuses the snippet. `Some("")` means it is supported but unset, so its
    /// default is used when present. Resolved values are inserted as text and
    /// are never parsed as snippet syntax.
    ///
    /// Plain text with no field and no variable is refused too: it is not a
    /// snippet, and the caller inserts it as text.
    pub fn parse_with_variables(
        source: &str,
        resolve: impl FnMut(&str) -> Option<String>,
    ) -> Option<Self> {
        if has_other_line_break(source) {
            return None;
        }
        let mut parser = Parser {
            chars: source.chars().collect(),
            at: 0,
            resolve,
            found_markup: false,
        };
        let nodes = parser.any(false)?;
        if parser.at != parser.chars.len() {
            return None;
        }
        if !parser.found_markup {
            return None;
        }
        let definitions = definitions(&nodes)?;
        let mut builder = Builder {
            definitions,
            text: String::new(),
            position: Position::ZERO,
            fields: Vec::new(),
        };
        builder.render(&nodes, None, &mut Vec::new())?;
        builder.finish()
    }
}

/// A parsed piece of a snippet, with variables already resolved.
#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Field {
        index: u32,
        /// The placeholder's content; empty for a bare tab stop.
        content: Vec<Node>,
        /// Whether the source gave content, even an empty one (`${1:}`).
        defines: bool,
        choices: Vec<String>,
    },
}

struct Parser<F> {
    chars: Vec<char>,
    at: usize,
    resolve: F,
    /// Whether a field or a variable was seen.
    found_markup: bool,
}

impl<F: FnMut(&str) -> Option<String>> Parser<F> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += 1;
        Some(c)
    }

    /// Reads a sequence up to the end of the source or, when `nested`, up to
    /// the `}` that closes the enclosing placeholder, which is not consumed.
    fn any(&mut self, nested: bool) -> Option<Vec<Node>> {
        let mut nodes = Vec::new();
        let mut text = String::new();
        while let Some(c) = self.peek() {
            match c {
                '}' if nested => break,
                '\\' => {
                    self.at += 1;
                    match self.next() {
                        Some(escaped @ ('$' | '}' | '\\')) => text.push(escaped),
                        // Any other character after a backslash is literal,
                        // backslash included, as in VS Code.
                        Some(other) => {
                            text.push('\\');
                            text.push(other);
                        }
                        None => text.push('\\'),
                    }
                }
                '$' => {
                    self.at += 1;
                    match self.dollar()? {
                        Some(mut expanded) => {
                            if !text.is_empty() {
                                nodes.push(Node::Text(std::mem::take(&mut text)));
                            }
                            nodes.append(&mut expanded);
                        }
                        // Not markup: a literal dollar sign.
                        None => text.push('$'),
                    }
                }
                c => {
                    self.at += 1;
                    text.push(c);
                }
            }
        }
        if !text.is_empty() {
            nodes.push(Node::Text(text));
        }
        Some(nodes)
    }

    /// Reads what follows a `$`.
    ///
    /// The outer `None` refuses the snippet; `Some(None)` means the dollar
    /// sign starts no markup and is literal.
    #[allow(clippy::option_option)]
    fn dollar(&mut self) -> Option<Option<Vec<Node>>> {
        let braced = self.peek() == Some('{');
        let start = self.at;
        if braced {
            self.at += 1;
        }
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                let index = self.int()?;
                self.found_markup = true;
                if !braced {
                    return Some(Some(vec![field(index, Vec::new(), false, Vec::new())]));
                }
                match self.next()? {
                    '}' => Some(Some(vec![field(index, Vec::new(), false, Vec::new())])),
                    ':' => {
                        let content = self.any(true)?;
                        self.expect('}')?;
                        Some(Some(vec![field(index, content, true, Vec::new())]))
                    }
                    '|' => {
                        let choices = self.choices()?;
                        let first = Node::Text(choices[0].clone());
                        Some(Some(vec![field(index, vec![first], true, choices)]))
                    }
                    // A transform, or malformed.
                    _ => None,
                }
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some(c) = self
                    .peek()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    self.at += 1;
                    name.push(c);
                }
                self.found_markup = true;
                let value = (self.resolve)(&name)?;
                if has_other_line_break(&value) {
                    return None;
                }
                let mut default = None;
                if braced {
                    match self.next()? {
                        '}' => {}
                        ':' => {
                            default = Some(self.any(true)?);
                            self.expect('}')?;
                        }
                        // A transform, or malformed.
                        _ => return None,
                    }
                }
                Some(Some(match default {
                    Some(default) if value.is_empty() => default,
                    _ => vec![Node::Text(value)],
                }))
            }
            // `${` followed by anything else cannot be completed.
            _ if braced => None,
            _ => {
                self.at = start;
                Some(None)
            }
        }
    }

    fn int(&mut self) -> Option<u32> {
        let mut digits = String::new();
        while let Some(c) = self.peek().filter(char::is_ascii_digit) {
            self.at += 1;
            digits.push(c);
        }
        digits.parse().ok()
    }

    fn expect(&mut self, wanted: char) -> Option<()> {
        (self.next()? == wanted).then_some(())
    }

    /// Reads `a,b,c|}` after `${n|`.
    fn choices(&mut self) -> Option<Vec<String>> {
        let mut choices = Vec::new();
        let mut current = String::new();
        loop {
            match self.next()? {
                '\\' => match self.next()? {
                    escaped @ ('$' | '}' | '\\' | ',' | '|') => current.push(escaped),
                    other => {
                        current.push('\\');
                        current.push(other);
                    }
                },
                ',' => choices.push(std::mem::take(&mut current)),
                '|' => {
                    self.expect('}')?;
                    choices.push(current);
                    return Some(choices);
                }
                c => current.push(c),
            }
        }
    }
}

fn field(index: u32, content: Vec<Node>, defines: bool, choices: Vec<String>) -> Node {
    Node::Field {
        index,
        content,
        defines,
        choices,
    }
}

/// For each tab stop, the occurrence whose content every occurrence shows: the
/// first one that gives content, or the first one when none does.
///
/// Refuses a field nested in an occurrence of itself, which has no finite
/// expansion, and a final stop that appears twice or contains fields.
fn definitions(nodes: &[Node]) -> Option<std::collections::BTreeMap<u32, Node>> {
    fn walk(
        nodes: &[Node],
        found: &mut std::collections::BTreeMap<u32, Node>,
        finals: &mut usize,
        open: &mut Vec<u32>,
    ) -> Option<()> {
        for node in nodes {
            let Node::Field {
                index,
                content,
                defines,
                ..
            } = node
            else {
                continue;
            };
            if open.contains(index) {
                return None;
            }
            if *index == 0 {
                *finals += 1;
                if *finals > 1 || content.iter().any(|n| matches!(n, Node::Field { .. })) {
                    return None;
                }
            }
            match found.get(index) {
                Some(Node::Field { defines: true, .. }) => {}
                _ if *defines || !found.contains_key(index) => {
                    found.insert(*index, node.clone());
                }
                _ => {}
            }
            open.push(*index);
            walk(content, found, finals, open)?;
            open.pop();
        }
        Some(())
    }
    let mut found = std::collections::BTreeMap::new();
    walk(nodes, &mut found, &mut 0, &mut Vec::new())?;
    Some(found)
}

struct Builder {
    definitions: std::collections::BTreeMap<u32, Node>,
    text: String,
    position: Position,
    fields: Vec<Field>,
}

impl Builder {
    /// Appends `nodes`. `open` holds the tab stops being expanded, which
    /// detects a stop whose definition contains another occurrence of itself.
    fn render(&mut self, nodes: &[Node], parent: Option<usize>, open: &mut Vec<u32>) -> Option<()> {
        for node in nodes {
            match node {
                Node::Text(text) => self.append(text),
                Node::Field { index, .. } => {
                    if open.contains(index) {
                        return None;
                    }
                    let Some(Node::Field {
                        content, choices, ..
                    }) = self.definitions.get(index).cloned()
                    else {
                        return None;
                    };
                    let id = self.fields.len();
                    self.fields.push(Field {
                        index: *index,
                        range: Range::empty(self.position),
                        parent,
                        choices,
                    });
                    open.push(*index);
                    self.render(&content, Some(id), open)?;
                    open.pop();
                    self.fields[id].range.end = self.position;
                }
            }
        }
        Some(())
    }

    fn append(&mut self, text: &str) {
        for c in text.chars() {
            self.text.push(c);
            if c == '\n' {
                self.position.line += 1;
                self.position.character = 0;
            } else {
                self.position.character += c.len_utf16() as u32;
            }
        }
    }

    fn finish(mut self) -> Option<Snippet> {
        if !self.fields.iter().any(|field| field.index == 0) {
            self.fields.push(Field {
                index: 0,
                range: Range::empty(self.position),
                parent: None,
                choices: Vec::new(),
            });
        }
        let mut groups: std::collections::BTreeMap<u32, Vec<usize>> = Default::default();
        for (id, field) in self.fields.iter().enumerate() {
            groups.entry(field.index).or_default().push(id);
        }
        let last = groups.remove(&0)?;
        let mut stops: Vec<Vec<usize>> = groups.into_values().collect();
        stops.push(last);
        Some(Snippet {
            text: self.text,
            fields: self.fields,
            stops,
        })
    }
}

/// Whether `text` contains a line break other than `\n`.
///
/// Positions count only `\n` as a line break. Another one would make every
/// later position disagree with the buffer's, so such a snippet is refused.
fn has_other_line_break(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c,
            '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Range {
        Range::new(Position::new(l0, c0), Position::new(l1, c1))
    }

    /// The ranges of each stop, in navigation order.
    fn stop_ranges(snippet: &Snippet) -> Vec<Vec<Range>> {
        snippet
            .stops
            .iter()
            .map(|group| group.iter().map(|&id| snippet.fields[id].range).collect())
            .collect()
    }

    #[test]
    fn orders_numbers_and_places_zero_last() {
        let snippet = Snippet::parse("${2:b} ${1:a}$0").unwrap();
        assert_eq!(snippet.text, "b a");
        assert_eq!(
            stop_ranges(&snippet),
            vec![
                vec![range(0, 2, 0, 3)],
                vec![range(0, 0, 0, 1)],
                vec![Range::empty(Position::new(0, 3))],
            ]
        );
    }

    #[test]
    fn counts_utf16_and_multiline_defaults() {
        let snippet = Snippet::parse("😀${1:日本\n語}\\$\\}\\\\").unwrap();
        assert_eq!(snippet.text, "😀日本\n語$}\\");
        assert_eq!(
            stop_ranges(&snippet),
            vec![
                vec![range(0, 2, 1, 1)],
                vec![Range::empty(Position::new(1, 4))],
            ]
        );
    }

    #[test]
    fn a_repeated_index_is_one_stop_showing_the_first_default() {
        // The default may come from any occurrence; every occurrence shows it.
        let snippet = Snippet::parse("$1 = ${1:name}; use($1)").unwrap();
        assert_eq!(snippet.text, "name = name; use(name)");
        assert_eq!(
            stop_ranges(&snippet),
            vec![
                vec![range(0, 0, 0, 4), range(0, 7, 0, 11), range(0, 17, 0, 21)],
                vec![Range::empty(Position::new(0, 22))],
            ]
        );
    }

    #[test]
    fn a_nested_placeholder_is_a_field_inside_its_parent() {
        let snippet = Snippet::parse("${1:call(${2:arg})}$0").unwrap();
        assert_eq!(snippet.text, "call(arg)");
        assert_eq!(snippet.fields[0].range, range(0, 0, 0, 9));
        assert_eq!(snippet.fields[0].parent, None);
        assert_eq!(snippet.fields[1].range, range(0, 5, 0, 8));
        assert_eq!(snippet.fields[1].parent, Some(0));
        assert_eq!(snippet.stops, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn a_copy_of_a_placeholder_holds_copies_of_its_nested_fields() {
        // Every occurrence of 1 shows the same content, so the nested 2 appears
        // in each, and editing 2 keeps the copies of 1 identical.
        let snippet = Snippet::parse("${1:a${2:b}c} $1").unwrap();
        assert_eq!(snippet.text, "abc abc");
        assert_eq!(
            stop_ranges(&snippet),
            vec![
                vec![range(0, 0, 0, 3), range(0, 4, 0, 7)],
                vec![range(0, 1, 0, 2), range(0, 5, 0, 6)],
                vec![Range::empty(Position::new(0, 7))],
            ]
        );
        assert_eq!(snippet.fields[3].parent, Some(2));
    }

    #[test]
    fn a_choice_inserts_its_first_option_and_keeps_the_others() {
        let snippet = Snippet::parse("${1|pub,pub(crate),\\,x\\|y|} fn").unwrap();
        assert_eq!(snippet.text, "pub fn");
        assert_eq!(snippet.fields[0].range, range(0, 0, 0, 3));
        assert_eq!(snippet.fields[0].choices, ["pub", "pub(crate)", ",x|y"]);
    }

    #[test]
    fn empty_fields_at_one_position_keep_their_order() {
        let snippet = Snippet::parse("${1}${2}$0").unwrap();
        assert_eq!(snippet.text, "");
        assert_eq!(snippet.stops, vec![vec![0], vec![1], vec![2]]);
        assert!(snippet
            .fields
            .iter()
            .all(|f| f.range == Range::empty(Position::ZERO)));
    }

    #[test]
    fn the_final_stop_may_hold_text() {
        let snippet = Snippet::parse("${1:a} ${0:rest}").unwrap();
        assert_eq!(snippet.text, "a rest");
        assert_eq!(stop_ranges(&snippet)[1], vec![range(0, 2, 0, 6)]);
    }

    #[test]
    fn a_dollar_sign_that_starts_nothing_is_text() {
        let snippet = Snippet::parse("cost: $ ${1:5}\\q").unwrap();
        assert_eq!(snippet.text, "cost: $ 5\\q");
    }

    #[test]
    fn refuses_unsupported_or_malformed_syntax() {
        for text in [
            "${1:${1:x}}",
            "${1:a $1}",
            "${1:x} ${0} $0",
            "${0:${1:x}}",
            "${1/x/y/}",
            "${1:open",
            "${1|a,b}",
            "${}",
            "${-1}",
            "$9999999999999999",
            "no fields at all",
        ] {
            assert!(Snippet::parse(text).is_none(), "{text}");
        }
    }

    #[test]
    fn rejects_non_lf_line_separators_before_producing_ranges() {
        for c in ['\r', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}'] {
            assert!(Snippet::parse(&format!("${{1:a{c}b}}!")).is_none());
        }
    }

    #[test]
    fn variables_are_literal_and_tab_stops_follow_their_utf16_length() {
        let snippet =
            Snippet::parse_with_variables("$TM_SELECTED_TEXT ${1:arg} ${TM_FILENAME}$0", |name| {
                match name {
                    "TM_SELECTED_TEXT" => Some("😀\n$1".to_owned()),
                    "TM_FILENAME" => Some("日本.rs".to_owned()),
                    _ => None,
                }
            })
            .unwrap();
        assert_eq!(snippet.text, "😀\n$1 arg 日本.rs");
        assert_eq!(
            stop_ranges(&snippet),
            vec![
                vec![range(1, 3, 1, 6)],
                vec![Range::empty(Position::new(1, 12))],
            ]
        );
    }

    #[test]
    fn unset_variables_use_their_defaults_and_can_repeat() {
        let snippet = Snippet::parse_with_variables(
            "${TM_FILENAME:untitled} $TM_FILENAME ${TM_FILENAME:\\$file\\}}",
            |_| Some(String::new()),
        )
        .unwrap();
        assert_eq!(snippet.text, "untitled  $file}");
        assert_eq!(
            stop_ranges(&snippet),
            vec![vec![Range::empty(Position::new(0, 16))]]
        );
        let snippet =
            Snippet::parse_with_variables("${TM_FILENAME:unused}", |_| Some("main.rs".to_owned()))
                .unwrap();
        assert_eq!(snippet.text, "main.rs");
    }

    #[test]
    fn a_default_may_hold_fields_used_only_when_the_variable_is_unset() {
        let source = "${TM_SELECTED_TEXT:${1:body}}";
        let unset = Snippet::parse_with_variables(source, |_| Some(String::new())).unwrap();
        assert_eq!(unset.text, "body");
        assert_eq!(stop_ranges(&unset)[0], vec![range(0, 0, 0, 4)]);

        let set = Snippet::parse_with_variables(source, |_| Some("x".to_owned())).unwrap();
        assert_eq!(set.text, "x");
        assert_eq!(set.stops.len(), 1, "only the final stop: {set:?}");
    }

    #[test]
    fn unsupported_variables_and_syntax_do_not_produce_partial_expansions() {
        for source in [
            "${1:ok} $UNKNOWN",
            "${UNKNOWN:default}",
            "${TM_FILENAME/(.*)/x/}",
            "${TM_FILENAME:unclosed",
        ] {
            assert!(
                Snippet::parse_with_variables(source, |name| {
                    (name == "TM_FILENAME").then(|| "main.rs".to_owned())
                })
                .is_none(),
                "{source}"
            );
        }
        for value in ["a\r\nb", "a\u{2028}b"] {
            assert!(Snippet::parse_with_variables("$TM_SELECTED_TEXT", |_| {
                Some(value.to_owned())
            })
            .is_none());
        }
    }
}
