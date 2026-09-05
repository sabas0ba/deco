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

/// The bracket and quote pairs `editor.autoClosingBrackets` closes in `language`.
///
/// # Why a table and not one list
///
/// The pairs are a property of the language, which is what VS Code's
/// `languageDefined` means — and two of them matter enough to be worth the table:
///
/// - **Rust's `'` is a lifetime**, not a quote. `&'a str` is ordinary, and closing it
///   would put `&''a str` on the screen every time somebody wrote one. rust-analyzer's
///   own language configuration leaves it out for the same reason.
/// - **Markdown, HTML and XML have no `'` pair** either: an apostrophe in prose is
///   far more common there than a quoted string, and `don''t` is worse than nothing.
///
/// A language deco has no entry for gets the brackets and the double quote, which are
/// the pairs every language in the table shares.
pub fn bracket_pairs(language: Option<&str>) -> &'static [(char, char)] {
    const BRACKETS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"')];
    const WITH_APOSTROPHE: &[(char, char)] =
        &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];
    const BACKTICK: &[(char, char)] = &[
        ('(', ')'),
        ('[', ']'),
        ('{', '}'),
        ('"', '"'),
        ('\'', '\''),
        ('`', '`'),
    ];
    match language {
        // A template literal is a quote in these, and the pair is worth having.
        Some("typescript" | "typescriptreact" | "javascript" | "javascriptreact") => BACKTICK,
        // The apostrophe is a lifetime, an apostrophe, or a tag delimiter.
        Some("rust" | "markdown" | "html" | "xml") => BRACKETS,
        Some(_) | None => WITH_APOSTROPHE,
    }
}

/// An open document.
#[derive(Debug)]
pub struct Document {
    /// Navigation state belongs to the document, so another tab cannot reuse it.
    pub(crate) snippet: Option<crate::snippet::ActiveSnippet>,
    /// The text.
    pub buffer: Buffer,
    /// Undo/redo for this document.
    pub history: History,
    /// Where it came from, if it was loaded from disk.
    pub path: Option<PathBuf>,
    /// The detected language id.
    pub language_id: Option<String>,
    /// Whether the language was chosen by hand rather than from the file name.
    ///
    /// Recorded rather than worked out by comparing the language against what
    /// the extension implies: picking Rust for a `.rs` file — to pin it before a
    /// rename, say — is indistinguishable from never having picked anything if
    /// the two are only compared by value, and the choice would then be lost the
    /// moment the file was renamed to something the extension no longer covers.
    pub language_pinned: bool,
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
    /// What the file's own text said about its indentation, if anything.
    ///
    /// Read once, when the file is opened, and re-applied by
    /// [`Document::apply_overrides`] every time the settings are resolved again. Its
    /// own field rather than folded straight into [`Document::settings`] for two
    /// reasons: a language change re-resolves those from scratch and would throw the
    /// answer away, and re-reading it would mean copying the whole file to be told
    /// what it already said.
    pub indentation: deco_config::indent::Guess,
    /// `alt+z`'s answer for this document, or `None` to follow `editor.wordWrap`.
    ///
    /// Here for the same reason: the keyboard said it, so re-resolving the settings
    /// must not un-say it.
    pub wrap_override: Option<deco_config::WordWrap>,
    /// Lines whose leading whitespace was inserted by an auto-indent and not typed.
    ///
    /// `editor.trimAutoWhitespace` removes it rather than letting one press of enter
    /// too many leave a line of trailing spaces in a diff. Each entry is a line and
    /// how many UTF-16 units of whitespace were put there.
    ///
    /// A record, not an authority: before anything is deleted the line is checked
    /// against the buffer, and only a line that still holds exactly that whitespace
    /// and nothing else is trimmed. So a stale entry cannot cost anybody their text —
    /// the worst it can do is nothing.
    pub auto_whitespace: Vec<(u32, u32)>,
    /// Whether the file's indentation differed from the settings, and won.
    ///
    /// Not merely "was something detected": a two-space file read as two-space when
    /// `editor.tabSize` already said two overrode nothing, and saying so would put a
    /// permanent note in the status bar for the case where there is nothing to
    /// disclose. Set by [`Document::apply_overrides`], which is the only place that
    /// can see both answers at once.
    pub indentation_overridden: bool,
}

/// The line ending `files.eol` asks for, or `None` when it defers.
///
/// `auto` is the deferral, and it is the only value that does not name an
/// ending. Callers decide when the setting gets to speak at all — see
/// [`Document::from_file`], where it does so only for a file with no ending of
/// its own.
fn configured_eol(setting: deco_config::EolSetting) -> Option<LineEnding> {
    match setting {
        deco_config::EolSetting::Lf => Some(LineEnding::Lf),
        deco_config::EolSetting::Crlf => Some(LineEnding::Crlf),
        deco_config::EolSetting::Auto => None,
    }
}

impl Document {
    /// A new, empty, untitled document.
    ///
    /// Nothing to detect: an empty buffer has no indentation to read, so the
    /// settings stand until something is typed. VS Code does not re-guess as you
    /// type either — the guess is about a file that already exists.
    pub fn untitled(settings: EditorSettings) -> Self {
        let mut buffer = Buffer::new();
        // There is no text to detect from, so `files.eol` — "the default end of
        // line character" — is exactly what it names here.
        if let Some(eol) = configured_eol(settings.eol) {
            buffer.set_line_ending(eol);
        }
        Self {
            buffer,
            snippet: None,
            history: History::default(),
            path: None,
            language_id: None,
            language_pinned: false,
            settings,
            indentation: deco_config::indent::Guess::default(),
            wrap_override: None,
            indentation_overridden: false,
            auto_whitespace: Vec::new(),
            dirty: false,
            syntax: Syntax::new(None),
        }
    }

    /// A document backed by `path` with contents `text`.
    pub fn from_file(path: PathBuf, text: &str, settings: EditorSettings) -> Self {
        let language_id = language_for_path(&path).map(str::to_owned);
        let mut buffer = Buffer::from_text(text);
        // A file that already has an ending keeps it, whatever `files.eol` says:
        // VS Code documents the key as the ending a *new* file gets, and applying
        // it on open rewrites every line of a file the user came to read. The
        // ending is changed deliberately or not at all.
        //
        // A file with no terminator in it — empty, or one line without a break —
        // has nothing to keep, and that is where the setting decides.
        if LineEnding::detected(text).is_none() {
            if let Some(eol) = configured_eol(settings.eol) {
                buffer.set_line_ending(eol);
            }
        }
        let mut document = Self {
            buffer,
            snippet: None,
            history: History::default(),
            path: Some(path),
            syntax: Syntax::new(language_id.as_deref()),
            language_id,
            language_pinned: false,
            settings,
            indentation: deco_config::indent::guess(text),
            wrap_override: None,
            indentation_overridden: false,
            auto_whitespace: Vec::new(),
            dirty: false,
        };
        document.apply_overrides();
        document
    }

    /// Re-applies what the file and the keyboard have said, over the settings.
    ///
    /// [`crate::Session`] calls this after every re-resolution — a workspace layer
    /// arriving, a rename, a language change — because each of those replaces the
    /// whole [`EditorSettings`] and would otherwise discard two answers that did not
    /// come from `settings.json`.
    ///
    /// `editor.detectIndentation` is read from the freshly resolved settings, so
    /// turning it off in workspace settings takes effect here rather than needing the
    /// file to be reopened.
    pub fn apply_overrides(&mut self) {
        let configured = (self.settings.insert_spaces, self.settings.tab_size);
        if self.settings.detect_indentation {
            // The two halves are independent. A tab-indented file settles
            // `insertSpaces` and says nothing about how wide to draw a tab, so
            // `editor.tabSize` still decides that — which is what VS Code does.
            if let Some(spaces) = self.indentation.insert_spaces {
                self.settings.insert_spaces = spaces;
            }
            if let Some(size) = self.indentation.tab_size {
                self.settings.tab_size = size;
            }
        }
        self.indentation_overridden =
            (self.settings.insert_spaces, self.settings.tab_size) != configured;
        if let Some(wrap) = self.wrap_override {
            self.settings.word_wrap = wrap;
        }
    }

    /// Applies `transaction` to the text, returning its inverse.
    ///
    /// The one place the buffer is mutated by an edit, so that everything derived
    /// from the text is invalidated without each caller having to remember to.
    /// Highlighting is the first such thing; there will be more.
    pub fn apply(&mut self, transaction: &Transaction) -> Transaction {
        if self
            .snippet
            .as_mut()
            .is_some_and(|snippet| !snippet.apply(transaction))
        {
            self.snippet = None;
        }
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
        self.snippet = None;
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
    /// Which wrapped row of [`View::scroll_top`] is the first on screen.
    ///
    /// Zero unless the line is wrapped and the window starts partway down it.
    /// The scroll position is anchored to a document line and an offset within it,
    /// rather than to a count of rows from the top of the file, because counting
    /// rows from the top means wrapping the whole file to find out where the
    /// window is — on every keystroke, for a file of any size. Anchored this way,
    /// scrolling and drawing both cost the height of the window.
    pub scroll_row: usize,
    /// The leftmost visible display column.
    ///
    /// Meaningless while wrapping, where nothing extends past the right edge.
    pub scroll_left: usize,
    /// Height of the text area in lines.
    pub height: usize,
    /// Width of the text area in columns.
    pub width: usize,
    /// Columns this group leaves for text: its own width less its gutter.
    ///
    /// The frontend's layout decides it — how many groups are on screen, how wide
    /// a gutter this document needs — and [`crate::Session::resize`] computes it
    /// from [`crate::layout`] so that both the wrap and the drawing use one
    /// answer. Zero means "not laid out yet", which wraps nothing.
    pub text_width: usize,
    /// Any chord waiting for its second keypress.
    pub chord: deco_keymap::ChordState,
}

impl Default for View {
    fn default() -> Self {
        Self {
            selections: SelectionSet::default(),
            scroll_top: 0,
            scroll_row: 0,
            scroll_left: 0,
            height: 24,
            width: 80,
            text_width: 0,
            chord: deco_keymap::ChordState::new(),
        }
    }
}

/// One document line as a string, or empty past the end of the buffer.
fn line_text(buffer: &Buffer, line: usize) -> String {
    buffer
        .line_content(line)
        .map(|slice| slice.to_string())
        .unwrap_or_default()
}

/// One row on screen: which document line it shows, and which part of it.
///
/// A line that is not wrapped produces exactly one of these, so a renderer has
/// one path rather than two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRow {
    /// The document line.
    pub line: usize,
    /// Which row of that line this is, counting from zero.
    ///
    /// Zero is where the line number goes: a continuation row leaves the gutter
    /// blank, because repeating the number would read as a line that is not there.
    pub row: usize,
    /// UTF-16 column the row starts at.
    pub start: u32,
    /// UTF-16 column the next row starts at, or `None` on a line's last row —
    /// which runs to the end of the line.
    pub end: Option<u32>,
    /// Display columns of blank this row's text is pushed in by.
    ///
    /// Zero on a line's first row. `editor.wrappingIndent` decides the rest, and the
    /// renderer has to draw exactly this much or the text lands somewhere the wrap did
    /// not put it.
    pub indent: usize,
}

impl VisualRow {
    /// Whether this row carries the line's number.
    pub fn numbered(&self) -> bool {
        self.row == 0
    }

    /// Whether `column` falls on this row.
    ///
    /// The end is exclusive, except on a line's last row, where a caret sitting
    /// one past the final character still belongs here — there is no next row for
    /// it to belong to.
    pub fn holds(&self, column: u32) -> bool {
        column >= self.start && self.end.is_none_or(|end| column < end)
    }
}

impl View {
    /// The column this view wraps at, or zero when it does not wrap.
    ///
    /// `editor.wordWrap` decides which of the two widths applies:
    /// `"on"` follows the window, `"wordWrapColumn"` ignores it, and `"bounded"`
    /// takes whichever is narrower — which is the one that keeps prose readable on
    /// a wide screen without letting a narrow one wrap twice.
    pub fn wrap_column(&self, settings: &EditorSettings) -> usize {
        // No width means nothing has laid this group out yet, or the frontend does
        // not wrap — see `Session::frontend_wraps`. Either way there is nowhere to
        // break, and that holds for `wordWrapColumn` too: wrapping in the session
        // while the frontend draws one line per row would scroll and move the caret
        // by rows nobody draws.
        if self.text_width == 0 {
            return 0;
        }
        match settings.word_wrap {
            deco_config::WordWrap::Off => 0,
            deco_config::WordWrap::On => self.text_width,
            deco_config::WordWrap::WordWrapColumn => settings.word_wrap_column,
            deco_config::WordWrap::Bounded => settings.word_wrap_column.min(self.text_width),
        }
    }

    /// Where a document line is broken, or a single row when it is not wrapped.
    pub fn row_starts(&self, buffer: &Buffer, settings: &EditorSettings, line: usize) -> Vec<u32> {
        let wrap = self.wrap_column(settings);
        if wrap == 0 {
            return vec![0];
        }
        let text = line_text(buffer, line);
        let indent = self.wrapping_indent_of(&text, settings, wrap);
        deco_core::wrap::row_starts(&text, wrap, settings.tab_size, indent)
    }

    /// How far this line's continuation rows are pushed in.
    ///
    /// `editor.wrappingIndent` decides, from the line's own leading whitespace —
    /// which is why it is measured here, where the text is, rather than resolved once
    /// with the rest of the settings.
    fn wrapping_indent_of(&self, text: &str, settings: &EditorSettings, wrap: usize) -> usize {
        let leading: String = text.chars().take_while(|c| c.is_whitespace()).collect();
        let columns = deco_core::wrap::width_between(
            &leading,
            0,
            leading.chars().map(|c| c.len_utf16() as u32).sum(),
            settings.tab_size,
        );
        settings.wrapping_prefix(columns, wrap)
    }

    /// How far the row `position` sits on is pushed in.
    ///
    /// Zero on a line's first row, which starts where the line starts.
    pub fn row_indent(
        &self,
        buffer: &Buffer,
        settings: &EditorSettings,
        position: Position,
    ) -> usize {
        let wrap = self.wrap_column(settings);
        if wrap == 0 {
            return 0;
        }
        let line = position.line as usize;
        let starts = self.row_starts(buffer, settings, line);
        if deco_core::wrap::row_of(&starts, position.character) == 0 {
            return 0;
        }
        self.wrapping_indent_of(&line_text(buffer, line), settings, wrap)
    }

    /// How many rows a document line occupies.
    fn line_rows(&self, buffer: &Buffer, settings: &EditorSettings, line: usize) -> usize {
        self.row_starts(buffer, settings, line).len()
    }

    /// Which row of its line `position` sits on.
    pub fn row_of(&self, buffer: &Buffer, settings: &EditorSettings, position: Position) -> usize {
        let starts = self.row_starts(buffer, settings, position.line as usize);
        deco_core::wrap::row_of(&starts, position.character)
    }

    /// The rows on screen, from the scroll anchor down.
    ///
    /// Shorter than the height only at the end of the document. Costs the height
    /// of the window and not the length of the file, which is the whole reason the
    /// anchor is a line rather than a row count.
    pub fn visible_rows(&self, buffer: &Buffer, settings: &EditorSettings) -> Vec<VisualRow> {
        let wrap = self.wrap_column(settings);
        let mut rows = Vec::with_capacity(self.height);
        let mut line = self.scroll_top;
        let mut row = self.scroll_row;
        while rows.len() < self.height && line < buffer.line_count() {
            let starts = self.row_starts(buffer, settings, line);
            if row >= starts.len() {
                // The anchor points past the end of a line that has since been
                // shortened — by an edit, or by the window getting wider.
                line += 1;
                row = 0;
                continue;
            }
            let (start, end) = deco_core::wrap::row_range(&starts, row);
            rows.push(VisualRow {
                line,
                row,
                start,
                end,
                indent: if row == 0 || wrap == 0 {
                    0
                } else {
                    self.wrapping_indent_of(&line_text(buffer, line), settings, wrap)
                },
            });
            row += 1;
            if row >= starts.len() {
                line += 1;
                row = 0;
            }
        }
        rows
    }

    /// Scrolls the minimum amount needed to bring the primary cursor into view,
    /// honouring `editor.cursorSurroundingLines`.
    pub fn reveal_cursor(&mut self, buffer: &Buffer, settings: &EditorSettings) {
        if self.wrap_column(settings) == 0 {
            self.scroll_row = 0;
            self.reveal_cursor_unwrapped(buffer, settings);
            return;
        }
        self.reveal_cursor_wrapped(buffer, settings);
    }

    fn reveal_cursor_unwrapped(&mut self, buffer: &Buffer, settings: &EditorSettings) {
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

    /// The same in rows rather than lines.
    ///
    /// Every walk here is bounded by the height of the window: a cursor further
    /// away than that re-anchors on itself instead of being counted towards.
    fn reveal_cursor_wrapped(&mut self, buffer: &Buffer, settings: &EditorSettings) {
        let cursor = buffer.clamp_position(self.selections.primary().active);
        let at = (cursor.line as usize, self.row_of(buffer, settings, cursor));
        let margin = settings
            .cursor_surrounding_lines
            .min(self.height.saturating_sub(1) / 2);
        let last = self.height.saturating_sub(1);

        // `None` when the cursor is above the anchor or further below it than the
        // window is tall; either way the answer is to re-anchor on the cursor.
        match self.rows_to(buffer, settings, at) {
            Some(distance) if distance >= margin && distance + margin <= last => {}
            Some(distance) if distance + margin > last => {
                let (line, row) = self.back(buffer, settings, at, last.saturating_sub(margin));
                self.scroll_top = line;
                self.scroll_row = row;
            }
            _ => {
                let (line, row) = self.back(buffer, settings, at, margin);
                self.scroll_top = line;
                self.scroll_row = row;
            }
        }

        if !settings.scroll_beyond_last_line {
            // The furthest the window may sit is one where its last row is the
            // document's last row. Found by walking back from the end, which costs
            // the height of the window rather than the length of the file.
            let last_line = buffer.line_count().saturating_sub(1);
            let end = (
                last_line,
                self.line_rows(buffer, settings, last_line)
                    .saturating_sub(1),
            );
            let furthest = self.back(buffer, settings, end, last);
            if (self.scroll_top, self.scroll_row) > furthest {
                (self.scroll_top, self.scroll_row) = furthest;
            }
        }
    }

    /// How many rows from the anchor down to `to`, or `None` when `to` is above
    /// the anchor or more than a window away from it.
    fn rows_to(
        &self,
        buffer: &Buffer,
        settings: &EditorSettings,
        to: (usize, usize),
    ) -> Option<usize> {
        if to < (self.scroll_top, self.scroll_row) {
            return None;
        }
        let mut rows = 0usize;
        let mut line = self.scroll_top;
        let mut row = self.scroll_row;
        while (line, row) != to {
            if rows > self.height || line >= buffer.line_count() {
                return None;
            }
            row += 1;
            if row >= self.line_rows(buffer, settings, line) {
                line += 1;
                row = 0;
            }
            rows += 1;
        }
        Some(rows)
    }

    /// The anchor `count` rows above `from`, stopping at the start of the file.
    fn back(
        &self,
        buffer: &Buffer,
        settings: &EditorSettings,
        from: (usize, usize),
        count: usize,
    ) -> (usize, usize) {
        let (mut line, mut row) = from;
        for _ in 0..count {
            if row > 0 {
                row -= 1;
            } else if line > 0 {
                line -= 1;
                row = self.line_rows(buffer, settings, line).saturating_sub(1);
            } else {
                break;
            }
        }
        (line, row)
    }

    /// The UTF-16 columns `position`'s own row covers.
    pub fn row_bounds(
        &self,
        buffer: &Buffer,
        settings: &EditorSettings,
        position: Position,
    ) -> (u32, Option<u32>) {
        let starts = self.row_starts(buffer, settings, position.line as usize);
        let row = deco_core::wrap::row_of(&starts, position.character);
        deco_core::wrap::row_range(&starts, row)
    }

    /// Which column of the text area `position` is drawn in.
    ///
    /// This is what a vertical motion keeps constant, and it is a column **on
    /// screen** — the row's own indent included. Measured from the line's start
    /// instead it would be a number with no meaning on screen; measured from the
    /// row's text instead, `down` would step sideways every time two rows are pushed
    /// in by different amounts, which is exactly what `editor.wrappingIndent` does.
    pub fn goal_column(
        &self,
        buffer: &Buffer,
        settings: &EditorSettings,
        position: Position,
    ) -> usize {
        let (start, _) = self.row_bounds(buffer, settings, position);
        let indent = self.row_indent(buffer, settings, position);
        indent
            + deco_core::wrap::width_between_from(
                &line_text(buffer, position.line as usize),
                start,
                position.character,
                settings.tab_size,
                indent,
            )
    }

    /// The position `count` rows away from `from`, keeping `goal` display columns
    /// into the row.
    ///
    /// Rows, not lines: with wrapping on, one press of `down` moves one row, which
    /// is what the key looks like it does. Moving by line instead would skip over
    /// however many rows the current line happens to occupy, and in prose that is
    /// most of a paragraph.
    pub fn step_rows(
        &self,
        buffer: &Buffer,
        settings: &EditorSettings,
        from: Position,
        down: bool,
        count: usize,
        goal: usize,
    ) -> Position {
        let mut line = from.line as usize;
        let mut row = self.row_of(buffer, settings, from);
        for _ in 0..count {
            if down {
                let rows = self.line_rows(buffer, settings, line);
                if row + 1 < rows {
                    row += 1;
                } else if line + 1 < buffer.line_count() {
                    line += 1;
                    row = 0;
                } else {
                    // Already on the last row of the last line. Stopping here
                    // rather than at the end of the document matches what `down`
                    // does when wrapping is off.
                    break;
                }
            } else if row > 0 {
                row -= 1;
            } else if line > 0 {
                line -= 1;
                row = self.line_rows(buffer, settings, line).saturating_sub(1);
            } else {
                break;
            }
        }

        let text = line_text(buffer, line);
        let starts = self.row_starts(buffer, settings, line);
        let (start, end) = deco_core::wrap::row_range(&starts, row);
        let indent = if row == 0 {
            0
        } else {
            self.wrapping_indent_of(&text, settings, self.wrap_column(settings))
        };
        // `goal` is a column of the text area, so the target row's indent comes off
        // it: landing the same distance into two rows pushed in by different amounts
        // would move the caret sideways. A goal inside the indent lands at the row's
        // first character, which is the nearest column there is.
        let character = deco_core::wrap::column_in_row_from(
            &text,
            start,
            end,
            goal.saturating_sub(indent),
            settings.tab_size,
            indent,
        );
        Position::new(line as u32, character)
    }

    /// The range of lines currently visible.
    ///
    /// Counts one row per line, so it over-reports while wrapping — a window four
    /// rows tall may be showing one line. [`View::visible_rows`] is the wrap-aware
    /// answer, and what a renderer wants.
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

    /// `settings()` with `files.eol` set to `eol`.
    fn settings_with_eol(eol: deco_config::EolSetting) -> EditorSettings {
        EditorSettings { eol, ..settings() }
    }

    #[test]
    fn files_eol_leaves_an_existing_files_own_line_ending_alone() {
        // The whole point: opening a CRLF file with `"files.eol": "\n"` must not
        // stage a rewrite of every line in it.
        let doc = Document::from_file(
            PathBuf::from("/w/a.txt"),
            "a\r\nb\r\n",
            settings_with_eol(deco_config::EolSetting::Lf),
        );
        assert_eq!(doc.buffer.line_ending(), LineEnding::Crlf);
        assert_eq!(doc.buffer.to_disk_string(), "a\r\nb\r\n");

        let doc = Document::from_file(
            PathBuf::from("/w/b.txt"),
            "a\nb\n",
            settings_with_eol(deco_config::EolSetting::Crlf),
        );
        assert_eq!(doc.buffer.line_ending(), LineEnding::Lf);
        assert_eq!(doc.buffer.to_disk_string(), "a\nb\n");
    }

    #[test]
    fn files_eol_decides_for_a_file_with_no_line_ending_to_keep() {
        // Nothing to detect, so the setting is all there is to go on — which is
        // what "the default end of line character" means.
        for (text, expected) in [("", LineEnding::Crlf), ("one line", LineEnding::Crlf)] {
            let doc = Document::from_file(
                PathBuf::from("/w/a.txt"),
                text,
                settings_with_eol(deco_config::EolSetting::Crlf),
            );
            assert_eq!(doc.buffer.line_ending(), expected, "for {text:?}");
        }
    }

    #[test]
    fn files_eol_gives_an_untitled_buffer_its_line_ending() {
        let doc = Document::untitled(settings_with_eol(deco_config::EolSetting::Crlf));
        assert_eq!(doc.buffer.line_ending(), LineEnding::Crlf);
        let doc = Document::untitled(settings_with_eol(deco_config::EolSetting::Lf));
        assert_eq!(doc.buffer.line_ending(), LineEnding::Lf);
    }

    #[test]
    fn an_untitled_buffer_falls_back_to_the_platform_under_auto() {
        let doc = Document::untitled(settings_with_eol(deco_config::EolSetting::Auto));
        assert_eq!(doc.buffer.line_ending(), LineEnding::platform_default());
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

    // ---- Wrapping ---------------------------------------------------------

    /// Settings that wrap at the window's width.
    fn wrapping() -> EditorSettings {
        EditorSettings {
            word_wrap: deco_config::WordWrap::On,
            ..EditorSettings::default()
        }
    }

    /// A view `text_width` columns wide and `height` rows tall.
    fn view(text_width: usize, height: usize) -> View {
        View {
            text_width,
            height,
            ..Default::default()
        }
    }

    #[test]
    fn wrapping_off_leaves_a_line_as_one_row() {
        let buffer = Buffer::from_text(&"x".repeat(200));
        let view = view(20, 5);
        assert_eq!(view.wrap_column(&EditorSettings::default()), 0);
        let rows = view.visible_rows(&buffer, &EditorSettings::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].start, 0);
        assert_eq!(rows[0].end, None, "the row runs to the end of the line");
    }

    #[test]
    fn a_wrapped_line_occupies_several_rows() {
        let buffer = Buffer::from_text("aaaaa bbbbb ccccc\nshort\n");
        let rows = view(6, 10).visible_rows(&buffer, &wrapping());
        let shape: Vec<(usize, usize)> = rows.iter().map(|r| (r.line, r.row)).collect();
        assert_eq!(shape, [(0, 0), (0, 1), (0, 2), (1, 0), (2, 0)]);
        assert!(rows[0].numbered() && !rows[1].numbered());
    }

    #[test]
    fn bounded_takes_whichever_of_the_two_widths_is_narrower() {
        let settings = EditorSettings {
            word_wrap: deco_config::WordWrap::Bounded,
            word_wrap_column: 40,
            ..EditorSettings::default()
        };
        assert_eq!(view(100, 5).wrap_column(&settings), 40, "the column");
        assert_eq!(view(20, 5).wrap_column(&settings), 20, "the window");
    }

    #[test]
    fn a_wrap_column_ignores_the_window() {
        // Which is the point of `wordWrapColumn`: the text keeps its measure on a
        // wide screen instead of running the whole way across it.
        let settings = EditorSettings {
            word_wrap: deco_config::WordWrap::WordWrapColumn,
            word_wrap_column: 30,
            ..EditorSettings::default()
        };
        assert_eq!(view(200, 5).wrap_column(&settings), 30);
    }

    #[test]
    fn the_window_can_start_partway_down_a_wrapped_line() {
        let buffer = Buffer::from_text("aaaaa bbbbb ccccc\n");
        let mut view = view(6, 2);
        view.scroll_row = 1;
        let rows = view.visible_rows(&buffer, &wrapping());
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].line, rows[0].row), (0, 1));
        assert_eq!((rows[1].line, rows[1].row), (0, 2));
    }

    #[test]
    fn revealing_a_caret_further_down_its_own_line_scrolls_by_rows() {
        // The whole point. Anchoring on the line would leave the window showing
        // the line's first row with the caret several rows below the bottom.
        let buffer = Buffer::from_text(&format!("{}\n", "word ".repeat(20)));
        let mut view = view(10, 4);
        view.selections = SelectionSet::caret(Position::new(0, 95));
        view.reveal_cursor(&buffer, &wrapping());
        assert_eq!(view.scroll_top, 0, "still the only line");
        assert!(view.scroll_row > 0, "but not its first row");

        let rows = view.visible_rows(&buffer, &wrapping());
        assert!(
            rows.iter().any(|row| row.holds(95)),
            "the caret's row is on screen: {rows:?}"
        );
    }

    #[test]
    fn revealing_a_caret_above_the_window_scrolls_back_by_rows() {
        let buffer = Buffer::from_text(&format!("{}\n", "word ".repeat(20)));
        let mut view = view(10, 4);
        view.scroll_row = 8;
        view.selections = SelectionSet::caret(Position::new(0, 2));
        view.reveal_cursor(&buffer, &wrapping());
        assert_eq!((view.scroll_top, view.scroll_row), (0, 0));
    }

    #[test]
    fn an_anchor_past_the_end_of_a_shortened_line_recovers() {
        // An edit or a wider window can leave the anchor pointing at a row the
        // line no longer has. Drawing nothing at all would look like a hang.
        let buffer = Buffer::from_text("short\nsecond\nthird\n");
        let mut view = view(20, 3);
        view.scroll_row = 7;
        let rows = view.visible_rows(&buffer, &wrapping());
        assert_eq!(
            rows.iter().map(|r| r.line).collect::<Vec<_>>(),
            [1, 2, 3],
            "carries on from the next line"
        );
    }

    #[test]
    fn the_last_row_of_the_file_can_be_scrolled_to_and_no_further() {
        // Clamping to `line_count - height`, which is what the unwrapped path
        // does, would stop with rows still below the bottom of the window: a
        // wrapped file has more rows than lines, so counting in lines undershoots.
        let buffer = Buffer::from_text(&format!("{}\n", "word ".repeat(20)));
        let mut view = view(10, 4);
        let settings = EditorSettings {
            scroll_beyond_last_line: false,
            ..wrapping()
        };
        view.selections = SelectionSet::caret(buffer.end_position());
        view.reveal_cursor(&buffer, &settings);

        let rows = view.visible_rows(&buffer, &settings);
        assert_eq!(rows.len(), 4, "the window is full: {rows:?}");
        let last = rows.last().unwrap();
        assert_eq!(last.line, buffer.line_count() - 1);
        assert_eq!(last.end, None, "and it is that line's last row");
    }

    #[test]
    fn scroll_beyond_last_line_lets_the_window_run_past_the_end() {
        let buffer = Buffer::from_text(&format!("{}\n", "word ".repeat(20)));
        let mut view = view(10, 6);
        let settings = EditorSettings {
            scroll_beyond_last_line: true,
            ..wrapping()
        };
        view.scroll_row = 9;
        view.selections = SelectionSet::caret(Position::new(0, 95));
        view.reveal_cursor(&buffer, &settings);
        let rows = view.visible_rows(&buffer, &settings);
        assert!(rows.len() < 6, "the end of the file is on screen: {rows:?}");
    }

    #[test]
    fn a_goal_column_is_measured_within_the_row() {
        // Not from the start of the line: on screen the row is the line, and a
        // goal counted from the line's start would put every vertical motion
        // through a wrapped paragraph at the wrong column.
        let buffer = Buffer::from_text("aaaaa bbbbb ccccc\n");
        let view = view(6, 5);
        let settings = wrapping();
        assert_eq!(view.goal_column(&buffer, &settings, Position::new(0, 8)), 2);
    }

    #[test]
    fn stepping_down_a_row_stays_on_the_same_line() {
        let buffer = Buffer::from_text("aaaaa bbbbb ccccc\nnext\n");
        let view = view(6, 5);
        let settings = wrapping();
        let from = Position::new(0, 1);
        let next = view.step_rows(&buffer, &settings, from, true, 1, 1);
        assert_eq!(next, Position::new(0, 7), "the second row, one column in");
    }

    #[test]
    fn stepping_down_past_a_lines_last_row_reaches_the_next_line() {
        let buffer = Buffer::from_text("aaaaa bbbbb\nnext\n");
        let view = view(6, 5);
        let settings = wrapping();
        let next = view.step_rows(&buffer, &settings, Position::new(0, 7), true, 1, 0);
        assert_eq!(next, Position::new(1, 0));
    }

    #[test]
    fn stepping_stops_at_the_ends_of_the_document() {
        let buffer = Buffer::from_text("aaaaa bbbbb\n");
        let view = view(6, 5);
        let settings = wrapping();
        assert_eq!(
            view.step_rows(&buffer, &settings, Position::new(0, 0), false, 9, 0),
            Position::new(0, 0),
            "up from the first row"
        );
        let last = buffer.line_count() - 1;
        assert_eq!(
            view.step_rows(
                &buffer,
                &settings,
                Position::new(last as u32, 0),
                true,
                9,
                0
            )
            .line as usize,
            last,
            "down from the last"
        );
    }
}
