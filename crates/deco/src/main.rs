//! The deco editor.

mod config;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use deco_config::paths::{Env, Layout};
use deco_editor::Session;
use deco_keymap::binding::Platform;

/// Which frontend to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Frontend {
    /// The terminal UI.
    Tui,
    /// The GPU-accelerated window.
    Gui,
}

/// A lightweight, VS Code-compatible editor.
#[derive(Debug, Parser)]
#[command(name = "deco", version, about, long_about = None)]
struct Cli {
    /// File to open.
    file: Option<PathBuf>,

    /// Which frontend to use. Defaults to the terminal UI.
    #[arg(long, value_enum, default_value_t = Frontend::Tui)]
    frontend: Frontend,

    /// Print the resolved configuration and exit.
    #[arg(long)]
    print_config: bool,

    /// Ignore the user's settings.json and keybindings.json.
    ///
    /// The escape hatch for a configuration that stops the editor working.
    #[arg(long)]
    clean: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let env = Env::from_process();
    let workspace = cli.file.as_deref().and_then(config::workspace_root_for);
    let loaded = if cli.clean {
        config::LoadedConfig {
            settings: deco_config::Settings::with_defaults(),
            keybindings: None,
            problems: Vec::new(),
        }
    } else {
        config::load(&env, Layout::host(), workspace.as_deref())
    };

    let mut session = Session::new(
        loaded.settings,
        loaded.keybindings.as_deref(),
        Platform::host(),
    );
    session.problems.extend(loaded.problems);

    if let Some(path) = &cli.file {
        // A path that does not exist yet is a new file, not an error — that is
        // how every editor is used to create one.
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()))
            }
        };
        session.open(path.clone(), &text);
    }

    if cli.print_config {
        print_config(&session);
        return Ok(());
    }

    // Configuration problems would scroll past unseen once the alternate screen
    // opens, so they are reported before the frontend starts.
    for problem in &session.problems {
        eprintln!("deco: {problem}");
    }

    match cli.frontend {
        Frontend::Tui => deco_tui::run(&mut session, cli.file),
        Frontend::Gui => run_gui(&mut session, cli.file),
    }
}

#[cfg(feature = "gui")]
fn run_gui(session: &mut Session, path: Option<PathBuf>) -> Result<()> {
    deco_gui::run(session, path)
}

#[cfg(not(feature = "gui"))]
fn run_gui(_session: &mut Session, _path: Option<PathBuf>) -> Result<()> {
    anyhow::bail!(
        "this build has no GPU frontend. Rebuild with `cargo build --features gui`, \
         or run `deco --frontend tui`."
    )
}

/// Prints what the editor resolved, which is the quickest way to answer "why is
/// my setting not taking effect".
fn print_config(session: &Session) {
    let settings = &session.document.settings;
    println!("theme               {}", session.theme.name);
    println!(
        "language            {}",
        session.document.language().unwrap_or("plain text")
    );
    println!("editor.tabSize      {}", settings.tab_size);
    println!("editor.insertSpaces {}", settings.insert_spaces);
    println!("editor.wordWrap     {:?}", settings.word_wrap);
    println!("editor.fontFamily   {}", settings.font_family);
    println!("editor.fontSize     {}", settings.font_size);
    println!("files.eol           {:?}", settings.eol);
    println!("keybindings         {} bindings", session.keymap.len());
    for problem in &session.problems {
        println!("problem             {problem}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_bare_invocation_opens_the_terminal_frontend() {
        let cli = Cli::parse_from(["deco"]);
        assert_eq!(cli.frontend, Frontend::Tui);
        assert!(cli.file.is_none());
        assert!(!cli.clean);
    }

    #[test]
    fn a_file_argument_is_parsed() {
        let cli = Cli::parse_from(["deco", "src/main.rs"]);
        assert_eq!(cli.file, Some(PathBuf::from("src/main.rs")));
    }

    #[test]
    fn the_frontend_can_be_chosen() {
        assert_eq!(
            Cli::parse_from(["deco", "--frontend", "gui"]).frontend,
            Frontend::Gui
        );
        assert_eq!(
            Cli::parse_from(["deco", "--frontend", "tui"]).frontend,
            Frontend::Tui
        );
    }

    #[test]
    fn an_unknown_frontend_is_rejected() {
        assert!(Cli::try_parse_from(["deco", "--frontend", "holograph"]).is_err());
    }

    #[test]
    fn clean_and_print_config_are_flags() {
        let cli = Cli::parse_from(["deco", "--clean", "--print-config"]);
        assert!(cli.clean);
        assert!(cli.print_config);
    }
}
