//! The deco editor.

mod cli;
mod config;

use std::path::{Path, PathBuf};

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

    // Before any configuration is read: a server has no editor, no theme and no
    // keybindings, and reading a `settings.json` on the remote to decide how to
    // answer `fs.read` would be an authority nobody asked it to have.
    if cli.server {
        return serve(cli.workspace.as_deref());
    }

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

    // A remote session: the files are on the other machine, so they are fetched
    // rather than read, and the same connection is what saves them later.
    let mut remote = match cli.remote.as_deref() {
        Some(authority) => Some(connect(authority, &cli, &mut session)?),
        None => None,
    };
    if let Some(client) = remote.as_mut() {
        for path in &cli.files {
            let asked = path.display().to_string();
            let text = client
                .read(&asked)
                .with_context(|| format!("could not open {asked} on the remote"))?;
            session.open(path.clone(), &text);
        }
    }

    for path in cli.files.iter().filter(|_| remote.is_none()) {
        // Absolute before the session sees it. Every other way a file gets opened
        // — quick open, `ctrl+o`, a search result, a jump to a definition —
        // resolves first, so a relative path from here was the one spelling that
        // never compared equal to any other: `deco src/main.rs` and then picking
        // the same file from `ctrl+p` opened it twice, in two buffers with two
        // undo histories.
        let path = absolute(path);
        // A path that does not exist yet is a new file, not an error — that is
        // how every editor is used to create one.
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()))
            }
        };
        session.open(path, &text);
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
    //
    // Made printable first, and for a sharper reason than inside the editor: a
    // problem message quotes what a settings file said — a theme name, a broken
    // keybinding — and a cloned repository's `.vscode/settings.json` is somebody
    // else's text. Written raw it would reach the real terminal, with no alternate
    // screen between it and the shell, where `\x1b]52;c;…` sets the clipboard.
    for problem in &session.problems {
        eprintln!("deco: {}", deco_tui::sanitise(problem));
    }

    match cli.frontend {
        Frontend::Tui => deco_tui::run_with(&mut session, cli.files.first().cloned(), remote),
        Frontend::Gui => run_gui(&mut session),
    }
}

/// `path` against the working directory, when it is not already absolute.
///
/// Lexical: a file that does not exist yet has to resolve too, so this cannot be
/// `fs::canonicalize`. A working directory that cannot be read leaves the path as
/// typed, which is what deco did before it resolved anything.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(feature = "gui")]
fn run_gui(session: &mut Session) -> Result<()> {
    deco_gui::run(session)
}

#[cfg(not(feature = "gui"))]
fn run_gui(_session: &mut Session) -> Result<()> {
    anyhow::bail!(
        "this build has no GPU frontend. Rebuild with `cargo build --features gui`, \
         or run `deco --frontend tui`."
    )
}

/// Connects to a remote and starts a server on it.
///
/// The workspace is `--workspace` if given, and otherwise wherever the transport
/// lands — for SSH that is the account's home directory, which is what
/// `ssh host deco --server --stdio` would serve. The server refuses everything
/// outside it, so this is also the decision about what this session can reach.
fn connect(
    authority: &str,
    cli: &crate::cli::Cli,
    session: &mut Session,
) -> Result<deco_remote::Client> {
    let authority = deco_remote::Authority::parse(authority)
        .with_context(|| format!("`{authority}` is not a remote deco understands"))?;
    let workspace = cli
        .workspace
        .as_deref()
        .map(|path| path.display().to_string());
    let command = deco_remote::command_for(
        &authority,
        &deco_remote::server_command("deco", workspace.as_deref()),
        deco_remote::TransportOptions::default(),
    )
    .context("that remote cannot be reached")?;

    let mut client = deco_remote::Client::start(&command)
        .with_context(|| format!("could not run `{}`", command.program))?;
    let hello = client
        .handshake()
        .context("the remote did not answer as a deco server")?;
    // Worth saying once: it names the machine's own idea of where it is, which is
    // the thing a mistyped `--workspace` gets wrong invisibly.
    session.problems.push(format!(
        "remote session: {} is serving {}",
        command.program, hello.workspace
    ));
    Ok(client)
}

/// Runs as the remote server, speaking the framed protocol over stdin and stdout.
///
/// The workspace defaults to the working directory, which is what an `ssh host
/// deco --server --stdio` with no `--workspace` means: serve where you landed.
/// Nothing outside it can be read or written, whatever is asked — see
/// [`deco_remote::server`].
fn serve(workspace: Option<&Path>) -> Result<()> {
    let root = match workspace {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("no working directory to serve")?,
    };
    let mut server = deco_remote::Server::new(&root)
        .with_context(|| format!("cannot serve {}", root.display()))?;

    // Locked once rather than per frame, and stdout is *only* written by the
    // protocol from here on: a stray `println!` would be read by the client as a
    // header and end the session.
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    deco_remote::server::serve(&mut input, &mut output, &mut server)
        .context("the remote session ended badly")
}

/// Prints what the editor resolved, which is the quickest way to answer "why is
/// my setting not taking effect".
fn print_config(session: &Session) {
    let settings = &session.document.settings;
    // Every value that came out of a settings file is made printable, for the reason
    // the problem list is: `--print-config` in a cloned repository prints that
    // repository's text to the terminal it was run from.
    let shown = deco_tui::sanitise;
    println!("theme               {}", shown(&session.theme.name));
    println!(
        "language            {}",
        shown(session.document.language().unwrap_or("plain text"))
    );
    println!("editor.tabSize      {}", settings.tab_size);
    println!("editor.insertSpaces {}", settings.insert_spaces);
    println!("editor.wordWrap     {:?}", settings.word_wrap);
    println!("editor.fontFamily   {}", shown(&settings.font_family));
    println!("editor.fontSize     {}", settings.font_size);
    println!("files.eol           {:?}", settings.eol);
    println!("keybindings         {} bindings", session.keymap.len());
    // How extensions would be run. Printed because refusing to degrade silently
    // only means anything if the answer is available somewhere, and this is where
    // someone looks for it.
    println!(
        "extension sandbox   {}",
        shown(&deco_tui::extensions::sandbox_summary(&session.settings))
    );
    for problem in &session.problems {
        println!("problem             {}", shown(problem));
    }
}
