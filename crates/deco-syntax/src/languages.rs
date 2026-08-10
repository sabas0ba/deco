//! The per-language rules, as data.
//!
//! Adding a language is a table here and nothing else. The identifiers are the
//! ones `deco_editor::document::language_for_path` produces, which are VS Code's
//! language ids — so a file deco calls `typescriptreact` finds the rules under
//! that name rather than under a second naming scheme invented here.
//!
//! Languages that are not token-oriented are deliberately absent. Markdown, HTML
//! and XML are structural: colouring them by keyword would be worse than leaving
//! them plain, because the interesting parts are tags and nesting, and a lexer
//! that pretended otherwise would highlight the wrong halves of the file. They
//! fall through to no highlighting, which is honest.

use crate::scopes;

/// How to lex one language.
#[derive(Debug)]
pub struct Language {
    /// The `source.*` scope every token sits under.
    pub source: &'static str,
    /// Words coloured as keywords.
    pub keywords: &'static [&'static str],
    /// Words coloured as types.
    pub types: &'static [&'static str],
    /// Words coloured as language constants.
    pub constants: &'static [&'static str],
    /// Line-comment openers.
    pub line_comments: &'static [&'static str],
    /// The block-comment delimiters, if the language has them.
    pub block_comment: Option<Block>,
    /// String delimiters, tried in order — so `"""` must come before `"`.
    pub strings: &'static [StringKind],
    /// Whether a capitalised word with no other classification is a type.
    ///
    /// True where the language has a naming convention strong enough to rely on,
    /// false for languages like Python where module-level constants are also
    /// capitalised and shouting `MAX_SIZE` as a type would be wrong more often
    /// than right.
    pub capitals_are_types: bool,
}

/// Block-comment delimiters.
#[derive(Debug)]
pub struct Block {
    /// What opens it.
    pub open: &'static str,
    /// What closes it.
    pub close: &'static str,
    /// Whether an opener inside one nests rather than being ignored, as in Rust.
    pub nests: bool,
}

/// One kind of string literal.
#[derive(Debug)]
pub struct StringKind {
    /// What opens it.
    pub open: &'static str,
    /// What closes it.
    pub close: &'static str,
    /// Whether a backslash escapes the next character.
    pub escapes: bool,
    /// Whether it may continue past the end of a line.
    pub multiline: bool,
    /// The scope to emit.
    pub scope: &'static str,
}

/// The rules for `language`, or `None` if there are none.
pub fn rules_for(language: &str) -> Option<&'static Language> {
    Some(match language {
        "rust" => &RUST,
        "javascript" | "javascriptreact" | "typescript" | "typescriptreact" => &TYPESCRIPT,
        "python" => &PYTHON,
        "go" => &GO,
        "c" | "cpp" => &C,
        "java" => &JAVA,
        "json" | "jsonc" => &JSON,
        "toml" => &TOML,
        "yaml" => &YAML,
        "shellscript" | "makefile" | "dockerfile" => &SHELL,
        "ruby" => &RUBY,
        "lua" => &LUA,
        "sql" => &SQL,
        "css" => &CSS,
        _ => return None,
    })
}

/// Every language with rules, for tests and for reporting what is covered.
pub const SUPPORTED: &[&str] = &[
    "rust",
    "javascript",
    "javascriptreact",
    "typescript",
    "typescriptreact",
    "python",
    "go",
    "c",
    "cpp",
    "java",
    "json",
    "jsonc",
    "toml",
    "yaml",
    "shellscript",
    "makefile",
    "dockerfile",
    "ruby",
    "lua",
    "sql",
    "css",
];

/// `"…"` with backslash escapes, which most languages have.
const DOUBLE: StringKind = StringKind {
    open: "\"",
    close: "\"",
    escapes: true,
    multiline: false,
    scope: scopes::DOUBLE_STRING,
};

/// `'…'` with backslash escapes.
const SINGLE: StringKind = StringKind {
    open: "'",
    close: "'",
    escapes: true,
    multiline: false,
    scope: scopes::SINGLE_STRING,
};

const C_BLOCK: Block = Block {
    open: "/*",
    close: "*/",
    nests: false,
};

static RUST: Language = Language {
    source: "source.rust",
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "static", "struct", "super", "trait", "type", "unsafe", "use",
        "where", "while", "yield",
    ],
    types: &[
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8",
        "u16", "u32", "u64", "u128", "usize", "String", "Vec", "Option", "Result", "Box", "Self",
    ],
    constants: &["true", "false", "None", "Some", "Ok", "Err", "self"],
    line_comments: &["//"],
    // Rust's block comments nest, which is why `nests` exists at all.
    block_comment: Some(Block {
        open: "/*",
        close: "*/",
        nests: true,
    }),
    // `'` is a lifetime far more often than a character literal, and a lifetime
    // lexed as an unterminated string would colour the rest of the line. Only
    // double quotes, deliberately.
    strings: &[DOUBLE],
    capitals_are_types: true,
};

static TYPESCRIPT: Language = Language {
    source: "source.ts",
    keywords: &[
        "abstract",
        "as",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "declare",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "finally",
        "for",
        "from",
        "function",
        "get",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "keyof",
        "let",
        "new",
        "of",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "satisfies",
        "set",
        "static",
        "switch",
        "throw",
        "try",
        "type",
        "typeof",
        "var",
        "void",
        "while",
        "yield",
    ],
    types: &[
        "any", "bigint", "boolean", "never", "number", "object", "string", "symbol", "unknown",
        "Array", "Promise", "Record", "Map", "Set",
    ],
    constants: &[
        "true",
        "false",
        "null",
        "undefined",
        "this",
        "NaN",
        "Infinity",
    ],
    line_comments: &["//"],
    block_comment: Some(C_BLOCK),
    // Template literals span lines; the other two do not.
    strings: &[
        StringKind {
            open: "`",
            close: "`",
            escapes: true,
            multiline: true,
            scope: scopes::DOUBLE_STRING,
        },
        DOUBLE,
        SINGLE,
    ],
    capitals_are_types: true,
};

static PYTHON: Language = Language {
    source: "source.python",
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
        "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with",
        "yield", "match", "case",
    ],
    types: &[
        "bool",
        "bytes",
        "dict",
        "float",
        "frozenset",
        "int",
        "list",
        "set",
        "str",
        "tuple",
        "type",
        "object",
    ],
    constants: &[
        "True",
        "False",
        "None",
        "self",
        "cls",
        "NotImplemented",
        "Ellipsis",
    ],
    line_comments: &["#"],
    block_comment: None,
    // Triple quotes first, or `"""a"""` lexes as an empty string followed by `a`.
    strings: &[
        StringKind {
            open: "\"\"\"",
            close: "\"\"\"",
            escapes: true,
            multiline: true,
            scope: scopes::DOUBLE_STRING,
        },
        StringKind {
            open: "'''",
            close: "'''",
            escapes: true,
            multiline: true,
            scope: scopes::SINGLE_STRING,
        },
        DOUBLE,
        SINGLE,
    ],
    // `MAX_SIZE` is a constant, not a type, and Python has enough of those that
    // guessing from the capital would be wrong more often than right.
    capitals_are_types: false,
};

static GO: Language = Language {
    source: "source.go",
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
    ],
    types: &[
        "bool",
        "byte",
        "complex64",
        "complex128",
        "error",
        "float32",
        "float64",
        "int",
        "int8",
        "int16",
        "int32",
        "int64",
        "rune",
        "string",
        "uint",
        "uint8",
        "uint16",
        "uint32",
        "uint64",
        "uintptr",
        "any",
    ],
    constants: &["true", "false", "nil", "iota"],
    line_comments: &["//"],
    block_comment: Some(C_BLOCK),
    strings: &[
        StringKind {
            open: "`",
            close: "`",
            escapes: false,
            multiline: true,
            scope: scopes::DOUBLE_STRING,
        },
        DOUBLE,
    ],
    capitals_are_types: true,
};

static C: Language = Language {
    source: "source.c",
    keywords: &[
        "alignas",
        "alignof",
        "auto",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "constexpr",
        "continue",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "explicit",
        "extern",
        "for",
        "friend",
        "goto",
        "if",
        "inline",
        "namespace",
        "new",
        "noexcept",
        "operator",
        "private",
        "protected",
        "public",
        "register",
        "return",
        "sizeof",
        "static",
        "struct",
        "switch",
        "template",
        "this",
        "throw",
        "try",
        "typedef",
        "typename",
        "union",
        "using",
        "virtual",
        "volatile",
        "while",
    ],
    types: &[
        "bool", "char", "double", "float", "int", "long", "short", "signed", "size_t", "unsigned",
        "void", "wchar_t", "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t",
        "uint32_t", "uint64_t",
    ],
    constants: &["true", "false", "NULL", "nullptr"],
    line_comments: &["//"],
    block_comment: Some(C_BLOCK),
    strings: &[DOUBLE, SINGLE],
    capitals_are_types: false,
};

static JAVA: Language = Language {
    source: "source.java",
    keywords: &[
        "abstract",
        "assert",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "instanceof",
        "interface",
        "native",
        "new",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "try",
        "volatile",
        "while",
        "var",
        "record",
        "sealed",
        "yield",
    ],
    types: &[
        "boolean", "byte", "char", "double", "float", "int", "long", "short", "void", "String",
        "Object", "List", "Map", "Set",
    ],
    constants: &["true", "false", "null"],
    line_comments: &["//"],
    block_comment: Some(C_BLOCK),
    strings: &[DOUBLE, SINGLE],
    capitals_are_types: true,
};

static JSON: Language = Language {
    source: "source.json",
    // JSON has no keywords; `true`/`false`/`null` are constants.
    keywords: &[],
    types: &[],
    constants: &["true", "false", "null"],
    // Plain JSON has no comments, but JSONC does and the two share a table.
    // Colouring a `//` in strict JSON as a comment is a smaller error than
    // failing to colour one in a `.jsonc` file, since strict JSON containing
    // `//` outside a string is already invalid.
    line_comments: &["//"],
    block_comment: Some(C_BLOCK),
    strings: &[DOUBLE],
    capitals_are_types: false,
};

static TOML: Language = Language {
    source: "source.toml",
    keywords: &[],
    types: &[],
    constants: &["true", "false"],
    line_comments: &["#"],
    block_comment: None,
    strings: &[
        StringKind {
            open: "\"\"\"",
            close: "\"\"\"",
            escapes: true,
            multiline: true,
            scope: scopes::DOUBLE_STRING,
        },
        StringKind {
            open: "'''",
            close: "'''",
            escapes: false,
            multiline: true,
            scope: scopes::SINGLE_STRING,
        },
        DOUBLE,
        StringKind {
            open: "'",
            close: "'",
            // TOML's literal strings take no escapes at all.
            escapes: false,
            multiline: false,
            scope: scopes::SINGLE_STRING,
        },
    ],
    capitals_are_types: false,
};

static YAML: Language = Language {
    source: "source.yaml",
    keywords: &[],
    types: &[],
    constants: &["true", "false", "null", "yes", "no", "on", "off", "~"],
    line_comments: &["#"],
    block_comment: None,
    strings: &[DOUBLE, SINGLE],
    capitals_are_types: false,
};

static SHELL: Language = Language {
    source: "source.shell",
    keywords: &[
        "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if",
        "in", "local", "readonly", "return", "select", "then", "until", "while", "set", "unset",
        "shift", "source", "trap",
    ],
    types: &[],
    constants: &["true", "false"],
    line_comments: &["#"],
    block_comment: None,
    // A single-quoted shell string takes no escapes: `'a\'` is a backslash.
    strings: &[
        DOUBLE,
        StringKind {
            open: "'",
            close: "'",
            escapes: false,
            multiline: false,
            scope: scopes::SINGLE_STRING,
        },
    ],
    capitals_are_types: false,
};

static RUBY: Language = Language {
    source: "source.ruby",
    keywords: &[
        "alias",
        "and",
        "begin",
        "break",
        "case",
        "class",
        "def",
        "defined?",
        "do",
        "else",
        "elsif",
        "end",
        "ensure",
        "for",
        "if",
        "in",
        "module",
        "next",
        "not",
        "or",
        "redo",
        "rescue",
        "retry",
        "return",
        "super",
        "then",
        "undef",
        "unless",
        "until",
        "when",
        "while",
        "yield",
        "require",
        "require_relative",
        "attr_accessor",
        "attr_reader",
    ],
    types: &[],
    constants: &["true", "false", "nil", "self", "__FILE__", "__LINE__"],
    line_comments: &["#"],
    block_comment: None,
    strings: &[DOUBLE, SINGLE],
    capitals_are_types: true,
};

static LUA: Language = Language {
    source: "source.lua",
    keywords: &[
        "and", "break", "do", "else", "elseif", "end", "for", "function", "goto", "if", "in",
        "local", "not", "or", "repeat", "return", "then", "until", "while",
    ],
    types: &[],
    constants: &["true", "false", "nil", "self"],
    line_comments: &["--"],
    block_comment: Some(Block {
        open: "--[[",
        close: "]]",
        nests: false,
    }),
    strings: &[DOUBLE, SINGLE],
    capitals_are_types: false,
};

static SQL: Language = Language {
    source: "source.sql",
    keywords: &[
        "ALTER", "AND", "AS", "ASC", "BEGIN", "BY", "CASE", "COMMIT", "CREATE", "DELETE", "DESC",
        "DISTINCT", "DROP", "ELSE", "END", "EXISTS", "FROM", "GROUP", "HAVING", "IN", "INDEX",
        "INNER", "INSERT", "INTO", "JOIN", "LEFT", "LIKE", "LIMIT", "NOT", "ON", "OR", "ORDER",
        "OUTER", "RIGHT", "ROLLBACK", "SELECT", "SET", "TABLE", "THEN", "UNION", "UPDATE",
        "VALUES", "VIEW", "WHEN", "WHERE", "WITH",
        // Lower case too: SQL is case-insensitive and both spellings are common,
        // and a lexer comparing words exactly needs each one it should match.
        "alter", "and", "as", "asc", "begin", "by", "case", "commit", "create", "delete", "desc",
        "distinct", "drop", "else", "end", "exists", "from", "group", "having", "in", "index",
        "inner", "insert", "into", "join", "left", "like", "limit", "not", "on", "or", "order",
        "outer", "right", "rollback", "select", "set", "table", "then", "union", "update",
        "values", "view", "when", "where", "with",
    ],
    types: &[
        "BIGINT",
        "BOOLEAN",
        "CHAR",
        "DATE",
        "DECIMAL",
        "DOUBLE",
        "FLOAT",
        "INT",
        "INTEGER",
        "NUMERIC",
        "REAL",
        "SMALLINT",
        "TEXT",
        "TIMESTAMP",
        "VARCHAR",
        "bigint",
        "boolean",
        "char",
        "date",
        "decimal",
        "double",
        "float",
        "int",
        "integer",
        "numeric",
        "real",
        "smallint",
        "text",
        "timestamp",
        "varchar",
    ],
    constants: &["NULL", "TRUE", "FALSE", "null", "true", "false"],
    line_comments: &["--"],
    block_comment: Some(C_BLOCK),
    strings: &[
        StringKind {
            open: "'",
            close: "'",
            escapes: false,
            multiline: false,
            scope: scopes::SINGLE_STRING,
        },
        DOUBLE,
    ],
    capitals_are_types: false,
};

static CSS: Language = Language {
    source: "source.css",
    keywords: &[
        "important",
        "media",
        "import",
        "charset",
        "supports",
        "keyframes",
        "font-face",
        "from",
        "to",
        "and",
        "not",
        "only",
    ],
    types: &[],
    constants: &[
        "inherit",
        "initial",
        "unset",
        "none",
        "auto",
        "transparent",
        "currentColor",
    ],
    // CSS has no line comments; `//` in a stylesheet is not one.
    line_comments: &[],
    block_comment: Some(C_BLOCK),
    strings: &[DOUBLE, SINGLE],
    capitals_are_types: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_language_has_rules() {
        for language in SUPPORTED {
            assert!(rules_for(language).is_some(), "{language} has no rules");
        }
    }

    #[test]
    fn an_unknown_language_has_none() {
        assert!(rules_for("cobol").is_none());
        assert!(rules_for("").is_none());
    }

    #[test]
    fn string_delimiters_are_ordered_longest_first() {
        // `"""` has to be tried before `"`, or a triple-quoted string lexes as an
        // empty one followed by code. The order in the table is the order tried.
        for language in SUPPORTED {
            let rules = rules_for(language).unwrap();
            for (index, kind) in rules.strings.iter().enumerate() {
                for earlier in &rules.strings[..index] {
                    assert!(
                        !kind.open.starts_with(earlier.open),
                        "{language}: `{}` is tried after `{}`, which is a prefix of it",
                        kind.open,
                        earlier.open
                    );
                }
            }
        }
    }

    #[test]
    fn no_language_lists_a_word_in_two_categories() {
        // A word in both `keywords` and `types` would be classified by whichever
        // check ran first, which is a coin toss dressed up as a rule.
        for language in SUPPORTED {
            let rules = rules_for(language).unwrap();
            for word in rules.keywords {
                assert!(
                    !rules.types.contains(word) && !rules.constants.contains(word),
                    "{language}: `{word}` is a keyword and something else"
                );
            }
            for word in rules.types {
                assert!(
                    !rules.constants.contains(word),
                    "{language}: `{word}` is a type and a constant"
                );
            }
        }
    }

    #[test]
    fn every_source_scope_is_a_source_scope() {
        for language in SUPPORTED {
            let source = rules_for(language).unwrap().source;
            assert!(source.starts_with("source."), "{language}: {source}");
        }
    }

    #[test]
    fn the_languages_deco_detects_are_either_covered_or_deliberately_not() {
        // Everything `language_for_path` can return, and why it is or is not here.
        // A new language added to the detector should be a deliberate decision
        // about highlighting rather than a silent omission.
        let structural = ["markdown", "html", "xml"];
        for language in structural {
            assert!(
                rules_for(language).is_none(),
                "{language} is structural; see the module docs"
            );
        }
    }
}
