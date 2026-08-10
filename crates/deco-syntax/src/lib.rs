//! Syntax highlighting: turning a line of text into TextMate scopes.
//!
//! `deco-theme` already resolves a style from a scope stack, and is tested
//! doing so. What was missing was anything that produced scope stacks. This crate
//! is that: a lexer per language, emitting the scope names a VS Code theme styles.
//!
//! # A lexer, not a parser
//!
//! This is deliberate, and it is a real limitation worth stating plainly.
//!
//! VS Code's own highlighting is a set of regular-expression grammars — a lexer
//! too, not a parser. So for *colouring*, a lexer gets most of the way: keywords,
//! strings, comments, numbers, and calls are all lexical. What it cannot do is
//! anything that needs structure — telling a type from a variable by how it was
//! declared, or highlighting a language embedded in another (SQL inside a string,
//! CSS inside HTML).
//!
//! The alternative was tree-sitter, which means a generated C parser per language,
//! compiled on every target. That is a dependency per language and a C toolchain
//! in the build for a feature whose visible output a lexer already produces. When
//! the lexer's limits start to matter, a real parser is the answer — and the
//! language server's semantic tokens, which `deco-theme` can already style, are
//! the other half of that answer.
//!
//! # Scope names are specific but not language-suffixed
//!
//! Emitted scopes look like `keyword.control` and `string.quoted.double`, not
//! `keyword.control.rust`. A theme pattern matches a scope when it is a
//! whole-segment prefix of it, so `keyword` and `keyword.control` both style
//! `keyword.control`, which is what themes actually contain. A rule written for
//! `keyword.control.rust` specifically would not match — rare enough to be worth
//! the simplicity of one static string per token kind rather than one per kind per
//! language.

pub mod languages;

use std::cell::RefCell;

use deco_core::Buffer;

pub use languages::{Language, StringKind};

/// A run of one line, and the scope that applies to it.
///
/// Positions are UTF-16 code units within the line, which is what the rest of
/// deco counts in, so a span can be compared with a cursor position without
/// conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Start of the run, in UTF-16 code units from the start of the line.
    pub start: u32,
    /// End of the run, exclusive.
    pub end: u32,
    /// The TextMate scope, e.g. `keyword.control`.
    pub scope: &'static str,
}

/// What the lexer was in the middle of at the end of a line.
///
/// Highlighting one line needs to know what the line before it left open, which
/// is why this exists rather than each line being lexed independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// Nothing open.
    #[default]
    Normal,
    /// Inside a block comment, with its nesting depth.
    BlockComment(u16),
    /// Inside a string that may span lines, identified by its index in the
    /// language's string table.
    String(u16),
}

/// Highlighting for one document.
///
/// Holds the lexer state entering each line, because that is the part that cannot
/// be recomputed for a single line in isolation. The spans themselves are cheap
/// enough to recompute for the handful of lines on screen, so they are not cached
/// and cannot go stale.
#[derive(Debug, Clone)]
pub struct Syntax {
    language: Option<&'static Language>,
    /// `states[i]` is the state entering line `i`; `states[0]` is always
    /// [`State::Normal`].
    ///
    /// Behind a `RefCell` because this is a memoisation cache and nothing else.
    /// Rendering is a pure function of the session and the terminal size — that is
    /// what lets the whole layout be asserted in CI with no terminal attached — and
    /// threading `&mut` up through the render path to serve a cache would trade
    /// that away for nothing observable.
    states: RefCell<Vec<State>>,
}

impl Syntax {
    /// Highlighting for `language`, or inert if there are no rules for it.
    pub fn new(language: Option<&str>) -> Self {
        Self {
            language: language.and_then(languages::rules_for),
            states: RefCell::new(vec![State::Normal]),
        }
    }

    /// Whether this document gets highlighted at all.
    pub fn is_active(&self) -> bool {
        self.language.is_some()
    }

    /// The `source.*` scope every token sits under, for the theme's parent
    /// selectors.
    pub fn source_scope(&self) -> Option<&'static str> {
        self.language.map(|language| language.source)
    }

    /// Forgets what was known from `line` onwards.
    ///
    /// Called after an edit. Everything above the edit is still true — a change on
    /// line 900 cannot alter what line 3 left open — which is what keeps editing a
    /// large file from re-lexing all of it.
    pub fn invalidate_from(&mut self, line: usize) {
        let states = self.states.get_mut();
        // `line + 1` entries survive: the state *entering* the edited line is
        // still valid, only what it produces is not.
        states.truncate(line + 1);
        if states.is_empty() {
            states.push(State::Normal);
        }
    }

    /// The spans for `line`.
    ///
    /// Lexes forward from the last line whose state is known, so jumping to the
    /// end of a file lexes it once. That is the cost of multi-line strings and
    /// block comments existing at all: nothing can know what line 9000 is inside
    /// without having read what came before it.
    pub fn spans(&self, buffer: &Buffer, line: usize) -> Vec<Span> {
        let Some(language) = self.language else {
            return Vec::new();
        };
        let Some(text) = buffer.line_content(line) else {
            return Vec::new();
        };

        let entry = {
            let mut states = self.states.borrow_mut();
            while states.len() <= line {
                let known = states.len() - 1;
                let previous = states[known];
                let next = match buffer.line_content(known) {
                    Some(content) => lex(language, &content.to_string(), previous).1,
                    // Past the end of the document. Nothing is open on a line that
                    // does not exist.
                    None => State::Normal,
                };
                states.push(next);
            }
            states[line]
        };

        lex(language, &text.to_string(), entry).0
    }
}

/// Lexes one line, returning its spans and the state the next line starts in.
fn lex(language: &Language, text: &str, entry: State) -> (Vec<Span>, State) {
    let chars: Vec<char> = text.chars().collect();
    let mut cursor = Cursor {
        chars: &chars,
        index: 0,
        utf16: 0,
    };
    let mut spans = Vec::new();
    let mut state = entry;

    // Finish whatever the previous line left open before looking at anything else:
    // a `*/` at the start of a line is the end of a comment, not a multiplication.
    match state {
        State::BlockComment(depth) => {
            let start = cursor.utf16;
            state = continue_block_comment(language, &mut cursor, depth);
            spans.push(Span {
                start,
                end: cursor.utf16,
                scope: scopes::BLOCK_COMMENT,
            });
        }
        State::String(index) => {
            let kind = &language.strings[index as usize];
            let start = cursor.utf16;
            let closed = continue_string(kind, &mut cursor);
            spans.push(Span {
                start,
                end: cursor.utf16,
                scope: kind.scope,
            });
            state = if closed {
                State::Normal
            } else {
                State::String(index)
            };
        }
        State::Normal => {}
    }

    while let Some(c) = cursor.peek() {
        if state != State::Normal {
            break;
        }

        if c.is_whitespace() {
            cursor.advance();
            continue;
        }

        // Comments before strings and before everything else: `//"` is a comment
        // containing a quote, not a comment and then an unterminated string.
        if let Some(opener) = language
            .line_comments
            .iter()
            .find(|opener| cursor.starts_with(opener))
        {
            let start = cursor.utf16;
            cursor.skip(opener.chars().count());
            while cursor.peek().is_some() {
                cursor.advance();
            }
            spans.push(Span {
                start,
                end: cursor.utf16,
                scope: scopes::LINE_COMMENT,
            });
            continue;
        }

        if let Some(block) = &language.block_comment {
            if cursor.starts_with(block.open) {
                let start = cursor.utf16;
                cursor.skip(block.open.chars().count());
                state = continue_block_comment(language, &mut cursor, 1);
                spans.push(Span {
                    start,
                    end: cursor.utf16,
                    scope: scopes::BLOCK_COMMENT,
                });
                continue;
            }
        }

        if let Some((index, kind)) = language
            .strings
            .iter()
            .enumerate()
            .find(|(_, kind)| cursor.starts_with(kind.open))
        {
            let start = cursor.utf16;
            cursor.skip(kind.open.chars().count());
            let closed = continue_string(kind, &mut cursor);
            spans.push(Span {
                start,
                end: cursor.utf16,
                scope: kind.scope,
            });
            if !closed && kind.multiline {
                state = State::String(index as u16);
            }
            continue;
        }

        if c.is_ascii_digit() {
            let start = cursor.utf16;
            // Deliberately loose: `0xFF`, `1_000`, `1.5e3` and `1.2.3` all lex as
            // one number. A colouring pass has no reason to reject a malformed
            // literal — the compiler will, with a better message.
            while cursor
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
            {
                cursor.advance();
            }
            spans.push(Span {
                start,
                end: cursor.utf16,
                scope: scopes::NUMBER,
            });
            continue;
        }

        if is_word_start(c) {
            let start = cursor.utf16;
            let mut word = String::new();
            while let Some(c) = cursor.peek() {
                if !is_word_continue(c) {
                    break;
                }
                word.push(c);
                cursor.advance();
            }
            let end = cursor.utf16;
            if let Some(scope) = classify(language, &word, &cursor) {
                spans.push(Span { start, end, scope });
            }
            continue;
        }

        cursor.advance();
    }

    (spans, state)
}

/// Consumes a block comment from the cursor, returning the state afterwards.
fn continue_block_comment(language: &Language, cursor: &mut Cursor<'_>, mut depth: u16) -> State {
    let Some(block) = &language.block_comment else {
        return State::Normal;
    };
    while cursor.peek().is_some() {
        if cursor.starts_with(block.close) {
            cursor.skip(block.close.chars().count());
            depth -= 1;
            if depth == 0 {
                return State::Normal;
            }
            continue;
        }
        // Rust's block comments nest, so `/* /* */ */` is one comment and the
        // first `*/` does not end it.
        if block.nests && cursor.starts_with(block.open) {
            cursor.skip(block.open.chars().count());
            depth = depth.saturating_add(1);
            continue;
        }
        cursor.advance();
    }
    State::BlockComment(depth)
}

/// Consumes a string body, returning whether it was closed on this line.
fn continue_string(kind: &StringKind, cursor: &mut Cursor<'_>) -> bool {
    while let Some(c) = cursor.peek() {
        if kind.escapes && c == '\\' {
            cursor.advance();
            // A trailing backslash escapes the line break, so the string
            // continues — and there is nothing left on this line to consume.
            if cursor.peek().is_some() {
                cursor.advance();
            }
            continue;
        }
        if cursor.starts_with(kind.close) {
            cursor.skip(kind.close.chars().count());
            return true;
        }
        cursor.advance();
    }
    false
}

/// What scope a word gets, if any.
fn classify(language: &Language, word: &str, cursor: &Cursor<'_>) -> Option<&'static str> {
    if language.keywords.contains(&word) {
        return Some(scopes::KEYWORD);
    }
    if language.constants.contains(&word) {
        return Some(scopes::CONSTANT);
    }
    if language.types.contains(&word) {
        return Some(scopes::TYPE);
    }
    // An identifier immediately followed by `(` is a call. Lexical, cheap, and
    // worth a great deal of what highlighting is for; it is also why a keyword is
    // checked first, so `if (x)` does not colour `if` as a function.
    if cursor.next_visible() == Some('(') {
        return Some(scopes::FUNCTION);
    }
    // A word starting with a capital is a type in every language here that has a
    // convention at all. Wrong for a constant like `MAX`, which is why it comes
    // last: an explicit type or keyword wins.
    if language.capitals_are_types && word.starts_with(char::is_uppercase) {
        return Some(scopes::TYPE);
    }
    None
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A position in a line, counted both in characters and in UTF-16 units.
///
/// Two counters because the scan works in characters and the rest of deco
/// addresses text in UTF-16 code units; keeping them side by side is cheaper and
/// less error-prone than converting afterwards.
struct Cursor<'a> {
    chars: &'a [char],
    index: usize,
    utf16: u32,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    fn advance(&mut self) {
        if let Some(c) = self.peek() {
            self.utf16 += c.len_utf16() as u32;
            self.index += 1;
        }
    }

    fn skip(&mut self, count: usize) {
        for _ in 0..count {
            self.advance();
        }
    }

    fn starts_with(&self, text: &str) -> bool {
        let mut index = self.index;
        for wanted in text.chars() {
            match self.chars.get(index) {
                Some(c) if *c == wanted => index += 1,
                _ => return false,
            }
        }
        true
    }

    /// The next character that is not whitespace, without moving.
    fn next_visible(&self) -> Option<char> {
        self.chars[self.index..]
            .iter()
            .find(|c| !c.is_whitespace())
            .copied()
    }
}

/// The scope names emitted, in one place so they can be read at a glance.
pub mod scopes {
    /// Keywords: `if`, `fn`, `return`.
    pub const KEYWORD: &str = "keyword.control";
    /// Named types: `String`, `int`.
    pub const TYPE: &str = "entity.name.type";
    /// Language constants: `true`, `null`, `nil`.
    pub const CONSTANT: &str = "constant.language";
    /// Numeric literals.
    pub const NUMBER: &str = "constant.numeric";
    /// An identifier being called.
    pub const FUNCTION: &str = "entity.name.function";
    /// A line comment.
    pub const LINE_COMMENT: &str = "comment.line.double-slash";
    /// A block comment.
    pub const BLOCK_COMMENT: &str = "comment.block";
    /// A double-quoted string.
    pub const DOUBLE_STRING: &str = "string.quoted.double";
    /// A single-quoted string.
    pub const SINGLE_STRING: &str = "string.quoted.single";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lexes `text` as `language` and returns `(scope, matched text)` pairs.
    fn spans(language: &str, text: &str) -> Vec<(&'static str, String)> {
        let buffer = Buffer::from_text(text);
        let syntax = Syntax::new(Some(language));
        let mut out = Vec::new();
        for line in 0..buffer.line_count() {
            let content: Vec<char> = buffer
                .line_content(line)
                .map(|s| s.to_string())
                .unwrap_or_default()
                .chars()
                .collect();
            for span in syntax.spans(&buffer, line) {
                // Back from UTF-16 units to characters, for readable assertions.
                let mut text = String::new();
                let mut units = 0u32;
                for c in &content {
                    if units >= span.start && units < span.end {
                        text.push(*c);
                    }
                    units += c.len_utf16() as u32;
                }
                out.push((span.scope, text));
            }
        }
        out
    }

    #[test]
    fn a_language_with_no_rules_is_inert() {
        let syntax = Syntax::new(Some("brainfuck"));
        assert!(!syntax.is_active());
        assert!(spans("brainfuck", "+++[->+<]").is_empty());
    }

    #[test]
    fn no_language_at_all_is_inert() {
        assert!(!Syntax::new(None).is_active());
    }

    #[test]
    fn keywords_strings_and_numbers_are_found() {
        let found = spans("rust", r#"let x = "hi" + 42;"#);
        assert_eq!(
            found,
            vec![
                (scopes::KEYWORD, "let".to_owned()),
                (scopes::DOUBLE_STRING, "\"hi\"".to_owned()),
                (scopes::NUMBER, "42".to_owned()),
            ]
        );
    }

    #[test]
    fn a_line_comment_swallows_the_rest_of_the_line() {
        let found = spans("rust", "let x = 1; // let y = 2;");
        assert_eq!(found.len(), 3);
        assert_eq!(found[2], (scopes::LINE_COMMENT, "// let y = 2;".to_owned()));
    }

    #[test]
    fn a_comment_marker_inside_a_string_is_not_a_comment() {
        let found = spans("rust", r#"let s = "// not a comment";"#);
        assert_eq!(
            found,
            vec![
                (scopes::KEYWORD, "let".to_owned()),
                (scopes::DOUBLE_STRING, r#""// not a comment""#.to_owned()),
            ]
        );
    }

    #[test]
    fn a_quote_inside_a_comment_is_not_a_string() {
        let found = spans("rust", r#"// it's fine"#);
        assert_eq!(
            found,
            vec![(scopes::LINE_COMMENT, "// it's fine".to_owned())]
        );
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let found = spans("rust", r#"let s = "a\"b";"#);
        assert_eq!(found[1], (scopes::DOUBLE_STRING, r#""a\"b""#.to_owned()));
    }

    #[test]
    fn a_block_comment_spans_lines() {
        let found = spans("rust", "a /* one\ntwo\nthree */ b");
        let comments: Vec<&String> = found
            .iter()
            .filter(|(scope, _)| *scope == scopes::BLOCK_COMMENT)
            .map(|(_, text)| text)
            .collect();
        assert_eq!(comments, vec!["/* one", "two", "three */"]);
    }

    #[test]
    fn rust_block_comments_nest() {
        // The first `*/` closes the inner comment, not the outer one, so `b` is
        // still inside it and `let` on the last line is not a keyword.
        let found = spans("rust", "/* a /* b */ still a */ let x");
        assert_eq!(found.last(), Some(&(scopes::KEYWORD, "let".to_owned())));
        let comment = &found[0];
        assert_eq!(comment.0, scopes::BLOCK_COMMENT);
        assert_eq!(comment.1, "/* a /* b */ still a */");
    }

    #[test]
    fn c_block_comments_do_not_nest() {
        let found = spans("c", "/* a /* b */ int x");
        assert_eq!(found[0], (scopes::BLOCK_COMMENT, "/* a /* b */".to_owned()));
        assert!(found
            .iter()
            .any(|(scope, text)| *scope == scopes::TYPE && text == "int"));
    }

    #[test]
    fn an_unterminated_block_comment_runs_to_the_end_of_the_file() {
        let found = spans("rust", "/* a\nb\nc");
        assert_eq!(found.len(), 3);
        assert!(found
            .iter()
            .all(|(scope, _)| *scope == scopes::BLOCK_COMMENT));
    }

    #[test]
    fn a_call_is_highlighted_as_a_function() {
        let found = spans("rust", "compute(x)");
        assert_eq!(found, vec![(scopes::FUNCTION, "compute".to_owned())]);
    }

    #[test]
    fn a_keyword_before_a_bracket_is_still_a_keyword() {
        let found = spans("c", "if (x) return;");
        assert_eq!(found[0], (scopes::KEYWORD, "if".to_owned()));
        assert_eq!(found[1], (scopes::KEYWORD, "return".to_owned()));
    }

    #[test]
    fn a_call_with_a_space_before_the_bracket_is_still_a_call() {
        let found = spans("rust", "compute (x)");
        assert_eq!(found[0].0, scopes::FUNCTION);
    }

    #[test]
    fn python_triple_quoted_strings_span_lines() {
        let found = spans("python", "s = \"\"\"one\ntwo\"\"\"\nx = 1");
        let strings: Vec<&String> = found
            .iter()
            .filter(|(scope, _)| *scope == scopes::DOUBLE_STRING)
            .map(|(_, text)| text)
            .collect();
        assert_eq!(strings, vec!["\"\"\"one", "two\"\"\""]);
        // And the line after it is back to normal.
        assert_eq!(found.last(), Some(&(scopes::NUMBER, "1".to_owned())));
    }

    #[test]
    fn a_single_line_string_does_not_leak_past_its_line() {
        // Rust's `"` strings can span lines, but an unterminated one on the last
        // line must not swallow a following line that does not exist.
        let found = spans("json", "{\"a\": \"unterminated\n\"b\": 1}");
        assert!(
            found.iter().any(|(scope, _)| *scope == scopes::NUMBER),
            "the second line should still be lexed: {found:?}"
        );
    }

    #[test]
    fn positions_are_utf16_units_so_they_line_up_with_the_cursor() {
        let buffer = Buffer::from_text("let s = \"😀\";");
        let syntax = Syntax::new(Some("rust"));
        let spans = syntax.spans(&buffer, 0);
        let string = spans
            .iter()
            .find(|span| span.scope == scopes::DOUBLE_STRING)
            .expect("the string should be found");
        // `let s = ` is eight units, and the emoji is two.
        assert_eq!(string.start, 8);
        assert_eq!(string.end, 12);
    }

    #[test]
    fn invalidating_forgets_only_what_came_after() {
        let buffer = Buffer::from_text("/* a\nb */\nlet x = 1;");
        let mut syntax = Syntax::new(Some("rust"));
        // Lex to the end, so every state is known.
        for line in 0..3 {
            syntax.spans(&buffer, line);
        }
        assert_eq!(syntax.states.borrow().len(), 3);
        syntax.invalidate_from(1);
        assert_eq!(syntax.states.borrow().len(), 2, "line 0's state survives");
        assert_eq!(syntax.states.borrow()[0], State::Normal);
    }

    #[test]
    fn invalidating_from_the_first_line_leaves_a_usable_state() {
        let mut syntax = Syntax::new(Some("rust"));
        syntax.invalidate_from(0);
        assert_eq!(*syntax.states.borrow(), vec![State::Normal]);
    }

    #[test]
    fn highlighting_after_an_edit_reflects_the_new_text() {
        // The property that matters: invalidation must not leave a stale state
        // that colours the rest of the file as a comment.
        let mut syntax = Syntax::new(Some("rust"));
        let opened = Buffer::from_text("/* a\nlet x = 1;");
        for line in 0..2 {
            syntax.spans(&opened, line);
        }
        let closed = Buffer::from_text("// a\nlet x = 1;");
        syntax.invalidate_from(0);
        let second = syntax.spans(&closed, 1);
        assert!(
            second.iter().any(|span| span.scope == scopes::KEYWORD),
            "line 1 should be code again: {second:?}"
        );
    }

    #[test]
    fn a_line_past_the_end_of_the_document_has_no_spans() {
        let buffer = Buffer::from_text("let x = 1;\n");
        let syntax = Syntax::new(Some("rust"));
        assert!(syntax.spans(&buffer, 99).is_empty());
    }

    #[test]
    fn spans_never_overlap_and_are_in_order() {
        // The renderer paints them in order and assumes they do not overlap; a
        // lexer bug that produced either would corrupt the line.
        let sources = [
            ("rust", "let a = \"s\"; /* c */ f(1) // end"),
            ("python", "def f(x): return {'a': 1}  # c"),
            ("json", "{\"a\": [1, 2, true, null]}"),
            ("shellscript", "echo \"$x\" # comment"),
            ("yaml", "key: value # comment"),
            ("toml", "[table]\nkey = \"v\" # c"),
            ("typescript", "const x: number = 1 as const;"),
            ("go", "func f() error { return nil }"),
            ("css", "a.b { color: #fff; /* c */ }"),
        ];
        for (language, text) in sources {
            let buffer = Buffer::from_text(text);
            let syntax = Syntax::new(Some(language));
            for line in 0..buffer.line_count() {
                let spans = syntax.spans(&buffer, line);
                for pair in spans.windows(2) {
                    assert!(
                        pair[0].end <= pair[1].start,
                        "{language}: {:?} overlaps {:?}",
                        pair[0],
                        pair[1]
                    );
                }
                for span in &spans {
                    assert!(span.start < span.end, "{language}: empty span {span:?}");
                }
            }
        }
    }
}
