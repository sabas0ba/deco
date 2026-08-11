//! An open document and the view onto it.

use std::path::{Path, PathBuf};

use deco_config::EditorSettings;
use deco_core::{Buffer, History, LineEnding, Position, SelectionSet, Transaction};
use deco_syntax::Syntax;

/// Guesses a language id from a path, using the associations deco knows about
/// before any extension has contributed its own.
pub fn language_for_path(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    let by_name = match name {
        "Cargo.toml" | "Cargo.lock" => Some("toml"),
        "Makefile" | "makefile" | "GNUmakefile" => Some("makefile"),
        "Dockerfile" => Some("dockerfile"),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "sh" | "bash" | "zsh" => "shellscript",
        "json" => "json",
        "jsonc" => "jsonc",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "sql" => "sql",
        "lua" => "lua",
        "xml" => "xml",
        _ => return None,
    })
}

/// Every language deco knows an identifier for, and what to call it.
///
/// # Why this is a list and not derived from [`language_for_path`]
///
/// Detection maps *file names* to identifiers, and several identifiers share one
/// pattern while others have none — `plaintext` is nothing's extension. This is
/// the other direction: what a user may choose, and what to show them for it.
///
/// The identifier is the part that matters and the part a title does not tell
/// you: it is what `[rust]` in a `settings.json` refers to, what a language
/// server is matched on, and what picks the lexer. So the picker shows both.
///
/// Written in the order the picker lists them — by title, ignoring case — so the
/// source reads as the list a user sees.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("c", "C"),
    ("cpp", "C++"),
    ("css", "CSS"),
    ("dockerfile", "Dockerfile"),
    ("go", "Go"),
    ("html", "HTML"),
    ("java", "Java"),
    ("javascript", "JavaScript"),
    ("javascriptreact", "JavaScript React"),
    ("json", "JSON"),
    ("jsonc", "JSON with Comments"),
    ("lua", "Lua"),
    ("makefile", "Makefile"),
    ("markdown", "Markdown"),
    ("plaintext", "Plain Text"),
    ("python", "Python"),
    ("ruby", "Ruby"),
    ("rust", "Rust"),
    ("shellscript", "Shell Script"),
    ("sql", "SQL"),
    ("toml", "TOML"),
    ("typescript", "TypeScript"),
    ("typescriptreact", "TypeScript React"),
    ("xml", "XML"),
    ("yaml", "YAML"),
];

/// What to call `language`, or the identifier itself if deco has no name for it.
///
/// An unknown identifier can arrive from a `settings.json` or a server, and
/// showing it verbatim is more useful than showing nothing.
pub fn language_title(language: &str) -> &str {
    LANGUAGES
        .iter()
        .find(|(id, _)| *id == language)
        .map(|(_, title)| *title)
        .unwrap_or(language)
}

/// The token that starts a line comment in `language`, if it has one.
pub fn line_comment_token(language: Option<&str>) -> Option<&'static str> {
    Some(match language? {
        "rust" | "typescript" | "typescriptreact" | "javascript" | "javascriptreact" | "go"
        | "c" | "cpp" | "java" | "css" | "jsonc" => "//",
        "python" | "shellscript" | "yaml" | "toml" | "ruby" | "makefile" | "dockerfile" => "#",
        "lua" | "sql" => "--",
        _ => return None,
    })
}

/// The tokens that open and close a block comment in `language`.
///
/// # What is deliberately absent
///
/// - **Shell, YAML, TOML, Makefile, Dockerfile.** They have no block comment,
///   and neither does VS Code claim one for them.
/// - **Ruby**, whose `=begin` / `=end` must each sit alone at the start of a
///   line. VS Code offers them anyway; wrapping a selection in the middle of a
///   line with them produces text Ruby will not parse, so deco says the language
///   has none rather than corrupting the file.
/// - **JSON**, which has no comments at all. `jsonc` does.
///
/// **Python's `"""` is a string, not a comment.** It is what VS Code inserts and
/// what a Python programmer means by commenting a block out, and it does disable
/// the code — but as an expression statement, so it is only sound where a
/// statement is allowed. Matching VS Code here beats inventing a different answer.
///
/// HTML, XML and Markdown appear even though [`crate::document`] has no lexer for
/// them: wrapping a selection needs the delimiters, not a grammar.
pub fn block_comment_tokens(language: Option<&str>) -> Option<(&'static str, &'static str)> {
    Some(match language? {
        "rust" | "typescript" | "typescriptreact" | "javascript" | "javascriptreact" | "go"
        | "c" | "cpp" | "java" | "css" | "jsonc" | "sql" => ("/*", "*/"),
        "html" | "xml" | "markdown" => ("<!--", "-->"),
        "lua" => ("--[[", "]]"),
        "python" => ("\"\"\"", "\"\"\""),
        _ => return None,
    })
}

/// An open document.
#[derive(Debug)]
pub struct Document {
    /// The text.
    pub buffer: Buffer,
    /// Undo/redo for this document.
    pub history: History,
    /// Where it came from, if it was loaded from disk.
    pub path: Option<PathBuf>,
    /// The detected language id.
    pub language_id: Option<String>,
    /// Settings resolved for this document's language.
    pub settings: EditorSettings,
    /// Whether the buffer differs from what is on disk.
    pub dirty: bool,
    /// Highlighting for this document's language.
    ///
    /// Lives here rather than in a frontend because the lexer state entering each
    /// line is a property of the text, not of a screen — and because both
    /// frontends would otherwise keep their own copy of it.
    pub syntax: Syntax,
}

impl Document {
    /// A new, empty, untitled document.
    pub fn untitled(settings: EditorSettings) -> Self {
        Self {
            buffer: Buffer::new(),
            history: History::default(),
            path: None,
            language_id: None,
            settings,
            dirty: false,
            syntax: Syntax::new(None),
        }
    }

    /// A document backed by `path` with contents `text`.
    pub fn from_file(path: PathBuf, text: &str, settings: EditorSettings) -> Self {
        let language_id = language_for_path(&path).map(str::to_owned);
        let mut buffer = Buffer::from_text(text);
        match settings.eol {
            deco_config::EolSetting::Lf => buffer.set_line_ending(LineEnding::Lf),
            deco_config::EolSetting::Crlf => buffer.set_line_ending(LineEnding::Crlf),
            // `auto` keeps whatever the file already used.
            deco_config::EolSetting::Auto => {}
        }
        Self {
            buffer,
            history: History::default(),
            path: Some(path),
            syntax: Syntax::new(language_id.as_deref()),
            language_id,
            settings,
            dirty: false,
        }
    }

    /// Applies `transaction` to the text, returning its inverse.
    ///
    /// The one place the buffer is mutated by an edit, so that everything derived
    /// from the text is invalidated without each caller having to remember to.
    /// Highlighting is the first such thing; there will be more.
    pub fn apply(&mut self, transaction: &Transaction) -> Transaction {
        // From the earliest line the edit touched. Everything above it is still
        // true — a change on line 900 cannot alter what line 3 left open — which
        // is what keeps editing a large file from re-lexing all of it.
        let first = transaction
            .changes()
            .iter()
            .map(|change| change.range.start.line as usize)
            .min()
            .unwrap_or(0);
        self.syntax.invalidate_from(first);
        self.buffer.apply(transaction)
    }

    /// Marks everything derived from the text as unknown.
    ///
    /// For a change that did not come through [`Document::apply`] — undo and redo
    /// apply their own transactions inside the history.
    pub fn invalidate(&mut self) {
        self.syntax.invalidate_from(0);
    }

    /// The name to show in the tab and status bar.
    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_owned())
    }

    /// The language id, if one was detected.
    pub fn language(&self) -> Option<&str> {
        self.language_id.as_deref()
    }

    /// The string one indent level inserts in this document.
    pub fn indent_unit(&self) -> String {
        self.settings.indent_unit()
    }
}

/// The visible window onto a document, plus this view's cursors.
///
/// A view is per-pane rather than per-document: the same file split across two
/// panes has two independent cursors and scroll positions, which is why these
/// do not live on [`Document`].
#[derive(Debug, Clone)]
pub struct View {
    /// The cursors.
    pub selections: SelectionSet,
    /// The first visible line.
    pub scroll_top: usize,
    /// The leftmost visible display column.
    pub scroll_left: usize,
    /// Height of the text area in lines.
    pub height: usize,
    /// Width of the text area in columns.
    pub width: usize,
    /// Any chord waiting for its second keypress.
    pub chord: deco_keymap::ChordState,
}

impl Default for View {
    fn default() -> Self {
        Self {
            selections: SelectionSet::default(),
            scroll_top: 0,
            scroll_left: 0,
            height: 24,
            width: 80,
            chord: deco_keymap::ChordState::new(),
        }
    }
}

impl View {
    /// Scrolls the minimum amount needed to bring the primary cursor into view,
    /// honouring `editor.cursorSurroundingLines`.
    pub fn reveal_cursor(&mut self, buffer: &Buffer, settings: &EditorSettings) {
        let line = self.selections.primary().active.line as usize;
        let margin = settings
            .cursor_surrounding_lines
            .min(self.height.saturating_sub(1) / 2);

        if line < self.scroll_top + margin {
            self.scroll_top = line.saturating_sub(margin);
        }
        let last_visible = self.scroll_top + self.height.saturating_sub(1);
        if line + margin > last_visible {
            self.scroll_top = line + margin + 1 - self.height.max(1);
        }

        // Never scroll past the end unless the setting explicitly allows it.
        if !settings.scroll_beyond_last_line {
            let max_top = buffer.line_count().saturating_sub(self.height.max(1));
            self.scroll_top = self.scroll_top.min(max_top);
        }
    }

    /// The range of lines currently visible.
    pub fn visible_lines(&self, buffer: &Buffer) -> std::ops::Range<usize> {
        let end = (self.scroll_top + self.height).min(buffer.line_count());
        self.scroll_top.min(end)..end
    }

    /// The primary cursor's position.
    pub fn cursor(&self) -> Position {
        self.selections.primary().active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> EditorSettings {
        EditorSettings::default()
    }

    #[test]
    fn detects_languages_from_extensions() {
        assert_eq!(language_for_path(Path::new("src/main.rs")), Some("rust"));
        assert_eq!(
            language_for_path(Path::new("a/b.TSX")),
            Some("typescriptreact")
        );
        assert_eq!(language_for_path(Path::new("x.unknown")), None);
        assert_eq!(language_for_path(Path::new("noext")), None);
    }

    #[test]
    fn detects_languages_from_whole_filenames() {
        assert_eq!(language_for_path(Path::new("/w/Cargo.toml")), Some("toml"));
        assert_eq!(
            language_for_path(Path::new("/w/Makefile")),
            Some("makefile")
        );
        assert_eq!(
            language_for_path(Path::new("/w/Dockerfile")),
            Some("dockerfile")
        );
    }

    #[test]
    fn knows_line_comment_tokens() {
        assert_eq!(line_comment_token(Some("rust")), Some("//"));
        assert_eq!(line_comment_token(Some("python")), Some("#"));
        assert_eq!(line_comment_token(Some("lua")), Some("--"));
        assert_eq!(line_comment_token(Some("html")), None);
        assert_eq!(line_comment_token(None), None);
    }

    #[test]
    fn a_document_from_a_file_picks_up_its_language() {
        let doc = Document::from_file(PathBuf::from("/w/main.rs"), "fn main() {}", settings());
        assert_eq!(doc.language(), Some("rust"));
        assert_eq!(doc.title(), "main.rs");
        assert!(!doc.dirty);
    }

    #[test]
    fn an_untitled_document_is_named_untitled() {
        assert_eq!(Document::untitled(settings()).title(), "Untitled");
    }

    #[test]
    fn files_eol_auto_keeps_the_documents_own_line_ending() {
        let doc = Document::from_file(PathBuf::from("/w/a.txt"), "a\r\nb\r\n", settings());
        assert_eq!(doc.buffer.line_ending(), LineEnding::Crlf);
        assert_eq!(doc.buffer.to_disk_string(), "a\r\nb\r\n");
    }

    #[test]
    fn revealing_the_cursor_scrolls_the_minimum_needed() {
        let buffer = Buffer::from_text(&"x\n".repeat(100));
        let mut view = View {
            height: 10,
            ..Default::default()
        };

        view.selections = SelectionSet::caret(Position::new(50, 0));
        view.reveal_cursor(&buffer, &settings());
        assert_eq!(
            view.scroll_top, 41,
            "should scroll just enough to show line 50"
        );

        // Already visible: nothing moves.
        let before = view.scroll_top;
        view.selections = SelectionSet::caret(Position::new(45, 0));
        view.reveal_cursor(&buffer, &settings());
        assert_eq!(view.scroll_top, before);
    }

    #[test]
    fn revealing_the_cursor_scrolls_back_up() {
        let buffer = Buffer::from_text(&"x\n".repeat(100));
        let mut view = View {
            height: 10,
            scroll_top: 50,
            ..Default::default()
        };
        view.selections = SelectionSet::caret(Position::new(20, 0));
        view.reveal_cursor(&buffer, &settings());
        assert_eq!(view.scroll_top, 20);
    }

    #[test]
    fn cursor_surrounding_lines_keeps_a_margin() {
        let buffer = Buffer::from_text(&"x\n".repeat(100));
        let mut settings = settings();
        settings.cursor_surrounding_lines = 3;
        let mut view = View {
            height: 10,
            scroll_top: 50,
            ..Default::default()
        };

        view.selections = SelectionSet::caret(Position::new(50, 0));
        view.reveal_cursor(&buffer, &settings);
        assert_eq!(
            view.scroll_top, 47,
            "three lines of context above the cursor"
        );
    }

    #[test]
    fn visible_lines_never_run_past_the_buffer() {
        let buffer = Buffer::from_text("a\nb\nc");
        let view = View {
            height: 50,
            scroll_top: 0,
            ..Default::default()
        };
        assert_eq!(view.visible_lines(&buffer), 0..3);
    }
}
