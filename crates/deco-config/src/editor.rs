//! A typed, resolved view of the settings the editor reads on hot paths.
//!
//! Resolving `Value`s by string key on every keystroke would be both slow and
//! error-prone. [`EditorSettings::resolve`] does it once per document (settings
//! are resolved per language, so a Rust file and a Markdown file legitimately
//! get different values) and hands the rest of the editor plain fields.

use crate::settings::Settings;

/// `editor.wordWrap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WordWrap {
    /// Never wrap.
    #[default]
    Off,
    /// Wrap at the viewport width.
    On,
    /// Wrap at `editor.wordWrapColumn`.
    WordWrapColumn,
    /// Wrap at the smaller of the viewport and `editor.wordWrapColumn`.
    Bounded,
}

/// `editor.wrappingIndent`.
///
/// How far a continuation row is pushed in from the left. `Same` is VS Code's
/// default and the reason a wrapped block of code still reads as one block: without
/// it the second row of an indented line starts at column zero, next to the
/// unindented lines around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WrappingIndent {
    /// Continuation rows start at column zero.
    None,
    /// As deep as the line's own indentation.
    #[default]
    Same,
    /// One level deeper than the line.
    Indent,
    /// Two levels deeper.
    DeepIndent,
}

impl WrappingIndent {
    /// Extra indent levels beyond the line's own, or `None` for column zero.
    fn extra_levels(self) -> Option<usize> {
        match self {
            Self::None => None,
            Self::Same => Some(0),
            Self::Indent => Some(1),
            Self::DeepIndent => Some(2),
        }
    }
}

/// `editor.lineNumbers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineNumbers {
    /// Hidden.
    Off,
    /// Absolute numbers.
    #[default]
    On,
    /// Numbers relative to the cursor line.
    Relative,
    /// Every tenth line.
    Interval,
}

/// `editor.renderWhitespace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderWhitespace {
    /// Never.
    None,
    /// Only within selections.
    #[default]
    Selection,
    /// Only between words.
    Boundary,
    /// Only trailing whitespace.
    Trailing,
    /// Always.
    All,
}

/// `editor.cursorStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorStyle {
    /// A vertical bar.
    #[default]
    Line,
    /// A filled block.
    Block,
    /// An underline.
    Underline,
    /// A thin vertical bar.
    LineThin,
    /// A hollow block.
    BlockOutline,
    /// A thin underline.
    UnderlineThin,
}

/// `files.eol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EolSetting {
    /// Keep whatever the file already uses.
    #[default]
    Auto,
    /// Always `\n`.
    Lf,
    /// Always `\r\n`.
    Crlf,
}

/// Settings the editor core and both frontends consult.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorSettings {
    /// `editor.tabSize`.
    pub tab_size: usize,
    /// `editor.insertSpaces`.
    pub insert_spaces: bool,
    /// `editor.detectIndentation`.
    pub detect_indentation: bool,
    /// `editor.wordSeparators`.
    pub word_separators: String,
    /// `editor.wordWrap`.
    pub word_wrap: WordWrap,
    /// `editor.wordWrapColumn`.
    pub word_wrap_column: usize,
    /// `editor.wrappingIndent`.
    pub wrapping_indent: WrappingIndent,
    /// `editor.lineNumbers`.
    pub line_numbers: LineNumbers,
    /// `editor.renderWhitespace`.
    pub render_whitespace: RenderWhitespace,
    /// `editor.cursorStyle`.
    pub cursor_style: CursorStyle,
    /// `editor.cursorSurroundingLines` — scrolloff.
    pub cursor_surrounding_lines: usize,
    /// `editor.scrollBeyondLastLine`.
    pub scroll_beyond_last_line: bool,
    /// `editor.rulers`.
    pub rulers: Vec<usize>,
    /// `editor.fontFamily`.
    pub font_family: String,
    /// `editor.fontSize`.
    pub font_size: f32,
    /// `editor.lineHeight`; `0` means "derive from the font size".
    pub line_height: f32,
    /// `workbench.colorTheme`.
    pub color_theme: String,
    /// `files.eol`.
    pub eol: EolSetting,
    /// `files.trimTrailingWhitespace`.
    pub trim_trailing_whitespace: bool,
    /// `files.insertFinalNewline`.
    pub insert_final_newline: bool,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self::resolve(&Settings::with_defaults(), None)
    }
}

impl EditorSettings {
    /// Resolves the settings for a document of `language`.
    ///
    /// Unknown enum spellings fall back to the default rather than failing:
    /// a typo in `settings.json` should not stop the editor from opening the
    /// file, and VS Code behaves the same way.
    pub fn resolve(settings: &Settings, language: Option<&str>) -> Self {
        let s = settings;
        let l = language;
        Self {
            tab_size: s.get_u64("editor.tabSize", l).unwrap_or(4).clamp(1, 64) as usize,
            insert_spaces: s.get_bool("editor.insertSpaces", l).unwrap_or(true),
            detect_indentation: s.get_bool("editor.detectIndentation", l).unwrap_or(true),
            word_separators: s
                .get_str("editor.wordSeparators", l)
                .unwrap_or("`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?")
                .to_owned(),
            word_wrap: match s.get_str("editor.wordWrap", l) {
                Some("on") => WordWrap::On,
                Some("wordWrapColumn") => WordWrap::WordWrapColumn,
                Some("bounded") => WordWrap::Bounded,
                _ => WordWrap::Off,
            },
            word_wrap_column: s.get_u64("editor.wordWrapColumn", l).unwrap_or(80).max(1) as usize,
            wrapping_indent: match s.get_str("editor.wrappingIndent", l) {
                Some("none") => WrappingIndent::None,
                Some("indent") => WrappingIndent::Indent,
                Some("deepIndent") => WrappingIndent::DeepIndent,
                _ => WrappingIndent::Same,
            },
            line_numbers: match s.get_str("editor.lineNumbers", l) {
                Some("off") => LineNumbers::Off,
                Some("relative") => LineNumbers::Relative,
                Some("interval") => LineNumbers::Interval,
                _ => LineNumbers::On,
            },
            render_whitespace: match s.get_str("editor.renderWhitespace", l) {
                Some("none") => RenderWhitespace::None,
                Some("boundary") => RenderWhitespace::Boundary,
                Some("trailing") => RenderWhitespace::Trailing,
                Some("all") => RenderWhitespace::All,
                _ => RenderWhitespace::Selection,
            },
            cursor_style: match s.get_str("editor.cursorStyle", l) {
                Some("block") => CursorStyle::Block,
                Some("underline") => CursorStyle::Underline,
                Some("line-thin") => CursorStyle::LineThin,
                Some("block-outline") => CursorStyle::BlockOutline,
                Some("underline-thin") => CursorStyle::UnderlineThin,
                _ => CursorStyle::Line,
            },
            cursor_surrounding_lines: s.get_u64("editor.cursorSurroundingLines", l).unwrap_or(0)
                as usize,
            scroll_beyond_last_line: s.get_bool("editor.scrollBeyondLastLine", l).unwrap_or(true),
            rulers: s
                .get_as::<Vec<u64>>("editor.rulers", l)
                .unwrap_or_default()
                .into_iter()
                .map(|v| v as usize)
                .collect(),
            font_family: s
                .get_str("editor.fontFamily", l)
                .unwrap_or("monospace")
                .to_owned(),
            font_size: s
                .get_f64("editor.fontSize", l)
                .unwrap_or(14.0)
                .clamp(4.0, 200.0) as f32,
            line_height: s.get_f64("editor.lineHeight", l).unwrap_or(0.0).max(0.0) as f32,
            color_theme: s
                .get_str("workbench.colorTheme", None)
                .unwrap_or("Default Dark Modern")
                .to_owned(),
            eol: match s.get_str("files.eol", l) {
                Some("\n") => EolSetting::Lf,
                Some("\r\n") => EolSetting::Crlf,
                _ => EolSetting::Auto,
            },
            trim_trailing_whitespace: s
                .get_bool("files.trimTrailingWhitespace", l)
                .unwrap_or(false),
            insert_final_newline: s.get_bool("files.insertFinalNewline", l).unwrap_or(false),
        }
    }

    /// How far a continuation row of a line indented `leading` columns is pushed in.
    ///
    /// Capped at half the available `width`: past that a wrapped line is more indent
    /// than text, and a deeply nested line would be wrapped into a column two
    /// characters wide. VS Code caps it for the same reason. The cap drops the indent
    /// rather than trimming it, because a partial indent lines the continuation up
    /// with nothing.
    pub fn wrapping_prefix(&self, leading: usize, width: usize) -> usize {
        let Some(levels) = self.wrapping_indent.extra_levels() else {
            return 0;
        };
        let prefix = leading + levels * self.tab_size.max(1);
        if prefix * 2 > width {
            0
        } else {
            prefix
        }
    }

    /// The string one indent level inserts.
    pub fn indent_unit(&self) -> String {
        if self.insert_spaces {
            " ".repeat(self.tab_size)
        } else {
            "\t".to_owned()
        }
    }

    /// The rendered line height, deriving it from the font size when
    /// `editor.lineHeight` is left at 0.
    pub fn effective_line_height(&self) -> f32 {
        if self.line_height > 0.0 {
            self.line_height
        } else {
            // VS Code's own heuristic for "unset".
            (self.font_size * 1.5).round()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Scope;

    fn resolve(user_json: &str, language: Option<&str>) -> EditorSettings {
        let mut settings = Settings::with_defaults();
        settings.load_layer(Scope::User, user_json).unwrap();
        EditorSettings::resolve(&settings, language)
    }

    #[test]
    fn defaults_resolve_to_vscode_values() {
        let s = EditorSettings::default();
        assert_eq!(s.tab_size, 4);
        assert!(s.insert_spaces);
        assert_eq!(s.word_wrap, WordWrap::Off);
        assert_eq!(s.line_numbers, LineNumbers::On);
        assert_eq!(s.render_whitespace, RenderWhitespace::Selection);
        assert_eq!(s.eol, EolSetting::Auto);
    }

    #[test]
    fn user_values_override_defaults() {
        let s = resolve(
            r#"{"editor.tabSize": 2, "editor.insertSpaces": false}"#,
            None,
        );
        assert_eq!(s.tab_size, 2);
        assert!(!s.insert_spaces);
    }

    #[test]
    fn language_specific_values_are_applied() {
        let json =
            r#"{"editor.tabSize": 2, "[go]": {"editor.tabSize": 8, "editor.insertSpaces": false}}"#;
        assert_eq!(resolve(json, Some("go")).tab_size, 8);
        assert!(!resolve(json, Some("go")).insert_spaces);
        assert_eq!(resolve(json, Some("json")).tab_size, 2);
    }

    #[test]
    fn enum_spellings_are_parsed() {
        assert_eq!(
            resolve(r#"{"editor.wordWrap": "on"}"#, None).word_wrap,
            WordWrap::On
        );
        assert_eq!(
            resolve(r#"{"editor.wordWrap": "bounded"}"#, None).word_wrap,
            WordWrap::Bounded
        );
        assert_eq!(
            resolve(r#"{"editor.lineNumbers": "relative"}"#, None).line_numbers,
            LineNumbers::Relative
        );
        assert_eq!(
            resolve(r#"{"editor.cursorStyle": "block"}"#, None).cursor_style,
            CursorStyle::Block
        );
        assert_eq!(
            resolve(r#"{"files.eol": "\r\n"}"#, None).eol,
            EolSetting::Crlf
        );
    }

    #[test]
    fn unknown_enum_spellings_fall_back_to_the_default() {
        assert_eq!(
            resolve(r#"{"editor.wordWrap": "sideways"}"#, None).word_wrap,
            WordWrap::Off
        );
        assert_eq!(
            resolve(r#"{"editor.lineNumbers": 42}"#, None).line_numbers,
            LineNumbers::On
        );
    }

    #[test]
    fn out_of_range_numbers_are_clamped() {
        assert_eq!(resolve(r#"{"editor.tabSize": 0}"#, None).tab_size, 1);
        assert_eq!(resolve(r#"{"editor.tabSize": 9999}"#, None).tab_size, 64);
        assert_eq!(resolve(r#"{"editor.fontSize": 0}"#, None).font_size, 4.0);
    }

    #[test]
    fn rulers_are_read_as_a_list() {
        assert_eq!(
            resolve(r#"{"editor.rulers": [80, 120]}"#, None).rulers,
            vec![80, 120]
        );
        assert!(resolve(r#"{"editor.rulers": "nonsense"}"#, None)
            .rulers
            .is_empty());
    }

    #[test]
    fn indent_unit_follows_insert_spaces() {
        assert_eq!(
            resolve(r#"{"editor.tabSize": 2}"#, None).indent_unit(),
            "  "
        );
        assert_eq!(
            resolve(r#"{"editor.insertSpaces": false}"#, None).indent_unit(),
            "\t"
        );
    }

    #[test]
    fn line_height_is_derived_when_unset() {
        assert_eq!(
            resolve(r#"{"editor.fontSize": 14}"#, None).effective_line_height(),
            21.0
        );
        assert_eq!(
            resolve(r#"{"editor.lineHeight": 30}"#, None).effective_line_height(),
            30.0
        );
    }

    #[test]
    fn the_colour_theme_is_not_language_scoped() {
        // A `[rust]` section must not be able to change the whole workbench
        // theme just because a Rust file happens to be focused.
        let s = resolve(
            r#"{"workbench.colorTheme": "Solarized Light", "[rust]": {"workbench.colorTheme": "Monokai"}}"#,
            Some("rust"),
        );
        assert_eq!(s.color_theme, "Solarized Light");
    }
}
