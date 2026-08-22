//! The deco editor.
//!
//! Argument parsing, configuration loading and everything else that decides what
//! the first frame shows live in the library next door, so that they can be run
//! against a directory a test built. What is left here is the part that only
//! makes sense in a process: the remote transports, the server mode, and choosing
//! a frontend.

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

    // Before any configuration is read: a server has no editor, no theme and no
    // keybindings, and reading a `settings.json` on the remote to decide how to
    // answer `fs.read` would be an authority nobody asked it to have.
    if cli.server {
        return serve(cli.workspace.as_deref(), cli.machine_settings.as_deref());
    }
    // Likewise: this process is one end of a socket, not an editor.
    if let Some(target) = cli.forward_to.as_deref() {
        return forward_to(target);
    }

    let boot = Boot::from_process();

    // A remote session: the files are on the other machine, so they are fetched
    // rather than read, and the same connection is what saves them later.
    //
    // Before the session rather than after it, which it did not used to be. The
    // connection carries the remote's own machine settings, and those are a
    // *layer* — they have to be in place before the theme is resolved and the
    // keymap is built, or the editor would come up wearing one configuration
    // and then be holding another.
    let mut connecting = Vec::new();
    let (mut remote, server_path, remote_settings) = match cli.remote.as_deref() {
        Some(authority) => {
            let (client, path, settings) = connect(authority, &cli, &mut connecting)?;
            (Some(client), Some(path), settings)
        }
        None => (None, None, None),
    };

    let mut session = startup::session(&cli, &boot, remote_settings.as_deref());
    // After the configuration's own problems, so the list reads in the order
    // things happened.
    session.problems.extend(connecting);
    // Held for as long as the editor runs: dropping these stops the listeners,
    // so binding them to the session rather than to the process is what makes a
    // forward end when the session does.
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
    // The socket goes somewhere only this account can reach — see
    // `TransportOptions::multiplexed`.
    let options = deco_remote::TransportOptions::multiplexed();

    // Only when asked. Everything about this branch — that it exists at all,
    // and that its absence is a plain failure rather than a silent fix — is the
    // decision described in `deco_remote::install`.
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
        // The overwhelmingly common cause of a server that never answers is the
        // remote having no deco, and that is fixable in one flag — so say so
        // here rather than leaving a person to guess at a protocol error.
        let missing =
            matches!(error, deco_remote::ClientError::Closed { .. }) && !cli.remote_install;
        anyhow::Error::new(error).context(if missing {
            no_server_hint(&server_path)
        } else {
            "the remote did not answer as a deco server".to_owned()
        })
    })?;
    // Worth saying once: it names the machine's own idea of where it is, which is
    // the thing a mistyped `--workspace` gets wrong invisibly.
    problems.push(format!(
        "remote session: {} is serving {}",
        command.program, hello.workspace
    ));

    // The remote's own settings, which become the `remote` layer. Asked for
    // only when the handshake says the server has the method: a server that
    // predates it would refuse, and a refusal has to stay distinguishable from
    // a machine that simply has no settings.
    //
    // A failure here is a problem message, not a failed startup. Being unable
    // to read one optional layer is not a reason to refuse to open a file, and
    // saying so is what stops it looking like the setting was applied.
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
    // The workspace as the *far end* spells it, which is what every URI a
    // language server over there sees has to be built from — and the one thing
    // this machine cannot work out for itself.
    let location = deco_tui::lsp::Location::Remote {
        authority,
        options,
        workspace: PathBuf::from(&hello.workspace),
    };
    Ok((
        deco_tui::RemoteSession { client, location },
        server_path,
        remote_settings,
    ))
}

/// Where the remote's deco is, once everything that can change the answer has.
///
/// One function because there is more than one caller — the file server and
/// every forward — and they have to agree. An install knows better than the
/// flag did, because it is what resolved `$HOME` on the remote into a path.
fn server_path(cli: &cli::Cli, installed: Option<&deco_remote::Installed>) -> String {
    match installed {
        Some(installed) => installed.path().to_owned(),
        // Bare `deco`, found on the remote's PATH, is the assumption that holds
        // when it was installed the ordinary way.
        None => cli
            .remote_server_path
            .clone()
            .unwrap_or_else(|| "deco".to_owned()),
    }
}

/// Starts every `--forward`, and says so.
///
/// Each one needs the remote's deco, which is the same binary the file server
/// runs — so `--remote-server-path` and `--remote-install` decide where it is
/// here too, and a forward without a remote has nothing to tunnel to.
fn forwards(
    cli: &cli::Cli,
    server_path: Option<&str>,
    session: &mut Session,
) -> Result<Vec<deco_remote::Forward>> {
    let options = deco_remote::TransportOptions::multiplexed();
    if cli.forwards.is_empty() {
        return Ok(Vec::new());
    }
    // `server_path` comes from the connection rather than being worked out again
    // here, and that is the whole point of passing it: computing it twice is how
    // the file server ended up talking to an installed deco while the forwards
    // looked for one on the remote's PATH that `--remote-install` had just
    // decided was not there.
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
        // Started eagerly so that a port already in use is an error now, rather
        // than a forward that silently never worked.
        let forward = deco_remote::Forward::start(command, *spec)
            .with_context(|| format!("could not forward {spec}"))?;
        session
            .problems
            .push(format!("remote session: forwarding {spec}"));
        started.push(forward);
    }
    Ok(started)
}

/// The remote half of a forward: a socket wearing stdin and stdout.
///
/// Refuses anything but loopback, which is the rule that keeps this from being a
/// way into the remote's network — see [`deco_remote::forward`].
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
        // Half-closed rather than closed: the service may still have something
        // to say after this end has finished asking.
        let _ = to_service.shutdown(Shutdown::Write);
    });
    // `pipe` rather than `io::copy` because this is the end that writes to a
    // line-buffered stdout, and a socket's bytes rarely contain a newline.
    let mut output = std::io::stdout().lock();
    pipe(&mut from_service, &mut output).context("the forwarded connection failed")?;
    // Not joined: if the service closed first, the thread above is blocked
    // reading a stdin that only the transport can close, and waiting for it
    // would hold a connection open that has nothing left to carry.
    drop(upstream);
    Ok(())
}

/// What to say when the remote answers nothing at all.
///
/// Its own function so that a test can read it. It is the one message here a
/// person is most likely to meet, and it is the difference between "something
/// went wrong" and knowing which flag fixes it.
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
/// own file: a deco that provisions a *different* deco than the one running
/// would make "which version is over there" unanswerable.
fn provision(
    authority: &deco_remote::Authority,
    cli: &cli::Cli,
    options: deco_remote::TransportOptions,
) -> Result<deco_remote::Installed> {
    let binary = std::env::current_exe().context("cannot find this deco to send it")?;
    let mut runner = deco_remote::TransportRunner::new(authority.clone(), options);
    let mut curl = deco_remote::fetch::Curl;
    // Beside this binary rather than in the system temporary directory, which on
    // a shared machine is writable by everyone: a path another account can
    // replace between the check and the upload would undo the check.
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
    // Whatever happened, the downloaded copy has been sent or refused and is not
    // wanted here. Left behind it would be an unversioned executable in a
    // directory nobody looks at.
    let _ = std::fs::remove_dir_all(&downloads);
    installed
}

/// Runs as the remote server, speaking the framed protocol over stdin and stdout.
///
/// The workspace defaults to the working directory, which is what an `ssh host
/// deco --server --stdio` with no `--workspace` means: serve where you landed.
/// Nothing outside it can be read or written, whatever is asked — see
/// [`deco_remote::server`].
fn serve(workspace: Option<&Path>, machine_settings: Option<&Path>) -> Result<()> {
    let root = match workspace {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("no working directory to serve")?,
    };
    let mut server = deco_remote::Server::new(&root)
        .with_context(|| format!("cannot serve {}", root.display()))?;
    // Only when told. Left alone, the server finds this machine's own file the
    // way the editor would.
    if let Some(path) = machine_settings {
        server = server.serving_machine_settings(Some(path.to_path_buf()));
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_install_decides_where_deco_is_for_the_forwards_as_well() {
        // The bug this pins: the file server used the path an install resolved
        // while every forward worked it out again and got `deco`, so
        // `--remote-install --forward 3000` opened files from the installed
        // binary and tunnelled to one that was not on the remote's PATH at all.
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

        // Without one, the flag decides, and without the flag it is whatever the
        // remote's PATH says.
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

        // A wrapped string literal whose continuations are broken reads as one
        // line with gaps in it, which is how this message reached a terminal
        // before anyone looked at it. Cheap to pin, invisible to a compiler.
        assert!(!hint.contains("  "), "gaps in the message: {hint:?}");
        assert!(!hint.contains('\n'), "a newline in the message: {hint:?}");
    }
}
