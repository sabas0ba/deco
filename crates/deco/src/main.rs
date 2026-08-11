//! The deco editor.

mod cli;
mod config;

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cli::{Frontend, Outcome};
use deco_config::paths::{Env, Layout};
use deco_editor::Session;
use deco_keymap::binding::Platform;

fn main() -> Result<()> {
    // Skipping the program name: `cli::parse` takes arguments only, so tests
    // can call it with the exact list a user would type.
    let cli = match cli::parse(std::env::args().skip(1)) {
        Ok(Outcome::Run(cli)) => *cli,
        Ok(Outcome::Help) => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Ok(Outcome::Version) => {
            println!("deco {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(error) => {
            // Exit 2 rather than 1: a usage error is not the editor failing,
            // and shell scripts distinguish the two.
            eprintln!("deco: {error}\n\nTry `deco --help`.");
            std::process::exit(2);
        }
    };

    let env = Env::from_process();
    // The first file names the workspace; a mixed invocation has to pick one,
    // and the first is the one the user led with.
    let workspace = cli
        .files
        .first()
        .map(PathBuf::as_path)
        .and_then(config::workspace_root_for);
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

    for path in &cli.files {
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
    // Opening focuses each file in turn, so the last one ends up active. The
    // first is what the user led with, so it is the one shown.
    if cli.files.len() > 1 {
        for _ in 0..cli.files.len() - 1 {
            session.run("workbench.action.previousEditor", None, 0);
        }
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
        Frontend::Tui => deco_tui::run(&mut session, cli.files.first().cloned()),
        Frontend::Gui => run_gui(&mut session, cli.files.first().cloned()),
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
