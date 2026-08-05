//! Built-in themes and the workbench-colour defaults that fill a theme's gaps.

use crate::color::Rgba;
use crate::theme::{ColorTheme, ThemeKind};

/// `0xRRGGBB` as an opaque colour.
const fn hex(v: u32) -> Rgba {
    Rgba::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// `0xRRGGBBAA` as a colour with alpha.
const fn hexa(v: u32) -> Rgba {
    Rgba::new((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// `(key, dark, light)` defaults, taken from VS Code's Dark Modern and Light
/// Modern.
///
/// Only the keys deco's own frontends read are listed. Adding a key here is a
/// commitment to honour it, so an unlisted key returning `None` is the honest
/// signal that nothing draws it yet.
#[rustfmt::skip]
const DEFAULTS: &[(&str, Rgba, Rgba)] = &[
    ("foreground", hex(0xcccccc), hex(0x3b3b3b)),
    ("focusBorder", hex(0x0078d4), hex(0x0090f1)),
    ("errorForeground", hex(0xf85149), hex(0xe51400)),
    // Editor surface
    ("editor.background", hex(0x1f1f1f), hex(0xffffff)),
    ("editor.foreground", hex(0xcccccc), hex(0x3b3b3b)),
    ("editorCursor.foreground", hex(0xaeafad), hex(0x000000)),
    ("editor.selectionBackground", hex(0x264f78), hex(0xadd6ff)),
    (
        "editor.selectionHighlightBackground",
        hexa(0xadd6ff26),
        hexa(0xadd6ff80),
    ),
    (
        "editor.lineHighlightBackground",
        hexa(0xffffff0a),
        hexa(0x0000000a),
    ),
    ("editor.findMatchBackground", hex(0x9e6a03), hex(0xa8ac94)),
    (
        "editor.findMatchHighlightBackground",
        hexa(0xea5c0055),
        hexa(0xea5c0055),
    ),
    ("editorLineNumber.foreground", hex(0x6e7681), hex(0x6e7681)),
    (
        "editorLineNumber.activeForeground",
        hex(0xcccccc),
        hex(0x171184),
    ),
    (
        "editorWhitespace.foreground",
        hexa(0xe3e4e229),
        hexa(0x33333333),
    ),
    (
        "editorIndentGuide.background1",
        hex(0x404040),
        hex(0xd3d3d3),
    ),
    ("editorRuler.foreground", hex(0x5a5a5a), hex(0xd3d3d3)),
    ("editorError.foreground", hex(0xf14c4c), hex(0xe51400)),
    ("editorWarning.foreground", hex(0xcca700), hex(0xbf8803)),
    ("editorInfo.foreground", hex(0x3794ff), hex(0x1a85ff)),
    ("editorGutter.background", hex(0x1f1f1f), hex(0xffffff)),
    // Widgets
    ("editorWidget.background", hex(0x202020), hex(0xf8f8f8)),
    ("editorWidget.border", hex(0x454545), hex(0xc8c8c8)),
    ("input.background", hex(0x313131), hex(0xffffff)),
    ("input.foreground", hex(0xcccccc), hex(0x3b3b3b)),
    ("dropdown.background", hex(0x313131), hex(0xffffff)),
    (
        "list.activeSelectionBackground",
        hex(0x04395e),
        hex(0xe4e6f1),
    ),
    ("list.hoverBackground", hex(0x2a2d2e), hex(0xe8e8e8)),
    (
        "scrollbarSlider.background",
        hexa(0x79797966),
        hexa(0x64646466),
    ),
    ("badge.background", hex(0x616161), hex(0xcccccc)),
    ("badge.foreground", hex(0xf8f8f8), hex(0x3b3b3b)),
    // Chrome
    ("statusBar.background", hex(0x181818), hex(0xf8f8f8)),
    ("statusBar.foreground", hex(0xcccccc), hex(0x3b3b3b)),
    ("sideBar.background", hex(0x181818), hex(0xf8f8f8)),
    ("sideBar.foreground", hex(0xcccccc), hex(0x3b3b3b)),
    ("activityBar.background", hex(0x181818), hex(0xf8f8f8)),
    ("activityBar.foreground", hex(0xd7d7d7), hex(0x1f1f1f)),
    ("tab.activeBackground", hex(0x1f1f1f), hex(0xffffff)),
    ("tab.activeForeground", hex(0xffffff), hex(0x3b3b3b)),
    ("tab.inactiveBackground", hex(0x181818), hex(0xf8f8f8)),
    ("tab.inactiveForeground", hex(0x9d9d9d), hex(0x868686)),
    ("tab.border", hex(0x2b2b2b), hex(0xe5e5e5)),
    ("panel.background", hex(0x181818), hex(0xf8f8f8)),
    ("panel.border", hex(0x2b2b2b), hex(0xe5e5e5)),
    ("terminal.background", hex(0x181818), hex(0xffffff)),
    ("terminal.foreground", hex(0xcccccc), hex(0x3b3b3b)),
];

/// High-contrast variants override a handful of keys; everything else falls
/// through to the matching dark or light default.
const HC_DARK_OVERRIDES: &[(&str, Rgba)] = &[
    ("editor.background", hex(0x000000)),
    ("editor.foreground", hex(0xffffff)),
    ("editor.selectionBackground", hex(0xffffff)),
    ("editorCursor.foreground", hex(0xffffff)),
    ("focusBorder", hex(0xf38518)),
    ("statusBar.background", hex(0x000000)),
    ("sideBar.background", hex(0x000000)),
    ("panel.background", hex(0x000000)),
    ("terminal.background", hex(0x000000)),
];

const HC_LIGHT_OVERRIDES: &[(&str, Rgba)] = &[
    ("editor.background", hex(0xffffff)),
    ("editor.foreground", hex(0x292929)),
    ("editorCursor.foreground", hex(0x000000)),
    ("focusBorder", hex(0x006bbd)),
];

/// The built-in default for `key` under `kind`, or `None` if deco has no
/// opinion about that key.
pub fn default_color(kind: ThemeKind, key: &str) -> Option<Rgba> {
    let overrides = match kind {
        ThemeKind::HighContrastDark => HC_DARK_OVERRIDES,
        ThemeKind::HighContrastLight => HC_LIGHT_OVERRIDES,
        _ => &[],
    };
    if let Some((_, color)) = overrides.iter().find(|(k, _)| *k == key) {
        return Some(*color);
    }
    DEFAULTS.iter().find(|(k, _, _)| *k == key).map(
        |(_, dark, light)| {
            if kind.is_dark() {
                *dark
            } else {
                *light
            }
        },
    )
}

/// Other keys `key` may borrow its value from, in preference order.
///
/// VS Code derives most workbench colours from a few base ones. Following the
/// chain means a theme that only sets `editor.foreground` still gets sensible
/// line numbers and tab labels instead of deco's generic defaults.
pub fn fallback_chain(key: &str) -> &'static [&'static str] {
    match key {
        "foreground" => &["editor.foreground"],
        "editorCursor.foreground" => &["editor.foreground"],
        "editorLineNumber.activeForeground" => &["editor.foreground"],
        "editorGutter.background" => &["editor.background"],
        "editorWidget.background" => &["editor.background"],
        "panel.background" => &["editor.background"],
        "terminal.background" => &["panel.background", "editor.background"],
        "terminal.foreground" => &["editor.foreground", "foreground"],
        "statusBar.background" => &["editor.background"],
        "statusBar.foreground" => &["foreground", "editor.foreground"],
        "sideBar.background" => &["editor.background"],
        "sideBar.foreground" => &["foreground", "editor.foreground"],
        "activityBar.background" => &["sideBar.background", "editor.background"],
        "tab.activeBackground" => &["editor.background"],
        "tab.inactiveBackground" => &["editor.background"],
        "tab.activeForeground" => &["editor.foreground"],
        "tab.inactiveForeground" => &["tab.activeForeground", "editor.foreground"],
        "input.background" => &["editor.background"],
        "input.foreground" => &["editor.foreground"],
        "dropdown.background" => &["input.background", "editor.background"],
        _ => &[],
    }
}

/// deco's built-in dark theme, matching VS Code's Dark Modern closely enough
/// that switching editors is not visually jarring.
pub const DARK_MODERN_JSONC: &str = r##"{
    "name": "Default Dark Modern",
    "type": "dark",
    "semanticHighlighting": true,
    "colors": {
        "editor.background": "#1f1f1f",
        "editor.foreground": "#cccccc",
        "editorCursor.foreground": "#aeafad",
        "editor.selectionBackground": "#264f78",
        "editor.lineHighlightBackground": "#ffffff0a",
        "editorLineNumber.foreground": "#6e7681",
        "editorLineNumber.activeForeground": "#cccccc",
        "statusBar.background": "#181818",
        "statusBar.foreground": "#cccccc",
        "sideBar.background": "#181818",
        "panel.background": "#181818",
        "terminal.background": "#181818",
        "tab.activeBackground": "#1f1f1f",
        "tab.inactiveBackground": "#181818"
    },
    "tokenColors": [
        { "settings": { "foreground": "#cccccc" } },
        { "scope": "comment", "settings": { "foreground": "#6a9955", "fontStyle": "italic" } },
        { "scope": "string", "settings": { "foreground": "#ce9178" } },
        { "scope": "constant.numeric", "settings": { "foreground": "#b5cea8" } },
        { "scope": "constant.language", "settings": { "foreground": "#569cd6" } },
        { "scope": "constant.character.escape", "settings": { "foreground": "#d7ba7d" } },
        { "scope": "keyword", "settings": { "foreground": "#569cd6" } },
        { "scope": "keyword.control", "settings": { "foreground": "#c586c0" } },
        { "scope": "keyword.operator", "settings": { "foreground": "#d4d4d4" } },
        { "scope": "storage", "settings": { "foreground": "#569cd6" } },
        { "scope": "storage.type", "settings": { "foreground": "#569cd6" } },
        { "scope": "entity.name.function", "settings": { "foreground": "#dcdcaa" } },
        { "scope": "entity.name.type, entity.name.class, support.type, support.class", "settings": { "foreground": "#4ec9b0" } },
        { "scope": "entity.name.tag", "settings": { "foreground": "#569cd6" } },
        { "scope": "entity.other.attribute-name", "settings": { "foreground": "#9cdcfe" } },
        { "scope": "variable", "settings": { "foreground": "#9cdcfe" } },
        { "scope": "variable.parameter", "settings": { "foreground": "#9cdcfe" } },
        { "scope": "variable.other.constant", "settings": { "foreground": "#4fc1ff" } },
        { "scope": "support.function", "settings": { "foreground": "#dcdcaa" } },
        { "scope": "punctuation", "settings": { "foreground": "#cccccc" } },
        { "scope": "invalid", "settings": { "foreground": "#f44747" } },
        { "scope": "markup.heading", "settings": { "foreground": "#569cd6", "fontStyle": "bold" } },
        { "scope": "markup.bold", "settings": { "fontStyle": "bold" } },
        { "scope": "markup.italic", "settings": { "fontStyle": "italic" } }
    ],
    "semanticTokenColors": {
        "namespace": "#4ec9b0",
        "class": "#4ec9b0",
        "enum": "#4ec9b0",
        "interface": "#b8d7a3",
        "struct": "#4ec9b0",
        "typeParameter": "#4ec9b0",
        "parameter": "#9cdcfe",
        "variable": "#9cdcfe",
        "variable.readonly": "#4fc1ff",
        "property": "#9cdcfe",
        "function": "#dcdcaa",
        "method": "#dcdcaa",
        "macro": "#dcdcaa",
        "*.deprecated": { "fontStyle": "strikethrough" }
    }
}"##;

/// deco's built-in light theme.
pub const LIGHT_MODERN_JSONC: &str = r##"{
    "name": "Default Light Modern",
    "type": "light",
    "semanticHighlighting": true,
    "colors": {
        "editor.background": "#ffffff",
        "editor.foreground": "#3b3b3b",
        "editorCursor.foreground": "#000000",
        "editor.selectionBackground": "#add6ff",
        "editor.lineHighlightBackground": "#0000000a",
        "editorLineNumber.foreground": "#6e7681",
        "editorLineNumber.activeForeground": "#171184",
        "statusBar.background": "#f8f8f8",
        "statusBar.foreground": "#3b3b3b",
        "sideBar.background": "#f8f8f8",
        "panel.background": "#f8f8f8",
        "terminal.background": "#ffffff",
        "tab.activeBackground": "#ffffff",
        "tab.inactiveBackground": "#f8f8f8"
    },
    "tokenColors": [
        { "settings": { "foreground": "#3b3b3b" } },
        { "scope": "comment", "settings": { "foreground": "#008000", "fontStyle": "italic" } },
        { "scope": "string", "settings": { "foreground": "#a31515" } },
        { "scope": "constant.numeric", "settings": { "foreground": "#098658" } },
        { "scope": "constant.language", "settings": { "foreground": "#0000ff" } },
        { "scope": "constant.character.escape", "settings": { "foreground": "#ee0000" } },
        { "scope": "keyword", "settings": { "foreground": "#0000ff" } },
        { "scope": "keyword.control", "settings": { "foreground": "#af00db" } },
        { "scope": "keyword.operator", "settings": { "foreground": "#3b3b3b" } },
        { "scope": "storage", "settings": { "foreground": "#0000ff" } },
        { "scope": "storage.type", "settings": { "foreground": "#0000ff" } },
        { "scope": "entity.name.function", "settings": { "foreground": "#795e26" } },
        { "scope": "entity.name.type, entity.name.class, support.type, support.class", "settings": { "foreground": "#267f99" } },
        { "scope": "entity.name.tag", "settings": { "foreground": "#800000" } },
        { "scope": "entity.other.attribute-name", "settings": { "foreground": "#e50000" } },
        { "scope": "variable", "settings": { "foreground": "#001080" } },
        { "scope": "variable.parameter", "settings": { "foreground": "#001080" } },
        { "scope": "variable.other.constant", "settings": { "foreground": "#0070c1" } },
        { "scope": "support.function", "settings": { "foreground": "#795e26" } },
        { "scope": "punctuation", "settings": { "foreground": "#3b3b3b" } },
        { "scope": "invalid", "settings": { "foreground": "#cd3131" } },
        { "scope": "markup.heading", "settings": { "foreground": "#0000ff", "fontStyle": "bold" } },
        { "scope": "markup.bold", "settings": { "fontStyle": "bold" } },
        { "scope": "markup.italic", "settings": { "fontStyle": "italic" } }
    ],
    "semanticTokenColors": {
        "namespace": "#267f99",
        "class": "#267f99",
        "enum": "#267f99",
        "interface": "#267f99",
        "struct": "#267f99",
        "typeParameter": "#267f99",
        "parameter": "#001080",
        "variable": "#001080",
        "variable.readonly": "#0070c1",
        "property": "#001080",
        "function": "#795e26",
        "method": "#795e26",
        "macro": "#795e26",
        "*.deprecated": { "fontStyle": "strikethrough" }
    }
}"##;

/// The names of the themes deco ships with.
pub const BUILTIN_THEME_NAMES: &[&str] = &["Default Dark Modern", "Default Light Modern"];

/// Loads a built-in theme by name.
pub fn builtin(name: &str) -> Option<ColorTheme> {
    let source = match name {
        "Default Dark Modern" => DARK_MODERN_JSONC,
        "Default Light Modern" => LIGHT_MODERN_JSONC,
        _ => return None,
    };
    ColorTheme::from_json(source).ok()
}

/// The theme used when `workbench.colorTheme` names something deco cannot find.
pub fn fallback_theme() -> ColorTheme {
    builtin("Default Dark Modern").expect("the built-in dark theme must parse")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::SemanticToken;

    #[test]
    fn both_builtin_themes_parse() {
        for name in BUILTIN_THEME_NAMES {
            let theme = builtin(name).unwrap_or_else(|| panic!("{name} failed to parse"));
            assert_eq!(theme.name, *name);
            assert!(!theme.rules().is_empty());
        }
    }

    #[test]
    fn an_unknown_builtin_name_is_none() {
        assert!(builtin("Monokai Pro").is_none());
    }

    #[test]
    fn the_builtin_themes_have_the_expected_kinds() {
        assert_eq!(
            builtin("Default Dark Modern").unwrap().kind,
            ThemeKind::Dark
        );
        assert_eq!(
            builtin("Default Light Modern").unwrap().kind,
            ThemeKind::Light
        );
    }

    #[test]
    fn the_builtin_themes_colour_common_scopes_differently() {
        for name in BUILTIN_THEME_NAMES {
            let theme = builtin(name).unwrap();
            let comment = theme.style_for_scopes(&["source.rust", "comment.line.rust"]);
            let string = theme.style_for_scopes(&["source.rust", "string.quoted.double.rust"]);
            let keyword = theme.style_for_scopes(&["source.rust", "keyword.control.rust"]);
            assert!(comment.foreground.is_some(), "{name} has no comment colour");
            assert_ne!(comment.foreground, string.foreground, "{name}");
            assert_ne!(string.foreground, keyword.foreground, "{name}");
            assert_eq!(comment.font_style.map(|f| f.italic), Some(true), "{name}");
        }
    }

    #[test]
    fn the_builtin_themes_style_semantic_tokens() {
        let theme = builtin("Default Dark Modern").unwrap();
        assert!(theme.semantic_highlighting());

        let readonly = theme
            .style_for_semantic(&SemanticToken::new("variable", &["readonly"], None))
            .unwrap();
        let plain = theme
            .style_for_semantic(&SemanticToken::new("variable", &[], None))
            .unwrap();
        assert_ne!(readonly.foreground, plain.foreground);

        let deprecated = theme
            .style_for_semantic(&SemanticToken::new("function", &["deprecated"], None))
            .unwrap();
        assert_eq!(deprecated.font_style.map(|f| f.strikethrough), Some(true));
    }

    #[test]
    fn defaults_exist_for_every_listed_key_in_both_polarities() {
        for (key, _, _) in DEFAULTS {
            assert!(default_color(ThemeKind::Dark, key).is_some(), "{key} dark");
            assert!(
                default_color(ThemeKind::Light, key).is_some(),
                "{key} light"
            );
        }
    }

    #[test]
    fn dark_and_light_defaults_differ_in_polarity() {
        let dark = default_color(ThemeKind::Dark, "editor.background").unwrap();
        let light = default_color(ThemeKind::Light, "editor.background").unwrap();
        assert!(dark.luminance() < light.luminance());
    }

    #[test]
    fn high_contrast_overrides_apply_over_the_base_table() {
        assert_eq!(
            default_color(ThemeKind::HighContrastDark, "editor.background"),
            Some(Rgba::BLACK)
        );
        // A key with no override falls through to the dark default.
        assert_eq!(
            default_color(ThemeKind::HighContrastDark, "editorLineNumber.foreground"),
            default_color(ThemeKind::Dark, "editorLineNumber.foreground")
        );
    }

    #[test]
    fn unknown_keys_have_no_default() {
        assert!(default_color(ThemeKind::Dark, "made.up.key").is_none());
    }

    #[test]
    fn every_fallback_target_is_itself_resolvable() {
        // A chain that points at a key with no default would silently produce
        // `None` for a colour the editor needs.
        for key in [
            "foreground",
            "editorCursor.foreground",
            "editorLineNumber.activeForeground",
            "terminal.background",
            "tab.inactiveForeground",
            "dropdown.background",
        ] {
            for target in fallback_chain(key) {
                assert!(
                    default_color(ThemeKind::Dark, target).is_some(),
                    "{key} falls back to {target}, which has no default"
                );
            }
        }
    }

    #[test]
    fn fallback_chains_terminate() {
        // A key must never appear in its own ancestry. Two keys legitimately
        // converging on `editor.background` is a diamond, not a cycle, so the
        // check tracks the current path rather than everything visited.
        fn walk(key: &str, path: &mut Vec<String>) {
            assert!(
                !path.iter().any(|p| p == key),
                "fallback cycle: {} -> {key}",
                path.join(" -> ")
            );
            path.push(key.to_owned());
            for next in fallback_chain(key) {
                walk(next, path);
            }
            path.pop();
        }

        for (key, _, _) in DEFAULTS {
            walk(key, &mut Vec::new());
        }
    }

    #[test]
    fn the_fallback_theme_is_the_dark_one() {
        assert_eq!(fallback_theme().name, "Default Dark Modern");
    }
}
