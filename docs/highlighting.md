# Syntax highlighting

Colours come from the theme, through the same scope-matching code that VS Code
themes are written against — so a theme you already use means the same thing here.

![The same theme colouring Rust, TypeScript, Python, TOML and JSON](img/highlighting.svg)

## How it works

`deco-theme` resolves a style from a **TextMate scope stack** and has done from
the start; what was missing was anything producing scope stacks. `deco-syntax` is
that: a lexer per language emitting the scope names themes style, and the renderer
asks the theme for a colour per run.

The scope stack is two deep — the language's `source.*` scope, then the token's —
so a theme's parent selectors (`meta.function entity.name`) have something to
match against.

| Scope emitted | What it is |
| --- | --- |
| `keyword.control` | `if`, `fn`, `return` |
| `entity.name.type` | `String`, `int`, a capitalised word where the language has that convention |
| `entity.name.function` | An identifier immediately followed by `(` |
| `constant.language` | `true`, `null`, `nil`, `None` |
| `constant.numeric` | Numeric literals |
| `string.quoted.double`, `string.quoted.single` | String literals |
| `comment.line.double-slash`, `comment.block` | Comments |

Scopes are specific but **not language-suffixed**: `keyword.control`, not
`keyword.control.rust`. A theme pattern matches a scope when it is a whole-segment
prefix of it, so both `keyword` and `keyword.control` style the above — which is
what themes actually contain. A rule written for `keyword.control.rust`
specifically would not match; that is the price of one static string per token kind
rather than one per kind per language.

## Languages

Rust, TypeScript, JavaScript (and the `react` variants), Python, Go, C, C++, Java,
JSON, JSONC, TOML, YAML, shell, Ruby, Lua, SQL, CSS, Makefile and Dockerfile.

Anything else renders in the theme's plain foreground. Markdown, HTML and XML are
**deliberately** absent: they are structural rather than token-oriented, and
colouring them by keyword would highlight the wrong halves of the file. Leaving
them plain is the honest answer until there is something that understands them.

Adding a language is a table in `crates/deco-syntax/src/languages.rs` and nothing
else.

## Choosing the language yourself

The language is worked out from the file name — its extension, or the whole name
for `Makefile`, `Dockerfile` and `Cargo.toml`. When that is wrong or when there is
nothing to go on, `ctrl+k m` picks one.

![Telling a .txt file that it is TOML](img/language-mode.svg)

The right-hand column is the **identifier**, not a second name for the language: it
is what `[toml]` in a `settings.json` refers to, what a language server is matched
on, and what selects the lexer. The title is for finding the row; the identifier is
the thing that acts.

Choosing one rebuilds everything downstream of it: the lexer, the settings — so a
`[toml]` block's `editor.tabSize` starts applying — and the `editorLangId` context
key, so a `when` clause means what it says. The terminal frontend also re-attaches
its language server, because a different language is a different server.

**Auto Detect** is the first row and the way back. Its own right-hand column says
what detection would decide, so choosing it is not a guess.

The text is never touched. Nothing about a document's bytes depends on which
language it is said to be, only on how it is read — so this is not an edit, and it
is not undoable.

| Key | Command |
| --- | --- |
| `ctrl+k m` | `workbench.action.editor.changeLanguageMode` |

The picker offers every identifier deco knows, including the ones with no lexer
(`markdown`, `html`, `xml`, `plaintext`) — they still select settings and a server,
which is most of what a language identifier is for.

## It is a lexer, not a parser

Worth stating plainly, because it bounds what you should expect.

VS Code's own highlighting is a set of regular-expression grammars — also a lexer.
So for colouring, a lexer gets most of the way there: keywords, strings, comments,
numbers and calls are all lexical properties. Multi-line constructs work too;
block comments and triple-quoted strings carry state from one line to the next,
and Rust's nested `/* /* */ */` nests correctly.

What a lexer cannot do:

- **Tell a type from a variable by how it was declared.** `Foo` is coloured as a
  type because it is capitalised, in languages where that convention holds. In
  Python, where `MAX_SIZE` is a constant rather than a type, deco does not guess.
- **Highlight a language inside another** — SQL in a string, CSS in HTML.
- **Distinguish a shadowed name, a macro from a function, a field from a method.**

The other half of that is a language server's **semantic tokens**, which carry
exactly the information a lexer lacks. Where a server provides them they are
drawn, and the lexer keeps colouring everything else — see
[Semantic tokens](language-servers.md#semantic-tokens).

## Why not tree-sitter

It was the obvious candidate and was rejected on the build cost. A tree-sitter
grammar is a generated C parser, compiled on every target — a new dependency per
language and a C toolchain in the build, for output a lexer already produces.
deco's dependency count is a stated goal, and the terminal build's 44 crates are
part of why it starts as fast as it does.

If the lexer's limits start to matter more than that cost, a real parser is the
answer and this crate is the thing to replace.

## Performance

The lexer state entering each line is cached; the spans themselves are recomputed
for the lines on screen, which is cheap and cannot go stale. An edit invalidates
from the edited line onwards — everything above it is still true, since a change on
line 900 cannot alter what line 3 left open.

Jumping to the end of a large file lexes it once. That is unavoidable while
multi-line constructs exist: nothing can know what line 9000 is inside without
having read what came before it.

## Not in the GPU frontend yet

`deco-gui` draws one run per line in a single colour. Per-span colouring there
means splitting each line into runs in its layout pass, which is not done. The
terminal frontend has it.
