//! Remote authorities, in VS Code's spelling.
//!
//! A remote authority names where a workspace actually lives:
//! `ssh-remote+myhost`, `wsl+Ubuntu`, `dev-container+<id>`. VS Code writes them
//! into `vscode-remote://` URIs and into window state, so parsing the same
//! strings is what lets a deco window be opened from a VS Code link — and lets
//! a user type the authority they already know.

use std::fmt;

/// Where a workspace lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authority {
    /// The local machine.
    Local,
    /// A host reachable over SSH.
    Ssh {
        /// The SSH destination: a `~/.ssh/config` host alias, `user@host`, or a
        /// bare hostname.
        host: String,
        /// An explicit port, if the authority carried one.
        port: Option<u16>,
    },
    /// A WSL distribution.
    Wsl {
        /// The distribution name, or `None` for the default one.
        distro: Option<String>,
    },
    /// A dev container built from the workspace.
    DevContainer {
        /// The container id or name.
        id: String,
    },
    /// An already-running container.
    AttachedContainer {
        /// The container id or name.
        id: String,
    },
}

/// Failure to parse an authority.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthorityError {
    /// The scheme before `+` is not one deco knows.
    #[error("unknown remote kind `{kind}`")]
    UnknownKind {
        /// The unrecognised prefix.
        kind: String,
    },
    /// The part after `+` was missing or empty.
    #[error("`{input}` names a {kind} remote but not which one")]
    MissingTarget {
        /// The whole authority.
        input: String,
        /// The remote kind.
        kind: String,
    },
    /// The port was not a number.
    #[error("`{port}` is not a valid port")]
    InvalidPort {
        /// The offending text.
        port: String,
    },
}

impl Authority {
    /// Parses an authority such as `ssh-remote+myhost`.
    ///
    /// An empty string is the local machine, matching how VS Code treats a URI
    /// with no authority.
    pub fn parse(input: &str) -> Result<Self, AuthorityError> {
        let input = input.trim();
        if input.is_empty() || input == "local" {
            return Ok(Authority::Local);
        }

        let (kind, target) = match input.split_once('+') {
            Some((kind, target)) => (kind, target.trim()),
            // `wsl` on its own means the default distribution; every other kind
            // needs a target.
            None => (input, ""),
        };

        let missing = || AuthorityError::MissingTarget {
            input: input.to_owned(),
            kind: kind.to_owned(),
        };

        match kind {
            "ssh-remote" => {
                if target.is_empty() {
                    return Err(missing());
                }
                let parse_port = |text: &str| {
                    text.parse::<u16>()
                        .map_err(|_| AuthorityError::InvalidPort {
                            port: text.to_owned(),
                        })
                };
                // A bracketed IPv6 literal is full of colons, so the port
                // separator is the one *after* the closing bracket — splitting
                // on the last colon would cut the address in half.
                let (host, port) = if target.starts_with('[') {
                    match target.rfind("]:") {
                        Some(idx) => (
                            target[..idx + 1].to_owned(),
                            Some(parse_port(&target[idx + 2..])?),
                        ),
                        None => (target.to_owned(), None),
                    }
                } else {
                    match target.rsplit_once(':') {
                        Some((host, port)) if !host.is_empty() => {
                            (host.to_owned(), Some(parse_port(port)?))
                        }
                        _ => (target.to_owned(), None),
                    }
                };
                Ok(Authority::Ssh { host, port })
            }
            "wsl" => Ok(Authority::Wsl {
                distro: (!target.is_empty()).then(|| target.to_owned()),
            }),
            "dev-container" => {
                if target.is_empty() {
                    return Err(missing());
                }
                Ok(Authority::DevContainer {
                    id: target.to_owned(),
                })
            }
            "attached-container" => {
                if target.is_empty() {
                    return Err(missing());
                }
                Ok(Authority::AttachedContainer {
                    id: target.to_owned(),
                })
            }
            other => Err(AuthorityError::UnknownKind {
                kind: other.to_owned(),
            }),
        }
    }

    /// Parses a `vscode-remote://` or `deco-remote://` URI, returning the
    /// authority and the path within it.
    pub fn parse_uri(uri: &str) -> Result<(Self, String), AuthorityError> {
        let rest = uri
            .strip_prefix("vscode-remote://")
            .or_else(|| uri.strip_prefix("deco-remote://"))
            .unwrap_or(uri);
        match rest.split_once('/') {
            Some((authority, path)) => Ok((Self::parse(authority)?, format!("/{path}"))),
            None => Ok((Self::parse(rest)?, String::new())),
        }
    }

    /// Whether this authority is the local machine.
    pub fn is_local(&self) -> bool {
        matches!(self, Authority::Local)
    }

    /// A short label for the status bar, matching VS Code's wording.
    pub fn label(&self) -> String {
        match self {
            Authority::Local => "Local".to_owned(),
            Authority::Ssh { host, port: None } => format!("SSH: {host}"),
            Authority::Ssh {
                host,
                port: Some(port),
            } => format!("SSH: {host}:{port}"),
            Authority::Wsl { distro: None } => "WSL".to_owned(),
            Authority::Wsl {
                distro: Some(distro),
            } => format!("WSL: {distro}"),
            Authority::DevContainer { id } => format!("Dev Container: {id}"),
            Authority::AttachedContainer { id } => format!("Container: {id}"),
        }
    }
}

impl fmt::Display for Authority {
    /// Writes the authority back in the form [`Authority::parse`] accepts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Authority::Local => f.write_str("local"),
            Authority::Ssh { host, port: None } => write!(f, "ssh-remote+{host}"),
            Authority::Ssh {
                host,
                port: Some(port),
            } => write!(f, "ssh-remote+{host}:{port}"),
            Authority::Wsl { distro: None } => f.write_str("wsl"),
            Authority::Wsl {
                distro: Some(distro),
            } => write!(f, "wsl+{distro}"),
            Authority::DevContainer { id } => write!(f, "dev-container+{id}"),
            Authority::AttachedContainer { id } => write!(f, "attached-container+{id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_authority_is_local() {
        assert_eq!(Authority::parse("").unwrap(), Authority::Local);
        assert_eq!(Authority::parse("local").unwrap(), Authority::Local);
        assert!(Authority::parse("").unwrap().is_local());
    }

    #[test]
    fn ssh_authorities_carry_their_host() {
        assert_eq!(
            Authority::parse("ssh-remote+myhost").unwrap(),
            Authority::Ssh {
                host: "myhost".into(),
                port: None
            }
        );
        assert_eq!(
            Authority::parse("ssh-remote+user@example.com").unwrap(),
            Authority::Ssh {
                host: "user@example.com".into(),
                port: None
            }
        );
    }

    #[test]
    fn ssh_authorities_may_carry_a_port() {
        assert_eq!(
            Authority::parse("ssh-remote+myhost:2222").unwrap(),
            Authority::Ssh {
                host: "myhost".into(),
                port: Some(2222)
            }
        );
    }

    #[test]
    fn an_ipv6_host_keeps_its_colons() {
        assert_eq!(
            Authority::parse("ssh-remote+[2001:db8::1]").unwrap(),
            Authority::Ssh {
                host: "[2001:db8::1]".into(),
                port: None
            }
        );
    }

    #[test]
    fn an_ipv6_host_can_still_carry_a_port() {
        assert_eq!(
            Authority::parse("ssh-remote+[2001:db8::1]:2222").unwrap(),
            Authority::Ssh {
                host: "[2001:db8::1]".into(),
                port: Some(2222)
            }
        );
    }

    #[test]
    fn a_non_numeric_port_is_an_error() {
        assert_eq!(
            Authority::parse("ssh-remote+myhost:not-a-port"),
            Err(AuthorityError::InvalidPort {
                port: "not-a-port".into()
            })
        );
    }

    #[test]
    fn wsl_defaults_to_the_default_distribution() {
        assert_eq!(
            Authority::parse("wsl").unwrap(),
            Authority::Wsl { distro: None }
        );
        assert_eq!(
            Authority::parse("wsl+Ubuntu-22.04").unwrap(),
            Authority::Wsl {
                distro: Some("Ubuntu-22.04".into())
            }
        );
    }

    #[test]
    fn container_authorities_carry_their_id() {
        assert_eq!(
            Authority::parse("dev-container+abc123").unwrap(),
            Authority::DevContainer {
                id: "abc123".into()
            }
        );
        assert_eq!(
            Authority::parse("attached-container+my-container").unwrap(),
            Authority::AttachedContainer {
                id: "my-container".into()
            }
        );
    }

    #[test]
    fn a_kind_with_no_target_is_an_error() {
        for input in [
            "ssh-remote",
            "ssh-remote+",
            "dev-container+",
            "attached-container",
        ] {
            assert!(
                matches!(
                    Authority::parse(input),
                    Err(AuthorityError::MissingTarget { .. })
                ),
                "{input} should need a target"
            );
        }
    }

    #[test]
    fn an_unknown_kind_is_an_error() {
        assert_eq!(
            Authority::parse("telepathy+brain"),
            Err(AuthorityError::UnknownKind {
                kind: "telepathy".into()
            })
        );
    }

    #[test]
    fn uris_split_into_an_authority_and_a_path() {
        let (authority, path) =
            Authority::parse_uri("vscode-remote://ssh-remote+myhost/home/u/main.rs").unwrap();
        assert_eq!(
            authority,
            Authority::Ssh {
                host: "myhost".into(),
                port: None
            }
        );
        assert_eq!(path, "/home/u/main.rs");
    }

    #[test]
    fn decos_own_uri_scheme_is_accepted_too() {
        let (authority, path) = Authority::parse_uri("deco-remote://wsl+Ubuntu/home/u").unwrap();
        assert_eq!(
            authority,
            Authority::Wsl {
                distro: Some("Ubuntu".into())
            }
        );
        assert_eq!(path, "/home/u");
    }

    #[test]
    fn a_uri_with_no_path_yields_an_empty_one() {
        let (authority, path) = Authority::parse_uri("vscode-remote://wsl").unwrap();
        assert_eq!(authority, Authority::Wsl { distro: None });
        assert_eq!(path, "");
    }

    #[test]
    fn authorities_round_trip_through_display() {
        for input in [
            "local",
            "ssh-remote+myhost",
            "ssh-remote+myhost:2222",
            "wsl",
            "wsl+Ubuntu",
            "dev-container+abc",
            "attached-container+xyz",
        ] {
            let parsed = Authority::parse(input).unwrap();
            assert_eq!(parsed.to_string(), input);
            assert_eq!(Authority::parse(&parsed.to_string()).unwrap(), parsed);
        }
    }

    #[test]
    fn labels_read_the_way_vs_code_writes_them() {
        assert_eq!(
            Authority::parse("ssh-remote+box").unwrap().label(),
            "SSH: box"
        );
        assert_eq!(
            Authority::parse("wsl+Ubuntu").unwrap().label(),
            "WSL: Ubuntu"
        );
        assert_eq!(
            Authority::parse("dev-container+abc").unwrap().label(),
            "Dev Container: abc"
        );
    }
}
