//! Turning a command line and a configuration directory into a session.
//!
//! This is the work between `main` returning from [`crate::cli::parse`] and a
//! frontend drawing its first frame. It is a module rather than a stretch of
//! `main` so that it can be run twice: once by the binary against the machine it
//! is installed on, and once by a test against a home directory it wrote itself.
//!
//! Nothing here reads the process environment. The three facts that would
//! otherwise be read from it — where home is, which platform's configuration
//! layout to use, and which platform's keybindings win — are [`Boot`], and
//! `main` is the only caller that fills it in from the process.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use deco_config::paths::{Env, Layout};
use deco_editor::Session;
use deco_keymap::binding::Platform;

use crate::cli::Cli;

/// The machine deco is starting on.
///
/// Data rather than calls, for the reason [`deco_config::paths::Env`] is: the
/// rules that depend on these are then the same rules under test as in
/// production, and a test does not have to mutate the process environment — which
/// is shared by every test thread and therefore cannot be done safely — to
/// exercise them.
#[derive(Debug, Clone)]
pub struct Boot {
    /// Home, `$XDG_CONFIG_HOME` and `%APPDATA%`.
    pub env: Env,
    /// Which platform's configuration directory layout applies.
    pub layout: Layout,
    /// Which platform's keybindings win, for `key` versus `mac` and for the
    /// `isMac`-style context keys.
    pub platform: Platform,
    /// What relative paths on the command line are taken against. `None` when the
    /// working directory cannot be read, in which case a relative path is left as
    /// it was typed.
    pub cwd: Option<PathBuf>,
}

impl Boot {
    /// What the process says about the machine it is running on.
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
/// No file is opened here — that is [`open_local`], which is separate because a
/// remote session fetches its files rather than reading them, and both then agree
/// about the settings that were already resolved.
///
/// Configuration failures land in `session.problems` rather than stopping
/// startup: a `settings.json` with a typo in it is a reason to say so, not a
/// reason to refuse to open the file somebody asked for.
pub fn session(cli: &Cli, boot: &Boot, remote_settings: Option<&str>) -> Session {
    // The first file names the workspace; a mixed invocation has to pick one,
    // and the first is the one the user led with.
    //
    // Resolved before the walk, not after. `workspace_root_for` climbs the path
    // asking the filesystem whether each directory holds a `.git` or a `.vscode`,
    // and a relative path makes every one of those questions a question about the
    // process's working directory — so the walk was reaching the right answer
    // only for as long as that directory and the one the path is relative to were
    // the same. They are the same for `deco src/main.rs` in a shell, and they are
    // not the same for anything that resolves paths itself, which is why this was
    // invisible until a test tried to run deco against a workspace of its own.
    let workspace = cli
        .files
        .first()
        .map(|path| absolute(path, boot.cwd.as_deref()))
        .as_deref()
        .and_then(crate::config::workspace_root_for);
    let loaded = if cli.clean {
        // `--clean` means no configuration, and the remote's is configuration:
        // a flag for "start with nothing" that still adopted another machine's
        // settings would not be the flag it says it is.
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
/// A path that does not exist yet is a new file, not an error — that is how every
/// editor is used to create one. Anything else that goes wrong reading a file
/// *is* an error, because an editor that silently opens an empty buffer over a
/// file it could not read is one keystroke away from truncating it.
pub fn open_local(session: &mut Session, files: &[PathBuf], boot: &Boot) -> Result<()> {
    for path in files {
        // Absolute before the session sees it. Every other way a file gets opened
        // — quick open, `ctrl+o`, a search result, a jump to a definition —
        // resolves first, so a relative path from here was the one spelling that
        // never compared equal to any other: `deco src/main.rs` and then picking
        // the same file from `ctrl+p` opened it twice, in two buffers with two
        // undo histories.
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
/// is what the user led with, so it is the one shown.
pub fn focus_first(session: &mut Session, count: usize) {
    for _ in 1..count {
        session.run("workbench.action.previousEditor", None, 0);
    }
}

/// `path` against the working directory, when it is not already absolute.
///
/// Lexical: a file that does not exist yet has to resolve too, so this cannot be
/// `fs::canonicalize`. A working directory that cannot be read leaves the path as
/// typed, which is what deco did before it resolved anything.
pub fn absolute(path: &Path, cwd: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match cwd {
        Some(cwd) => cwd.join(path),
        None => path.to_path_buf(),
    }
}

/// What `--print-config` prints: what the editor resolved, which is the quickest
/// way to answer "why is my setting not taking effect".
///
/// Returned rather than printed so that the answer can be asserted. Every value
/// that came out of a settings file is made printable first, for the same reason
/// the problem list is: `--print-config` in a cloned repository prints that
/// repository's text to the terminal it was run from, and a terminal interprets
/// what it is written.
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
    // How extensions would be run. Printed because refusing to degrade silently
    // only means anything if the answer is available somewhere, and this is where
    // someone looks for it.
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
