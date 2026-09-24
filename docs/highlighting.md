# Syntax highlighting

Colours come from the theme, through the same scope-matching rules that VS Code themes are written for, so a theme you already use has the same meaning in deco.

![The same theme colouring Rust, TypeScript, Python, TOML and JSON](img/highlighting.svg)

## How it works

`deco-syntax` uses a lexer for each supported language to assign TextMate scope names to tokens. `deco-theme` resolves a style from each **TextMate scope stack**, and the renderer applies that style to the corresponding text.

The scope stack has two levels: the language's `source.*` scope, then the token's scope. A theme's parent selectors (`meta.function entity.name`) therefore have a parent scope to match.

| Scope emitted | What it is |
| --- | --- |
| `keyword.control` | `if`, `fn`, `return` |
| `entity.name.type` | `String`, `int`, a capitalised word where the language has that convention |
| `entity.name.function` | An identifier immediately followed by `(` |
| `constant.language` | `true`, `null`, `nil`, `None` |
| `constant.numeric` | Numeric literals |
| `string.quoted.double`, `string.quoted.single` | String literals |
| `comment.line.double-slash`, `comment.block` | Comments |

Scopes are specific but **not language-suffixed**: `keyword.control`, not `keyword.control.rust`. A theme pattern matches a scope when it is a whole-segment prefix of it, so both `keyword` and `keyword.control` apply to the scopes above, and these are the patterns themes typically contain. A rule written specifically for `keyword.control.rust` does not match. This is the trade-off for using one static string per token kind instead of one per kind per language.

## Languages

Rust, TypeScript, JavaScript (and the `react` variants), Python, Go, C, C++, Java,
JSON, JSONC, TOML, YAML, shell, Ruby, Lua, SQL, CSS, Makefile and Dockerfile.

Other languages render in the theme's plain foreground. Markdown, HTML and XML have no lexer: the current keyword-based language tables cannot represent their markup structure and embedded languages.

Adding a language requires only a table in `crates/deco-syntax/src/languages.rs`.

## Choosing the language yourself

The language is determined from the file name: its extension, or the whole name for `Makefile`, `Dockerfile` and `Cargo.toml`. When that is wrong or the name gives no indication, `ctrl+k m` selects a language.

![Telling a .txt file that it is TOML](img/language-mode.svg)

The right-hand column shows the **identifier**, not a second name for the language. The identifier is what `[toml]` in a `settings.json` refers to, what language servers are matched on, and what selects the lexer. The title is for finding the row; the identifier determines the behaviour.

Choosing a language updates everything that depends on it: the lexer, the settings (so a `[toml]` block's `editor.tabSize` starts to apply), and the `editorLangId` context key, so `when` clauses evaluate against the new language. The terminal frontend also re-attaches its language server, because a different language uses a different server.

**Auto Detect** is the first row and restores automatic detection. Its right-hand column shows the language that detection would choose.

The text is never changed. A document's bytes do not depend on its language, only how they are interpreted, so changing the language is not an edit and is not undoable.

| Key | Command |
| --- | --- |
| `ctrl+k m` | `workbench.action.editor.changeLanguageMode` |

The picker lists every identifier deco knows, including those without a lexer (`markdown`, `html`, `xml`, `plaintext`). They still select settings and a language server, which are the main uses of a language identifier.

## It is a lexer, not a parser

The lexer recognises tokens but does not resolve declarations or types.

VS Code's own highlighting uses a set of regular-expression grammars, which is also a lexer. For colouring, a lexer covers most needs: keywords, strings, comments, numbers and calls are all lexical properties. Multi-line constructs also work: block comments and triple-quoted strings carry state from one line to the next, and Rust's nested `/* /* */ */` comments nest correctly.

What a lexer cannot do:

- **Tell a type from a variable by how it was declared.** `Foo` is coloured as a type because it is capitalised, in languages where that convention applies. In Python, where `MAX_SIZE` is a constant rather than a type, deco does not apply this heuristic.
- **Highlight a language inside another** — SQL in a string, CSS in HTML.
- **Distinguish a shadowed name, a macro from a function, a field from a method.**

A language server's **semantic tokens** provide the information a lexer lacks. When a server provides them, they are drawn, and the lexer continues to colour everything else; see [Semantic tokens](language-servers.md#semantic-tokens).

## Why not tree-sitter

Tree-sitter was the obvious candidate and was rejected because of its build cost. A tree-sitter grammar is a generated C parser compiled for every target. It adds a dependency per language and requires a C toolchain in the build, for output that a lexer already produces. A low dependency count is a stated goal of deco, and the terminal build's 44 crates contribute to its fast startup.

If the lexer's limitations become more important than that cost, the solution is a real parser that replaces this crate.

## Performance

The lexer state at the start of each line is cached. The spans are recomputed for the visible lines, which is cheap and cannot become stale. An edit invalidates the cache from the edited line onwards; earlier lines are unaffected, because a change on line 900 cannot change the state at the end of line 3.

Jumping to the end of a large file lexes the whole file once. This is unavoidable with multi-line constructs, because the state at line 9000 depends on all preceding lines.

## Not in the GPU frontend yet

`deco-gui` draws each line as one run in a single colour. Per-span colouring requires splitting each line into runs in the layout pass, which is not implemented. The terminal frontend supports it.
