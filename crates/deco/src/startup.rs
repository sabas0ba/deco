//! Turning a command line and a configuration directory into a session.
//!
//! This is the work between `main` returning from [`crate::cli::parse`] and a
//! frontend drawing its first frame. It is a module rather than part of `main`
//! so that it has two callers: the binary, against the machine it is installed
//! on, and tests, against a home directory they created.
//!
//! Nothing here reads the process environment. The three values that would
//! otherwise come from it (the home directory, the platform's configuration
//! layout, and the platform whose keybindings apply) are held in [`Boot`].
//! `main` is the only caller that fills it in from the process.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use deco_config::paths::{Env, Layout};
use deco_editor::Session;
use deco_keymap::binding::Platform;

use crate::cli::Cli;

/// The machine deco is starting on.
///
/// Stored as data rather than read by calls, for the same reason as
/// [`deco_config::paths::Env`]. The rules that depend on these values are then
/// the same in tests and in production. A test does not have to modify the
/// process environment, which every test thread shares and which therefore
/// cannot be modified safely.
#[derive(Debug, Clone)]
pub struct Boot {
    /// Home, `$XDG_CONFIG_HOME` and `%APPDATA%`.
    pub env: Env,
    /// Which platform's configuration directory layout applies.
    pub layout: Layout,
    /// Which platform's keybindings apply, for `key` versus `mac` and for the
    /// `isMac`-style context keys.
    pub platform: Platform,
    /// The directory that relative paths on the command line are resolved
    /// against. `None` when the working directory cannot be read, in which case a
    /// relative path is left as it was typed.
    pub cwd: Option<PathBuf>,
}

impl Boot {
    /// Reads these values from the current process.
    pub fn from_process() -> Self {
        Self {
            env: Env::from_process(),
            layout: Layout::host(),
            platform: Platform::host(),
            cwd: std::env::current_dir().ok(),
        }
    }
}

/// The session a command line asks for, with every configuration layer applied.
///
/// No file is opened here; [`open_local`] does that. It is separate because a
/// remote session fetches its files instead of reading them. Both paths use the
/// settings already resolved here.
///
/// Configuration failures are added to `session.problems` rather than stopping
/// startup. A typo in `settings.json` should be reported, but it should not
/// prevent the requested file from opening.
pub fn session(cli: &Cli, boot: &Boot, remote_settings: Option<&str>) -> Session {
    // The first file determines the workspace. When files come from different
    // places one must be chosen, and the first is the one the user gave first.
    //
    // The path is made absolute before the walk. `workspace_root_for` climbs the
    // path and checks whether each directory contains a `.git` or a `.vscode`.
    // With a relative path, each check is relative to the process's working
    // directory, so the result was correct only when that directory matched the
    // one the path was relative to. They match for `deco src/main.rs` in a
    // shell, but not for callers that resolve paths themselves. The problem was
    // found when a test ran deco against its own workspace.
    let workspace = cli
        .files
        .first()
        .map(|path| absolute(path, boot.cwd.as_deref()))
        .as_deref()
        .and_then(crate::config::workspace_root_for);
    let loaded = if cli.clean {
        // `--clean` means no configuration, and that includes the remote's
        // settings.
        crate::config::LoadedConfig {
            settings: deco_config::Settings::with_defaults(),
            keybindings: None,
            problems: Vec::new(),
        }
    } else {
        crate::config::load(
            &boot.env,
            boot.layout,
            workspace.as_deref(),
            remote_settings,
        )
    };

    let mut session = Session::new(
        loaded.settings,
        loaded.keybindings.as_deref(),
        boot.platform,
    );
    session.problems.extend(loaded.problems);
    session
}

/// Opens each file from the command line, reading it from this machine.
///
/// A path that does not exist yet is a new file, not an error; editors are
/// commonly used this way to create files. Any other read failure *is* an
/// error. If the editor opened an empty buffer for a file it could not read,
/// saving that buffer would truncate the file.
pub fn open_local(session: &mut Session, files: &[PathBuf], boot: &Boot) -> Result<()> {
    for path in files {
        // Make the path absolute before the session sees it. Every other way of
        // opening a file (quick open, `ctrl+o`, a search result, a jump to a
        // definition) resolves the path first. A relative path from here never
        // compared equal to those, so `deco src/main.rs` followed by picking the
        // same file from `ctrl+p` opened it twice, in two buffers with two undo
        // histories.
        let path = absolute(path, boot.cwd.as_deref());
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()))
            }
        };
        session.open(path, &text);
    }
    Ok(())
}

/// Leaves the first of `count` freshly opened files showing.
///
/// Opening focuses each file in turn, so the last one ends up active. The first
/// file on the command line is the one shown.
pub fn focus_first(session: &mut Session, count: usize) {
    for _ in 1..count {
        session.run("workbench.action.previousEditor", None, 0);
    }
}

/// `path` against the working directory, when it is not already absolute.
///
/// Resolution is lexical because a file that does not exist yet must also
/// resolve, so `fs::canonicalize` cannot be used. If the working directory
/// cannot be read, the path is left as typed, matching deco's behaviour before
/// paths were resolved.
pub fn absolute(path: &Path, cwd: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match cwd {
        Some(cwd) => cwd.join(path),
        None => path.to_path_buf(),
    }
}

/// What `--print-config` prints: the values the editor resolved. This is the
/// quickest way to find out why a setting is not taking effect.
///
/// Returned rather than printed so that the output can be asserted. Every value
/// read from a settings file is sanitised first, as the problem list is:
/// `--print-config` in a cloned repository prints that repository's text to the
/// terminal, and the terminal interprets control sequences in it.
pub fn config_report(session: &Session) -> String {
    use std::fmt::Write as _;

    let settings = &session.document.settings;
    let shown = deco_tui::sanitise;
    let mut out = String::new();
    let _ = writeln!(out, "theme               {}", shown(&session.theme.name));
    let _ = writeln!(
        out,
        "language            {}",
        shown(session.document.language().unwrap_or("plain text"))
    );
    let _ = writeln!(out, "editor.tabSize      {}", settings.tab_size);
    let _ = writeln!(out, "editor.insertSpaces {}", settings.insert_spaces);
    let _ = writeln!(out, "editor.wordWrap     {:?}", settings.word_wrap);
    let _ = writeln!(out, "editor.fontFamily   {}", shown(&settings.font_family));
    let _ = writeln!(out, "editor.fontSize     {}", settings.font_size);
    let _ = writeln!(out, "files.eol           {:?}", settings.eol);
    let _ = writeln!(out, "keybindings         {} bindings", session.keymap.len());
    // How extensions would be run. deco reports rather than hides a weaker
    // sandbox, so the current mode is printed here, where users look for it.
    let _ = writeln!(
        out,
        "extension sandbox   {}",
        shown(&deco_tui::extensions::sandbox_summary(&session.settings))
    );
    for problem in &session.problems {
        let _ = writeln!(out, "problem             {}", shown(problem));
    }
    out
}
