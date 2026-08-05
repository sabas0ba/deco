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
    /// File to open. `None` starts on an empty buffer.
    pub file: Option<PathBuf>,
    /// Which frontend to use.
    pub frontend: Frontend,
    /// Print the resolved configuration and exit.
    pub print_config: bool,
    /// Ignore the user's settings.json and keybindings.json.
    pub clean: bool,
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
    /// More than one file was given. deco opens one buffer.
    ExtraArgument(String),
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
            Self::ExtraArgument(arg) => {
                write!(f, "unexpected argument `{arg}` — deco opens one file")
            }
        }
    }
}

impl std::error::Error for CliError {}

/// The help text. `deco --help`.
pub const HELP: &str = "\
A lightweight, VS Code-compatible editor.

Usage: deco [OPTIONS] [FILE]

Arguments:
  [FILE]  File to open

Options:
      --frontend <FRONTEND>  Which frontend to use [default: tui] [possible values: tui, gui]
      --print-config         Print the resolved configuration and exit
      --clean                Ignore the user's settings.json and keybindings.json
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
            "--frontend" => {
                let value = args.next().ok_or(CliError::MissingValue("--frontend"))?;
                cli.frontend = Frontend::parse(value.as_ref())?;
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--frontend=") {
                    cli.frontend = Frontend::parse(value)?;
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

fn set_file(cli: &mut Cli, arg: &str) -> Result<(), CliError> {
    if cli.file.is_some() {
        return Err(CliError::ExtraArgument(arg.to_string()));
    }
    cli.file = Some(PathBuf::from(arg));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unwraps to the options, failing the test on help/version/error. Every
    /// case below that cares about options rather than control flow uses it.
    fn run(args: &[&str]) -> Cli {
        match parse(args) {
            Ok(Outcome::Run(cli)) => *cli,
            other => panic!("expected options, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_invocation_opens_the_terminal_frontend() {
        let cli = run(&[]);
        assert_eq!(cli.frontend, Frontend::Tui);
        assert!(cli.file.is_none());
        assert!(!cli.clean);
        assert!(!cli.print_config);
    }

    #[test]
    fn a_file_argument_is_parsed() {
        assert_eq!(
            run(&["src/main.rs"]).file,
            Some(PathBuf::from("src/main.rs"))
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
            file: Some(PathBuf::from("a.rs")),
            frontend: Frontend::Gui,
            print_config: false,
            clean: true,
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
        assert_eq!(cli.file, Some(PathBuf::from("--clean")));
        assert!(!cli.clean, "past `--` it is a path, not the flag");
    }

    #[test]
    fn a_double_dash_does_not_discard_earlier_flags() {
        let cli = run(&["--clean", "--", "-weird.rs"]);
        assert!(cli.clean);
        assert_eq!(cli.file, Some(PathBuf::from("-weird.rs")));
    }

    #[test]
    fn a_second_file_is_rejected() {
        assert_eq!(
            parse(["a.rs", "b.rs"]),
            Err(CliError::ExtraArgument("b.rs".into()))
        );
    }

    #[test]
    fn a_lone_dash_is_a_path_not_a_flag() {
        assert_eq!(run(&["-"]).file, Some(PathBuf::from("-")));
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
            CliError::ExtraArgument("b.rs".into()),
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
