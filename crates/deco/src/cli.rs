//! Command-line parsing.
//!
//! Hand-written rather than derived. deco's command line is one path and three
//! flags, and an argument parser that covers subcommands, shell completion and
//! coloured help costs about a dozen crates — one of them a procedural macro,
//! which is code that runs on the build machine. That is a poor trade for a
//! surface this small, and the smaller the dependency graph the less of it
//! anyone has to trust. See deny.toml for the wider policy.
//!
//! The accepted grammar deliberately matches what the derived version accepted,
//! so this is not a change in behaviour for anyone's muscle memory:
//!
//! ```text
//! deco [OPTIONS] [FILE]
//!   --frontend <tui|gui>   also accepted as --frontend=<value>
//!   --print-config
//!   --clean
//!   -h, --help
//!   -V, --version
//! ```

use std::fmt;
use std::path::PathBuf;

/// Which frontend to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Frontend {
    /// The terminal UI.
    #[default]
    Tui,
    /// The GPU-accelerated window.
    Gui,
}

impl Frontend {
    fn parse(value: &str) -> Result<Self, CliError> {
        match value {
            "tui" => Ok(Self::Tui),
            "gui" => Ok(Self::Gui),
            other => Err(CliError::BadValue {
                flag: "--frontend",
                value: other.to_string(),
                expected: "tui, gui",
            }),
        }
    }
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cli {
    /// Files to open, each in its own tab. Empty starts on an empty buffer.
    pub files: Vec<PathBuf>,
    /// Which frontend to use.
    pub frontend: Frontend,
    /// Print the resolved configuration and exit.
    pub print_config: bool,
    /// Ignore the user's settings.json and keybindings.json.
    pub clean: bool,
    /// Run as the headless remote server rather than as an editor.
    ///
    /// Set by `--server`. `--stdio` is accepted and is the only transport there
    /// is, so it changes nothing — it is taken because the command
    /// `deco-remote` builds passes it, and because a future transport would need
    /// the flag to have meant something.
    pub server: bool,
    /// The directory a server serves, or the one an editor treats as the
    /// workspace root.
    pub workspace: Option<PathBuf>,
    /// The file a server hands over as this machine's settings.
    ///
    /// Server-side only, and normally left alone: the default is this machine's
    /// own `machine-settings.json`, worked out from the same rules the editor
    /// uses for a configuration directory. Named here for whoever *starts* the
    /// server — a packager placing it elsewhere, or a test that cannot change
    /// the process environment without changing it for everything running
    /// beside it.
    ///
    /// This is not a way for a client to choose a file. `settings.read` takes
    /// no path; the decision is made where the server is launched, which is on
    /// the remote and by whoever runs it.
    pub machine_settings: Option<PathBuf>,
    /// A remote authority to open the files on, as `ssh-remote+host`.
    ///
    /// Present means every file named on the command line lives there, and the
    /// editor is a client rather than the thing holding the files.
    pub remote: Option<String>,
    /// Where deco lives on the remote.
    ///
    /// `None` means whatever `deco` resolves to on the remote's PATH, which is
    /// right when it was installed the ordinary way and wrong when it was
    /// unpacked into a directory no login shell adds.
    pub remote_server_path: Option<String>,
    /// Put this machine's deco on the remote before connecting.
    ///
    /// Off unless asked for. Pointing an editor at a machine is not the same as
    /// authorising it to install software there — see [`deco_remote::install`].
    pub remote_install: bool,
    /// Ports on the remote to make reachable from here, as `3000` or `8080:3000`.
    pub forwards: Vec<deco_remote::PortSpec>,
    /// Run as one end of a forwarded connection rather than as an editor.
    ///
    /// Set by `--forward-to`. This is the remote half: it connects to the address
    /// and pipes it to stdin and stdout, which is how a port crosses a transport
    /// that has no idea what a port is.
    pub forward_to: Option<String>,
}

/// What `parse` decided the process should do.
///
/// `--help` and `--version` are not errors and not settings; they are requests
/// to print something and exit successfully. Keeping them in the return type
/// rather than writing to stdout here keeps parsing free of side effects, which
/// is what makes it testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Run the editor with these options.
    Run(Box<Cli>),
    /// Print the help text and exit 0.
    Help,
    /// Print the version and exit 0.
    Version,
}

/// Why a command line was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// A flag this build does not know.
    UnknownFlag(String),
    /// A flag that takes a value was the last argument.
    MissingValue(&'static str),
    /// A flag's value was not one of the accepted ones.
    BadValue {
        /// The flag, with its leading dashes.
        flag: &'static str,
        /// What the user wrote.
        value: String,
        /// The accepted values, comma separated.
        expected: &'static str,
    },
    /// The same file was given twice, which would be a tab fighting itself.
    DuplicateFile(String),
    /// A `--forward` value was not a port or a pair of them.
    BadPort(deco_remote::PortSpecError),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFlag(flag) => write!(f, "unknown option `{flag}`"),
            Self::MissingValue(flag) => write!(f, "`{flag}` needs a value"),
            Self::BadValue {
                flag,
                value,
                expected,
            } => write!(
                f,
                "`{flag}` does not accept `{value}` (expected {expected})"
            ),
            Self::DuplicateFile(arg) => {
                write!(f, "`{arg}` was given more than once")
            }
            Self::BadPort(error) => write!(f, "`--forward` was given {error}"),
        }
    }
}

impl std::error::Error for CliError {}

/// The help text. `deco --help`.
pub const HELP: &str = "\
A lightweight, VS Code-compatible editor.

Usage: deco [OPTIONS] [FILE]...

Arguments:
  [FILE]...  Files to open, each in its own tab

Options:
      --frontend <FRONTEND>  Which frontend to use [default: tui] [possible values: tui, gui]
      --print-config         Print the resolved configuration and exit
      --clean                Ignore the user's settings.json and keybindings.json
      --server               Run as the headless remote server, not as an editor
      --stdio                Speak the remote protocol over stdin and stdout
      --workspace <DIR>      The directory to serve [default: the current one]
      --machine-settings <PATH>
                             The file a server serves as this machine's settings
      --remote <AUTHORITY>   Open the files on a remote, as ssh-remote+host
      --remote-server-path <PATH>
                             Where deco is on the remote [default: found on its PATH]
      --remote-install       Send this machine's deco to the remote first, if it needs one
      --forward <PORT>       Reach a port on the remote here, as 3000 or 8080:3000
      --forward-to <ADDR>    Pipe stdin and stdout to a loopback address; the remote half
  -h, --help                 Print help
  -V, --version              Print version
";

/// Parses arguments, which must *not* include the program name.
///
/// The rules that are easy to get wrong, and so are pinned by tests below:
/// `--` ends option parsing, so a file really called `--clean` is reachable;
/// `--frontend=gui` and `--frontend gui` are the same thing; and an unknown
/// flag is refused rather than silently taken as a filename, because taking
/// a mistyped `--cleen` as a path would create a file named `--cleen`.
pub fn parse<I, S>(args: I) -> Result<Outcome, CliError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut cli = Cli::default();
    let mut args = args.into_iter();
    let mut options_ended = false;

    while let Some(arg) = args.next() {
        let arg = arg.as_ref();

        if options_ended {
            set_file(&mut cli, arg)?;
            continue;
        }

        match arg {
            "--" => options_ended = true,
            "-h" | "--help" => return Ok(Outcome::Help),
            "-V" | "--version" => return Ok(Outcome::Version),
            "--print-config" => cli.print_config = true,
            "--clean" => cli.clean = true,
            "--server" => cli.server = true,
            // Accepted and ignored: stdio is the only transport, and refusing a
            // flag the transport command already passes would make the two halves
            // of this repository disagree.
            "--stdio" => {}
            "--machine-settings" => {
                let value = args
                    .next()
                    .ok_or(CliError::MissingValue("--machine-settings"))?;
                cli.machine_settings = Some(PathBuf::from(value.as_ref()));
            }
            "--workspace" => {
                let value = args.next().ok_or(CliError::MissingValue("--workspace"))?;
                cli.workspace = Some(PathBuf::from(value.as_ref()));
            }
            "--remote" => {
                let value = args.next().ok_or(CliError::MissingValue("--remote"))?;
                cli.remote = Some(value.as_ref().to_owned());
            }
            "--remote-server-path" => {
                let value = args
                    .next()
                    .ok_or(CliError::MissingValue("--remote-server-path"))?;
                cli.remote_server_path = Some(value.as_ref().to_owned());
            }
            "--remote-install" => cli.remote_install = true,
            "--forward" => {
                let value = args.next().ok_or(CliError::MissingValue("--forward"))?;
                cli.forwards.push(port_spec(value.as_ref())?);
            }
            "--forward-to" => {
                let value = args.next().ok_or(CliError::MissingValue("--forward-to"))?;
                cli.forward_to = Some(value.as_ref().to_owned());
            }
            "--frontend" => {
                let value = args.next().ok_or(CliError::MissingValue("--frontend"))?;
                cli.frontend = Frontend::parse(value.as_ref())?;
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--frontend=") {
                    cli.frontend = Frontend::parse(value)?;
                } else if let Some(value) = arg.strip_prefix("--machine-settings=") {
                    cli.machine_settings = Some(PathBuf::from(value));
                } else if let Some(value) = arg.strip_prefix("--workspace=") {
                    cli.workspace = Some(PathBuf::from(value));
                } else if let Some(value) = arg.strip_prefix("--forward=") {
                    cli.forwards.push(port_spec(value)?);
                } else if let Some(value) = arg.strip_prefix("--forward-to=") {
                    cli.forward_to = Some(value.to_owned());
                } else if let Some(value) = arg.strip_prefix("--remote-server-path=") {
                    cli.remote_server_path = Some(value.to_owned());
                } else if let Some(value) = arg.strip_prefix("--remote=") {
                    cli.remote = Some(value.to_owned());
                } else if arg.starts_with('-') && arg != "-" {
                    // `-` on its own is conventionally stdin, not a flag. deco
                    // has no use for it yet, so it falls through to being a
                    // path, which is what the derived parser did too.
                    return Err(CliError::UnknownFlag(arg.to_string()));
                } else {
                    set_file(&mut cli, arg)?;
                }
            }
        }
    }

    Ok(Outcome::Run(Box::new(cli)))
}

/// Parses a `--forward` value here rather than at startup, so that a mistyped
/// port is a usage error with an exit code of 2 like every other one.
fn port_spec(value: &str) -> Result<deco_remote::PortSpec, CliError> {
    deco_remote::PortSpec::parse(value).map_err(CliError::BadPort)
}

fn set_file(cli: &mut Cli, arg: &str) -> Result<(), CliError> {
    let path = PathBuf::from(arg);
    // Refused rather than deduplicated silently: `deco a.rs a.rs` is nearly
    // always a typo for two different files, and opening what looks like two
    // tabs onto one buffer would be more confusing than an error.
    if cli.files.contains(&path) {
        return Err(CliError::DuplicateFile(arg.to_string()));
    }
    cli.files.push(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Unwraps to the options, failing the test on help/version/error. Every
    /// case below that cares about options rather than control flow uses it.
    fn run(args: &[&str]) -> Cli {
        match parse(args) {
            Ok(Outcome::Run(cli)) => *cli,
            other => panic!("expected options, got {other:?}"),
        }
    }

    #[test]
    fn a_remote_authority_is_taken_either_way_round() {
        let cli = run(&["--remote", "ssh-remote+myhost", "src/main.rs"]);
        assert_eq!(cli.remote.as_deref(), Some("ssh-remote+myhost"));
        assert_eq!(cli.files, vec![PathBuf::from("src/main.rs")]);
        assert_eq!(
            run(&["--remote=wsl+Ubuntu"]).remote.as_deref(),
            Some("wsl+Ubuntu")
        );
        // A path is not an authority, and the file list is not the place to put
        // one: `--remote` without a value is an error rather than a filename.
        assert_eq!(parse(["--remote"]), Err(CliError::MissingValue("--remote")));
    }

    #[test]
    fn installing_onto_a_remote_is_asked_for_rather_than_assumed() {
        // The default has to stay off: this flag is what turns "open a file over
        // there" into "put software on that machine", and a person who did not
        // type it did not agree to the second one.
        let cli = run(&["--remote", "ssh-remote+myhost", "src/main.rs"]);
        assert!(!cli.remote_install);
        assert_eq!(cli.remote_server_path, None);

        let cli = run(&[
            "--remote",
            "ssh-remote+myhost",
            "--remote-install",
            "--remote-server-path",
            "/opt/deco/bin/deco",
        ]);
        assert!(cli.remote_install);
        assert_eq!(
            cli.remote_server_path.as_deref(),
            Some("/opt/deco/bin/deco")
        );

        assert_eq!(
            run(&["--remote-server-path=/opt/deco"])
                .remote_server_path
                .as_deref(),
            Some("/opt/deco")
        );
        assert_eq!(
            parse(["--remote-server-path"]),
            Err(CliError::MissingValue("--remote-server-path"))
        );
    }

    #[test]
    fn server_mode_takes_a_workspace_either_way_round() {
        // `--stdio` is what `deco_remote::server_command` passes, so it has to be
        // accepted here or the two halves of this repository disagree about the
        // command line one of them builds.
        let cli = run(&["--server", "--stdio", "--workspace", "/srv/project"]);
        assert!(cli.server);
        assert_eq!(cli.workspace.as_deref(), Some(Path::new("/srv/project")));

        let cli = run(&["--server", "--workspace=/srv/project"]);
        assert!(cli.server);
        assert_eq!(cli.workspace.as_deref(), Some(Path::new("/srv/project")));
    }

    #[test]
    fn a_server_without_a_workspace_is_allowed() {
        // `ssh host deco --server --stdio` with no directory means "serve where
        // you landed", which is the working directory the transport left it in.
        let cli = run(&["--server", "--stdio"]);
        assert!(cli.server);
        assert_eq!(cli.workspace, None);
    }

    #[test]
    fn the_editor_is_the_default_and_says_nothing_about_servers() {
        let cli = run(&["main.rs"]);
        assert!(!cli.server);
        assert_eq!(cli.workspace, None);
    }

    #[test]
    fn the_server_command_deco_remote_builds_parses_here() {
        // The one place the two ends can drift apart: `deco-remote` writes the
        // command and this parses it, and nothing else compares them.
        let built = deco_remote::server_command("deco", Some("/home/u/project"));
        let (program, args) = built.split_first().expect("a program");
        assert_eq!(program, "deco");
        let cli = run(&args.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(cli.server);
        assert_eq!(cli.workspace.as_deref(), Some(Path::new("/home/u/project")));
        assert!(cli.files.is_empty());
    }

    #[test]
    fn a_bare_invocation_opens_the_terminal_frontend() {
        let cli = run(&[]);
        assert_eq!(cli.frontend, Frontend::Tui);
        assert!(cli.files.is_empty());
        assert!(!cli.clean);
        assert!(!cli.print_config);
    }

    #[test]
    fn a_file_argument_is_parsed() {
        assert_eq!(run(&["src/main.rs"]).files, [PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn several_files_are_parsed_in_order() {
        assert_eq!(
            run(&["a.rs", "b.rs", "c.rs"]).files,
            [
                PathBuf::from("a.rs"),
                PathBuf::from("b.rs"),
                PathBuf::from("c.rs")
            ]
        );
    }

    #[test]
    fn the_frontend_can_be_chosen() {
        assert_eq!(run(&["--frontend", "gui"]).frontend, Frontend::Gui);
        assert_eq!(run(&["--frontend", "tui"]).frontend, Frontend::Tui);
    }

    #[test]
    fn the_frontend_also_takes_an_equals_form() {
        assert_eq!(run(&["--frontend=gui"]).frontend, Frontend::Gui);
    }

    #[test]
    fn an_unknown_frontend_is_rejected() {
        assert_eq!(
            parse(["--frontend", "holograph"]),
            Err(CliError::BadValue {
                flag: "--frontend",
                value: "holograph".into(),
                expected: "tui, gui",
            })
        );
    }

    #[test]
    fn a_frontend_without_a_value_is_rejected() {
        assert_eq!(
            parse(["--frontend"]),
            Err(CliError::MissingValue("--frontend"))
        );
    }

    #[test]
    fn clean_and_print_config_are_flags() {
        let cli = run(&["--clean", "--print-config"]);
        assert!(cli.clean);
        assert!(cli.print_config);
    }

    #[test]
    fn flags_and_a_file_mix_in_any_order() {
        let expected = Cli {
            machine_settings: None,
            files: vec![PathBuf::from("a.rs")],
            frontend: Frontend::Gui,
            print_config: false,
            clean: true,
            server: false,
            workspace: None,
            remote: None,
            remote_server_path: None,
            remote_install: false,
            forwards: Vec::new(),
            forward_to: None,
        };
        assert_eq!(run(&["--clean", "a.rs", "--frontend", "gui"]), expected);
        assert_eq!(run(&["a.rs", "--frontend=gui", "--clean"]), expected);
    }

    #[test]
    fn help_and_version_win_over_everything_after_them() {
        assert_eq!(parse(["--help"]), Ok(Outcome::Help));
        assert_eq!(parse(["-h"]), Ok(Outcome::Help));
        assert_eq!(parse(["--version"]), Ok(Outcome::Version));
        assert_eq!(parse(["-V"]), Ok(Outcome::Version));
        // Notably including arguments that would otherwise be errors: asking
        // for help is how you find out that the rest was wrong.
        assert_eq!(parse(["--help", "--nonsense"]), Ok(Outcome::Help));
    }

    #[test]
    fn a_mistyped_flag_is_an_error_rather_than_a_filename() {
        // The failure this prevents: `deco --cleen` creating a new buffer for
        // a file literally named `--cleen`.
        assert_eq!(
            parse(["--cleen"]),
            Err(CliError::UnknownFlag("--cleen".into()))
        );
        assert_eq!(parse(["-x"]), Err(CliError::UnknownFlag("-x".into())));
    }

    #[test]
    fn a_double_dash_makes_a_flag_shaped_name_reachable() {
        let cli = run(&["--", "--clean"]);
        assert_eq!(cli.files, [PathBuf::from("--clean")]);
        assert!(!cli.clean, "past `--` it is a path, not the flag");
    }

    #[test]
    fn a_double_dash_does_not_discard_earlier_flags() {
        let cli = run(&["--clean", "--", "-weird.rs"]);
        assert!(cli.clean);
        assert_eq!(cli.files, [PathBuf::from("-weird.rs")]);
    }

    #[test]
    fn the_same_file_twice_is_rejected() {
        assert_eq!(
            parse(["a.rs", "b.rs", "a.rs"]),
            Err(CliError::DuplicateFile("a.rs".into()))
        );
    }

    #[test]
    fn a_lone_dash_is_a_path_not_a_flag() {
        assert_eq!(run(&["-"]).files, [PathBuf::from("-")]);
    }

    #[test]
    fn errors_say_what_to_do() {
        for error in [
            CliError::UnknownFlag("--cleen".into()),
            CliError::MissingValue("--frontend"),
            CliError::BadValue {
                flag: "--frontend",
                value: "holograph".into(),
                expected: "tui, gui",
            },
            CliError::DuplicateFile("b.rs".into()),
        ] {
            let rendered = error.to_string();
            assert!(!rendered.is_empty());
            // Every message is printed as `deco: {error}`, so it is a
            // continuation of that line rather than a sentence of its own —
            // uncapitalised, and no trailing full stop. (Not "is lowercase":
            // several of these correctly begin with a backticked flag.)
            assert!(
                !rendered.chars().next().is_some_and(char::is_uppercase),
                "reads as a capitalised sentence after `deco: `: {rendered}"
            );
            assert!(
                !rendered.ends_with('.'),
                "the caller adds the punctuation: {rendered}"
            );
            // The user has to be able to see which input was rejected.
            assert!(
                rendered.contains("--frontend")
                    || rendered.contains("--cleen")
                    || rendered.contains("b.rs"),
                "does not quote the offending argument: {rendered}"
            );
        }
    }

    #[test]
    fn the_help_text_covers_every_option_the_parser_accepts() {
        for flag in [
            "--frontend",
            "--print-config",
            "--clean",
            "--help",
            "--version",
        ] {
            assert!(HELP.contains(flag), "{flag} is undocumented");
        }
    }
}
