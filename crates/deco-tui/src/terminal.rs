//! The integrated terminal in the terminal frontend: drawing its screen into
//! the panel, and starting the shell it runs.
//!
//! The screen's cells map one to one onto the panel's cells. Colours go
//! through the theme: the default colours are `terminal.foreground` and
//! `terminal.background`, and the sixteen ANSI colours are
//! `terminal.ansiBlack` to `terminal.ansiBrightWhite`, with VS Code's defaults
//! when the theme does not set them. The other 240 indexed colours are
//! xterm's fixed cube and grey ramp.

use std::ffi::OsString;
use std::path::Path;

use deco_config::{Scope, Settings};
use deco_editor::terminal::TerminalPanel;
use deco_term::screen::{Cell, Color, Line};
use deco_theme::Rgba;

use crate::render::{Row, Span};

/// The theme keys of the sixteen ANSI colours, in index order.
const ANSI_KEYS: [&str; 16] = [
    "terminal.ansiBlack",
    "terminal.ansiRed",
    "terminal.ansiGreen",
    "terminal.ansiYellow",
    "terminal.ansiBlue",
    "terminal.ansiMagenta",
    "terminal.ansiCyan",
    "terminal.ansiWhite",
    "terminal.ansiBrightBlack",
    "terminal.ansiBrightRed",
    "terminal.ansiBrightGreen",
    "terminal.ansiBrightYellow",
    "terminal.ansiBrightBlue",
    "terminal.ansiBrightMagenta",
    "terminal.ansiBrightCyan",
    "terminal.ansiBrightWhite",
];

/// VS Code's ANSI colours for dark themes.
const DARK_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
    0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xe5e5e5,
];

/// VS Code's ANSI colours for light themes.
const LIGHT_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x00bc00, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, 0x666666,
    0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
];

fn hex(value: u32) -> Rgba {
    Rgba::rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

/// The colours a terminal is drawn with.
pub struct Colors {
    fg: Rgba,
    bg: Rgba,
    ansi: [Rgba; 16],
    /// `terminal.integrated.drawBoldTextInBrightColors`.
    bold_is_bright: bool,
}

impl Colors {
    /// The theme's terminal colours, with `panel_fg` and `panel_bg` for the
    /// defaults when the theme has no terminal colours.
    pub fn from_theme(
        theme: &deco_theme::ColorTheme,
        settings: &Settings,
        panel_fg: Rgba,
        panel_bg: Rgba,
    ) -> Self {
        let defaults = if theme.kind.is_dark() {
            DARK_ANSI
        } else {
            LIGHT_ANSI
        };
        let mut ansi = [Rgba::BLACK; 16];
        for (index, color) in ansi.iter_mut().enumerate() {
            *color = theme
                .color(ANSI_KEYS[index])
                .unwrap_or_else(|| hex(defaults[index]));
        }
        let bg = theme
            .color("terminal.background")
            .or_else(|| theme.color("panel.background"))
            .unwrap_or(panel_bg);
        Self {
            fg: theme.color("terminal.foreground").unwrap_or(panel_fg),
            bg: bg.over(panel_bg),
            ansi,
            bold_is_bright: settings
                .get_bool("terminal.integrated.drawBoldTextInBrightColors", None)
                .unwrap_or(true),
        }
    }

    fn indexed(&self, index: u8) -> Rgba {
        match index {
            0..=15 => self.ansi[usize::from(index)],
            // The 6×6×6 cube.
            16..=231 => {
                let level = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
                let index = index - 16;
                Rgba::rgb(level(index / 36), level(index / 6 % 6), level(index % 6))
            }
            // The grey ramp.
            232..=255 => {
                let grey = 8 + (index - 232) * 10;
                Rgba::rgb(grey, grey, grey)
            }
        }
    }

    /// The colours a cell is drawn in.
    fn cell(&self, cell: &Cell) -> (Rgba, Rgba) {
        let style = cell.style;
        let attributes = style.attributes;
        let mut fg = match style.fg {
            Color::Default => self.fg,
            Color::Indexed(index @ 0..=7) if attributes.bold && self.bold_is_bright => {
                self.indexed(index + 8)
            }
            Color::Indexed(index) => self.indexed(index),
            Color::Rgb(r, g, b) => Rgba::rgb(r, g, b),
        };
        let mut bg = match style.bg {
            Color::Default => self.bg,
            Color::Indexed(index) => self.indexed(index),
            Color::Rgb(r, g, b) => Rgba::rgb(r, g, b),
        };
        if attributes.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if attributes.dim {
            fg = fg.with_alpha(160).over(bg);
        }
        if attributes.hidden {
            fg = bg;
        }
        (fg, bg)
    }
}

/// The terminal's rows, `width` by `height`.
pub fn rows(terminal: &TerminalPanel, colors: &Colors, width: usize, height: usize) -> Vec<Row> {
    let screen = terminal.screen();
    let back = terminal.back();
    let mut rows = Vec::with_capacity(height);
    for row in 0..height {
        rows.push(match screen.line(row, back) {
            Some(line) => line_row(line, colors, width),
            None => Row {
                spans: vec![Span {
                    text: " ".repeat(width),
                    fg: colors.bg,
                    bg: colors.bg,
                    italic: false,
                }],
            },
        });
    }
    rows
}

/// The cursor's column and row within the terminal's rows, when it should be
/// shown: the program has not hidden it, the view is not scrolled into the
/// history, and the program is still running.
pub fn cursor(terminal: &TerminalPanel, width: usize, height: usize) -> Option<(usize, usize)> {
    let screen = terminal.screen();
    let (row, col) = screen.cursor();
    (terminal.back() == 0
        && terminal.exited().is_none()
        && screen.modes().cursor_visible
        && row < height
        && col < width)
        .then_some((col, row))
}

/// One line as spans of equal colour, padded or cut to `width` columns.
fn line_row(line: &Line, colors: &Colors, width: usize) -> Row {
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0;
    for cell in &line.cells {
        if cell.is_continuation() {
            continue;
        }
        let cell_width = usize::from(cell.width.max(1));
        if used + cell_width > width {
            break;
        }
        let (fg, bg) = colors.cell(cell);
        let mut text = String::new();
        text.push(cell.c);
        if let Some(combining) = &cell.combining {
            text.push_str(combining);
        }
        match spans.last_mut() {
            Some(last) if last.fg == fg && last.bg == bg => last.text.push_str(&text),
            _ => spans.push(Span {
                text,
                fg,
                bg,
                italic: false,
            }),
        }
        used += cell_width;
    }
    if used < width {
        spans.push(Span {
            text: " ".repeat(width - used),
            fg: colors.bg,
            bg: colors.bg,
            italic: false,
        });
    }
    Row { spans }
}

/// The shell to start, from the user's settings.
///
/// `terminal.integrated.defaultProfile.<platform>` names a profile in
/// `terminal.integrated.profiles.<platform>`, whose `path` is the program and
/// `args` its arguments. Without one, `$SHELL` on Unix and `%COMSPEC%` on
/// Windows is used, then `/bin/sh` or `cmd.exe`.
///
/// Only the user's settings are read. A workspace or a remote machine's
/// settings could otherwise name a program that runs the moment the terminal
/// is opened, so their values are ignored and reported, as for language
/// servers.
pub fn shell(settings: &Settings, problems: &mut Vec<String>) -> deco_term::pty::Command {
    let platform = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    };
    let profiles_key = format!("terminal.integrated.profiles.{platform}");
    let default_key = format!("terminal.integrated.defaultProfile.{platform}");
    for key in [&profiles_key, &default_key] {
        for scope in [Scope::Remote, Scope::Workspace, Scope::Folder] {
            if settings
                .layer(scope)
                .is_some_and(|layer| layer.contains_key(key.as_str()))
            {
                problems.push(format!(
                    "{key} is only read from user settings; the {scope:?} value is ignored"
                ));
            }
        }
    }
    let user = settings.layer(Scope::User);
    let profile = user
        .and_then(|layer| layer.get(&default_key))
        .and_then(|name| name.as_str())
        .and_then(|name| {
            user.and_then(|layer| layer.get(&profiles_key))
                .and_then(|profiles| profiles.get(name))
        });
    if let Some(profile) = profile {
        // `path` may be a list of candidates; the first one that exists wins.
        let path = match profile.get("path") {
            Some(serde_json::Value::String(path)) => Some(path.clone()),
            Some(serde_json::Value::Array(paths)) => paths
                .iter()
                .filter_map(|path| path.as_str())
                .find(|path| Path::new(path).exists() || !path.contains(['/', '\\']))
                .map(str::to_owned),
            _ => None,
        };
        if let Some(path) = path {
            let args = match profile.get("args") {
                Some(serde_json::Value::Array(args)) => args
                    .iter()
                    .filter_map(|arg| arg.as_str())
                    .map(OsString::from)
                    .collect(),
                _ => Vec::new(),
            };
            let env = match profile.get("env") {
                Some(serde_json::Value::Object(env)) => env
                    .iter()
                    .filter_map(|(name, value)| {
                        Some((OsString::from(name), OsString::from(value.as_str()?)))
                    })
                    .collect(),
                _ => Vec::new(),
            };
            return deco_term::pty::Command {
                program: path.into(),
                args,
                cwd: None,
                env,
            };
        }
        problems.push(format!("{default_key}: the profile has no usable `path`"));
    }
    let program = if cfg!(windows) {
        std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into())
    } else {
        std::env::var_os("SHELL")
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".into())
    };
    deco_term::pty::Command {
        program,
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
    }
}

/// The variables every shell in the terminal gets, as VS Code sets them.
pub fn environment() -> Vec<(OsString, OsString)> {
    vec![
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        ("TERM_PROGRAM".into(), "deco".into()),
        (
            "TERM_PROGRAM_VERSION".into(),
            env!("CARGO_PKG_VERSION").into(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Colors {
        Colors::from_theme(
            &deco_editor::Session::with_defaults().theme,
            &Settings::with_defaults(),
            Rgba::WHITE,
            Rgba::BLACK,
        )
    }

    #[test]
    fn indexed_colours_cover_the_cube_and_the_grey_ramp() {
        let colors = colors();
        let theme = deco_editor::Session::with_defaults().theme;
        let red = theme.color("terminal.ansiRed").unwrap_or(hex(DARK_ANSI[1]));
        assert_eq!(colors.indexed(1), red);
        assert_eq!(colors.indexed(16), Rgba::rgb(0, 0, 0));
        assert_eq!(colors.indexed(196), Rgba::rgb(255, 0, 0));
        assert_eq!(colors.indexed(231), Rgba::rgb(255, 255, 255));
        assert_eq!(colors.indexed(232), Rgba::rgb(8, 8, 8));
        assert_eq!(colors.indexed(255), Rgba::rgb(238, 238, 238));
    }

    #[test]
    fn a_screen_becomes_rows_of_the_panel_s_width() {
        let mut session = deco_editor::Session::with_defaults();
        session.terminal_started(1, 6, 2);
        session.terminal_output(1, "a\x1b[31m日b".as_bytes());
        let panel = session.terminal().expect("started");
        let rows = rows(panel, &colors(), 8, 2);
        let cursor = cursor(panel, 8, 2);
        let text: String = rows[0]
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect();
        assert_eq!(text, "a日b    ", "the wide character takes two columns");
        assert_eq!(rows[0].spans[1].fg, colors().indexed(1));
        assert_eq!(cursor, Some((4, 0)));
    }

    #[test]
    fn the_default_shell_comes_from_the_environment_and_workspace_profiles_are_ignored() {
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                Scope::Workspace,
                r#"{ "terminal.integrated.defaultProfile.linux": "evil",
                     "terminal.integrated.profiles.linux": { "evil": { "path": "/tmp/evil" } } }"#,
            )
            .unwrap();
        let mut problems = Vec::new();
        let command = shell(&settings, &mut problems);
        assert_ne!(command.program, OsString::from("/tmp/evil"));
        if cfg!(target_os = "linux") {
            assert_eq!(problems.len(), 2, "{problems:?}");
        }

        settings
            .load_layer(
                Scope::User,
                r#"{ "terminal.integrated.defaultProfile.linux": "b",
                     "terminal.integrated.defaultProfile.osx": "b",
                     "terminal.integrated.defaultProfile.windows": "b",
                     "terminal.integrated.profiles.linux": { "b": { "path": "bash", "args": ["-l"] } },
                     "terminal.integrated.profiles.osx": { "b": { "path": "bash", "args": ["-l"] } },
                     "terminal.integrated.profiles.windows": { "b": { "path": "bash", "args": ["-l"] } } }"#,
            )
            .unwrap();
        let command = shell(&settings, &mut Vec::new());
        assert_eq!(command.program, OsString::from("bash"));
        assert_eq!(command.args, [OsString::from("-l")]);
    }
}
