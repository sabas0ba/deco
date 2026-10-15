//! The deco editor.
//!
//! Argument parsing, configuration loading and everything else that determines
//! the first frame are in this crate's library, so that tests can run them
//! against a directory they created. This file holds the parts that need a real
//! process: the remote transports, the server mode, and frontend selection.

use std::path::{Path, PathBuf};

use deco_editor::Session;

use anyhow::{Context, Result};

use deco::cli::{self, Frontend, Outcome};
use deco::startup::{self, Boot};

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

    // Checked before any configuration is read. A server has no editor, theme or
    // keybindings, and it must not read a `settings.json` on the remote to decide
    // how to answer `fs.read`.
    if cli.server {
        return serve(cli.workspace.as_deref(), cli.machine_settings.as_deref());
    }
    // Likewise: this process is one end of a socket, not an editor.
    if let Some(target) = cli.forward_to.as_deref() {
        return forward_to(target);
    }
    // And this one is the far end of an editor's terminal.
    if cli.pty {
        return pty_stdio(cli.workspace.as_deref());
    }

    let boot = Boot::from_process();

    // A remote session: the files are on the other machine, so they are fetched
    // rather than read, and the same connection saves them later.
    //
    // The connection is made before the session is created (this was previously
    // done afterwards). The connection provides the remote's machine settings,
    // which are a configuration *layer*. They must be loaded before the theme is
    // resolved and the keymap is built; otherwise the editor would start with
    // one configuration and then switch to another.
    let mut connecting = Vec::new();
    let (mut remote, server_path, remote_settings) = match cli.remote.as_deref() {
        Some(authority) => {
            let (client, path, settings) = connect(authority, &cli, &mut connecting)?;
            (Some(client), Some(path), settings)
        }
        None => (None, None, None),
    };

    let mut session = startup::session(&cli, &boot, remote_settings.as_deref());
    // Added after the configuration's own problems, so the list is in
    // chronological order.
    session.problems.extend(connecting);
    // Kept for as long as the editor runs. Dropping these stops the listeners,
    // so each forward ends when the session ends.
    let _forwards = forwards(&cli, server_path.as_deref(), &mut session)?;
    if let Some(client) = remote.as_mut().map(|remote| &mut remote.client) {
        for path in &cli.files {
            let asked = path.display().to_string();
            let text = client
                .read(&asked)
                .with_context(|| format!("could not open {asked} on the remote"))?;
            session.open(path.clone(), &text);
        }
    }

    if remote.is_none() {
        startup::open_local(&mut session, &cli.files, &boot)?;
    }
    startup::focus_first(&mut session, cli.files.len());

    if cli.print_config {
        print!("{}", startup::config_report(&session));
        return Ok(());
    }

    // Configuration problems would scroll past unseen once the alternate screen
    // opens, so they are reported before the frontend starts.
    //
    // Messages are sanitised first. A problem message quotes content from a
    // settings file, such as a theme name or a broken keybinding, and a cloned
    // repository's `.vscode/settings.json` is untrusted text. Written raw, it
    // would reach the real terminal outside the alternate screen, where a
    // sequence such as `\x1b]52;c;…` sets the clipboard.
    for problem in &session.problems {
        eprintln!("deco: {}", deco_tui::sanitise(problem));
    }

    match cli.frontend {
        Frontend::Tui => deco_tui::run_with(&mut session, cli.files.first().cloned(), remote),
        Frontend::Gui => run_gui(&mut session),
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
/// The workspace is `--workspace` if given, and otherwise the transport's
/// starting directory. For SSH that is the account's home directory, which is
/// what `ssh host deco --server --stdio` would serve. The server rejects every
/// path outside the workspace, so this also determines what the session can
/// access.
fn connect(
    authority: &str,
    cli: &cli::Cli,
    problems: &mut Vec<String>,
) -> Result<(deco_tui::RemoteSession, String, Option<String>)> {
    let authority = deco_remote::Authority::parse(authority)
        .with_context(|| format!("`{authority}` is not a remote deco understands"))?;
    let workspace = cli
        .workspace
        .as_deref()
        .map(|path| path.display().to_string());
    // Multiplexed: the file server opens one connection, but every forwarded
    // connection opens another, and a browser loading one page can open twenty.
    // The socket is placed where only this account can access it; see
    // `TransportOptions::multiplexed`.
    let options = deco_remote::TransportOptions::multiplexed();

    // Only when requested. Without the flag, a missing server is reported as a
    // failure rather than fixed automatically, as described in
    // `deco_remote::install`.
    let server_path = if cli.remote_install {
        let installed = provision(&authority, cli, options.clone())?;
        problems.push(match &installed {
            deco_remote::Installed::AlreadyThere { path, version } => {
                format!("remote session: {version} was already at {path}")
            }
            deco_remote::Installed::Sent {
                path,
                version,
                replaced,
            } => match replaced {
                Some(old) => format!("remote session: replaced {old} at {path} with {version}"),
                None => format!("remote session: installed {version} at {path}"),
            },
        });
        server_path(cli, Some(&installed))
    } else {
        server_path(cli, None)
    };

    let command = deco_remote::command_for(
        &authority,
        &deco_remote::server_command(&server_path, workspace.as_deref()),
        &options,
    )
    .context("that remote cannot be reached")?;

    let mut client = deco_remote::Client::start(&command)
        .with_context(|| format!("could not run `{}`", command.program))?;
    let hello = client.handshake().map_err(|error| {
        // The most common cause of a server that never answers is that the
        // remote has no deco, which one flag fixes. Report that here instead of
        // a bare protocol error.
        let missing =
            matches!(error, deco_remote::ClientError::Closed { .. }) && !cli.remote_install;
        anyhow::Error::new(error).context(if missing {
            no_server_hint(&server_path)
        } else {
            "the remote did not answer as a deco server".to_owned()
        })
    })?;
    // Reported once. It shows the workspace path as the remote resolved it,
    // which makes a mistyped `--workspace` visible.
    problems.push(format!(
        "remote session: {} is serving {}",
        command.program, hello.workspace
    ));

    // The remote's own settings, which become the `remote` layer. They are
    // requested only when the handshake lists the method. An older server would
    // reject the request, and that must stay distinguishable from a machine
    // that has no settings.
    //
    // A failure here is reported as a problem and does not stop startup. An
    // unreadable optional layer should not prevent opening a file, and the
    // message shows that the settings were not applied.
    let remote_settings = if hello.serves("settings.read") {
        match client.machine_settings() {
            Ok((path, Some(text))) => {
                problems.push(format!("remote session: settings from {path}"));
                Some(text)
            }
            Ok((_, None)) => None,
            Err(error) => {
                problems.push(format!(
                    "remote session: could not read the remote's settings: {error}"
                ));
                None
            }
        }
    } else {
        None
    };

    // Git status can walk for a long time and commit hooks are arbitrary
    // programs. They get their own server connection, owned by a worker in the
    // frontend, so neither can hold up file reads or the terminal event loop.
    // Optional for compatibility: an older server still opens the workspace,
    // but says at startup that source control needs an update.
    let scm_methods = ["scm.status", "scm.committed", "scm.apply"];
    let scm = if scm_methods.iter().all(|method| hello.serves(method)) {
        match deco_remote::Client::start(&command) {
            Ok(mut scm) => match scm.handshake() {
                Ok(answer) if scm_methods.iter().all(|method| answer.serves(method)) => Some(scm),
                Ok(_) => {
                    problems.push(
                        "remote session: source control is unavailable; update the remote deco"
                            .to_owned(),
                    );
                    None
                }
                Err(error) => {
                    problems.push(format!(
                        "remote session: could not start source control: {error}"
                    ));
                    None
                }
            },
            Err(error) => {
                problems.push(format!(
                    "remote session: could not start source control: {error}"
                ));
                None
            }
        }
    } else {
        problems.push(
            "remote session: source control is unavailable; update the remote deco".to_owned(),
        );
        None
    };
    // The workspace path as the *remote side* reports it. Every URI sent to a
    // language server on the remote must be built from it, and this machine
    // cannot determine it on its own.
    // The integrated terminal runs the remote's shell over its own transport
    // process. Only offered when the remote says it can, so an older remote
    // gives a message rather than a terminal that fails to start.
    let terminal = if hello.serves(deco_remote::pty::CAPABILITY) {
        deco_remote::command_for(
            &authority,
            &deco_remote::pty::pty_command(&server_path, &hello.workspace),
            &options,
        )
        .ok()
    } else {
        None
    };
    let location = deco_tui::lsp::Location::Remote {
        authority,
        options,
        workspace: PathBuf::from(&hello.workspace),
    };
    Ok((
        deco_tui::RemoteSession {
            client,
            scm,
            location,
            terminal,
        },
        server_path,
        remote_settings,
    ))
}

/// The path of deco on the remote, after any install has run.
///
/// A single function because the file server and every forward use it and
/// must agree. An install result takes precedence over the flag, because the
/// install resolved `$HOME` on the remote into a path.
fn server_path(cli: &cli::Cli, installed: Option<&deco_remote::Installed>) -> String {
    match installed {
        Some(installed) => installed.path().to_owned(),
        // Bare `deco`, found on the remote's PATH, works for a normal
        // installation.
        None => cli
            .remote_server_path
            .clone()
            .unwrap_or_else(|| "deco".to_owned()),
    }
}

/// Starts every `--forward` and reports each one.
///
/// Each forward needs the remote's deco, the same binary the file server runs,
/// so `--remote-server-path` and `--remote-install` also decide its location
/// here. A forward without a remote has no machine to tunnel to.
fn forwards(
    cli: &cli::Cli,
    server_path: Option<&str>,
    session: &mut Session,
) -> Result<Vec<deco_remote::Forward>> {
    let options = deco_remote::TransportOptions::multiplexed();
    if cli.forwards.is_empty() {
        return Ok(Vec::new());
    }
    // `server_path` comes from the connection instead of being computed again
    // here. When it was computed twice, the file server used the installed deco
    // while the forwards looked for one on the remote's PATH, where
    // `--remote-install` had already found none.
    let (Some(authority), Some(server_path)) = (cli.remote.as_deref(), server_path) else {
        anyhow::bail!("`--forward` needs `--remote`: there is no other machine to reach a port on");
    };
    let authority = deco_remote::Authority::parse(authority)
        .with_context(|| format!("`{authority}` is not a remote deco understands"))?;

    let mut started = Vec::new();
    for spec in &cli.forwards {
        let command = deco_remote::command_for(
            &authority,
            &deco_remote::forward::forward_command(server_path, spec.remote),
            &options,
        )
        .context("that remote cannot be reached")?;
        // Started immediately so that a port already in use is reported now,
        // instead of producing a forward that never works.
        let forward = deco_remote::Forward::start(command, *spec)
            .with_context(|| format!("could not forward {spec}"))?;
        session
            .problems
            .push(format!("remote session: forwarding {spec}"));
        started.push(forward);
    }
    Ok(started)
}

/// The remote half of a forward: connects a socket to stdin and stdout.
///
/// Rejects any address other than loopback, so this cannot be used to reach
/// the remote's network; see [`deco_remote::forward`].
fn forward_to(target: &str) -> Result<()> {
    use deco_remote::forward::pipe;
    use std::net::{Shutdown, TcpStream};

    let address = deco_remote::forward::resolve_loopback(target).map_err(anyhow::Error::msg)?;
    let stream = TcpStream::connect(address)
        .with_context(|| format!("nothing is listening on {address} here"))?;
    let mut to_service = stream.try_clone().context("could not split the socket")?;
    let mut from_service = stream;

    let upstream = std::thread::spawn(move || {
        let _ = pipe(&mut std::io::stdin().lock(), &mut to_service);
        // Half-closed rather than closed: the service may still send data after
        // this side has finished sending.
        let _ = to_service.shutdown(Shutdown::Write);
    });
    // `pipe` rather than `io::copy` because this side writes to a line-buffered
    // stdout, and socket data rarely contains a newline.
    let mut output = std::io::stdout().lock();
    pipe(&mut from_service, &mut output).context("the forwarded connection failed")?;
    // Not joined: if the service closed first, the thread above is blocked
    // reading stdin, which only the transport can close. Waiting for it would
    // keep an idle connection open.
    drop(upstream);
    Ok(())
}

/// The remote half of the integrated terminal: a shell in a pseudo-terminal
/// here, carried as `deco_remote::pty` frames over stdin and stdout.
///
/// The shell is this machine's default (`$SHELL`, or `/bin/sh`), started in
/// `workspace`. No settings are read: the editor's terminal profiles name
/// programs on the editor's machine.
fn pty_stdio(workspace: Option<&std::path::Path>) -> Result<()> {
    use deco_remote::pty::{read_frame, write_frame, Frame};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    // The editor sends the size first, because the shell must start at the
    // right size: a program that draws the screen at once reads it at start.
    let mut input = std::io::stdin().lock();
    let size = match read_frame(&mut input).context("reading the terminal's size")? {
        Some(Frame::Resize { cols, rows }) => deco_term::pty::Size { cols, rows },
        _ => anyhow::bail!("the editor did not send the terminal's size first"),
    };
    drop(input);

    let mut command =
        deco_tui::terminal::shell(&deco_config::Settings::with_defaults(), &mut Vec::new());
    command.cwd = workspace
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .filter(|directory| directory.is_dir());
    command.env = deco_tui::terminal::environment();
    let mut pty = deco_term::pty::Pty::spawn(&command, size)
        .with_context(|| format!("could not start {}", command.program.to_string_lossy()))?;

    // Keys and sizes from the editor, on a thread because reading stdin
    // blocks. The pty itself stays on this thread, which owns it.
    let (sender, frames) = mpsc::channel();
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        while let Ok(Some(frame)) = read_frame(&mut input) {
            if sender.send(frame).is_err() {
                break;
            }
        }
    });

    let mut output = std::io::stdout().lock();
    let mut quiet_since: Option<Instant> = None;
    loop {
        loop {
            match frames.try_recv() {
                Ok(Frame::Data(bytes)) => {
                    let _ = pty.write(&bytes);
                }
                Ok(Frame::Resize { cols, rows }) => {
                    let _ = pty.resize(deco_term::pty::Size { cols, rows });
                }
                Ok(Frame::Exit(_)) | Err(mpsc::TryRecvError::Empty) => break,
                // The editor closed the terminal or lost the connection:
                // the shell is hung up with it, by dropping the pty.
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        let read = pty.read();
        if read.bytes.is_empty() {
            quiet_since.get_or_insert_with(Instant::now);
        } else {
            quiet_since = None;
            write_frame(&mut output, &Frame::Data(read.bytes)).context("writing to the editor")?;
        }
        // Finished as the editor's own terminal is: when the output ends,
        // or once the shell has exited and the output has been quiet for a
        // moment, which the Windows pseudo console needs.
        let code = pty.exit_code();
        let quiet = quiet_since.is_some_and(|since| since.elapsed() >= Duration::from_millis(500));
        if read.ended || (code.is_some() && quiet) {
            let mut code = code;
            for _ in 0..20 {
                if code.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
                code = pty.exit_code();
            }
            write_frame(&mut output, &Frame::Exit(code)).context("writing to the editor")?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The message shown when the remote does not answer.
///
/// A separate function so that a test can check it. Users are likely to see
/// this message, and it names the flags that fix the problem.
fn no_server_hint(server_path: &str) -> String {
    format!(
        "the remote did not answer as a deco server. If `{server_path}` is not there, \
         `--remote-install` sends this one, or `--remote-server-path` points at an \
         existing install"
    )
}

/// Puts this machine's deco on the remote, if `--remote-install` asked for it.
///
/// The version sent is this binary's own, and the binary sent is this process's
/// own file. Installing a *different* deco from the one running would make the
/// remote version impossible to determine.
fn provision(
    authority: &deco_remote::Authority,
    cli: &cli::Cli,
    options: deco_remote::TransportOptions,
) -> Result<deco_remote::Installed> {
    let binary = std::env::current_exe().context("cannot find this deco to send it")?;
    let mut runner = deco_remote::TransportRunner::new(authority.clone(), options);
    let mut curl = deco_remote::fetch::Curl;
    // Next to this binary rather than in the system temporary directory, which
    // every account can write to on a shared machine. If another account could
    // replace the file between the check and the upload, the check would be
    // useless.
    let downloads = binary
        .parent()
        .unwrap_or(Path::new("."))
        .join(".deco-download");
    let for_other = if cli.remote_install_download {
        deco_remote::install::ForOther::Download {
            fetcher: &mut curl,
            into: &downloads,
        }
    } else {
        deco_remote::install::ForOther::Refuse
    };
    let installed = deco_remote::install::ensure(
        &mut runner,
        cli.remote_server_path.as_deref(),
        &binary,
        env!("CARGO_PKG_VERSION"),
        for_other,
    )
    .context("could not put deco on the remote");
    // In every case the downloaded copy has been sent or rejected and is no
    // longer needed. Otherwise an unversioned executable would remain in an
    // unmonitored directory.
    let _ = std::fs::remove_dir_all(&downloads);
    installed
}

/// Runs as the remote server, speaking the framed protocol over stdin and stdout.
///
/// The workspace defaults to the working directory, which is what
/// `ssh host deco --server --stdio` with no `--workspace` serves. Nothing
/// outside it can be read or written, regardless of the request; see
/// [`deco_remote::server`].
fn serve(workspace: Option<&Path>, machine_settings: Option<&Path>) -> Result<()> {
    let root = match workspace {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("no working directory to serve")?,
    };
    let mut server = deco_remote::Server::new(&root)
        .with_context(|| format!("cannot serve {}", root.display()))?;
    // Only when specified. Otherwise the server locates this machine's own file
    // the same way the editor does.
    if let Some(path) = machine_settings {
        server = server.serving_machine_settings(Some(path.to_path_buf()));
    }

    // Locked once rather than per frame. From here on, *only* the protocol
    // writes to stdout: a stray `println!` would be read by the client as a
    // header and end the session.
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    deco_remote::server::serve(&mut input, &mut output, &mut server)
        .context("the remote session ended badly")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_install_decides_where_deco_is_for_the_forwards_as_well() {
        // Regression test: the file server used the path an install resolved,
        // while every forward computed it again and got `deco`. As a result,
        // `--remote-install --forward 3000` opened files through the installed
        // binary but tunnelled to one that was not on the remote's PATH.
        let installed = deco_remote::Installed::Sent {
            path: "/home/u/.deco/bin/deco".to_owned(),
            version: "deco 0.1.0".to_owned(),
            replaced: None,
        };
        let cli = cli::Cli::default();
        assert_eq!(
            server_path(&cli, Some(&installed)),
            "/home/u/.deco/bin/deco"
        );

        // Without an install, the flag decides. Without the flag, the remote's
        // PATH decides.
        assert_eq!(server_path(&cli, None), "deco");
        let cli = cli::Cli {
            remote_server_path: Some("/opt/deco".to_owned()),
            ..cli::Cli::default()
        };
        assert_eq!(server_path(&cli, None), "/opt/deco");
    }

    #[test]
    fn the_hint_for_a_remote_with_no_deco_names_both_ways_out_of_it() {
        let hint = no_server_hint("deco");
        assert!(hint.contains("--remote-install"), "{hint}");
        assert!(hint.contains("--remote-server-path"), "{hint}");

        // A wrapped string literal with incorrect continuations prints as one
        // line with gaps, which is how this message previously appeared. The
        // compiler does not detect this, so the test checks it.
        assert!(!hint.contains("  "), "gaps in the message: {hint:?}");
        assert!(!hint.contains('\n'), "a newline in the message: {hint:?}");
    }
}
