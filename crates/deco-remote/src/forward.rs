//! Reaching a port on the remote from this machine.
//!
//! A service listening on the remote's `:3000`, such as a dev server, is not
//! reachable from the local machine without port forwarding.
//!
//! # Why deco is its own tunnel
//!
//! `ssh -L` is not used because it is available on only one of deco's three
//! transports. `docker exec` cannot forward a port, and WSL has no equivalent of
//! `-L`. Forwarding is meant to work on every transport, not only SSH.
//!
//! The remote deco therefore acts as the tunnel. [`forward_command`] runs
//! `deco --forward-to 127.0.0.1:3000 --stdio`, which connects to that port and
//! pipes it to its own stdin and stdout. Every transport already carries a
//! program's stdio, as the file server does, so forwarding works over all three
//! without `socat`, `nc` or any other tool on the remote.
//!
//! Each connection starts a new process. Over SSH that would mean an
//! authentication round-trip per connection. The `TransportOptions` default has
//! no control path and does not multiplex, so the `deco` binary builds its
//! options with
//! [`TransportOptions::multiplexed`](crate::TransportOptions::multiplexed).
//! With a control socket, later connections reuse the existing SSH session.
//!
//! # Reachable addresses
//!
//! Only loopback addresses on the remote are reachable.
//! `deco --forward-to 10.0.0.5:5432` is rejected, because otherwise deco would
//! act as a proxy into the remote's private network. The file server applies the
//! same restriction to paths.
//!
//! The local side also listens only on loopback. This is the more important
//! restriction: binding `0.0.0.0` would expose the remote service, such as a
//! database, to this machine's network.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crate::transport::Command;

/// A mapping from a local port to a remote port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSpec {
    /// The port to listen on, on this machine.
    pub local: u16,
    /// The port to connect to, on the remote.
    pub remote: u16,
}

/// Why a `--forward` value was rejected.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PortSpecError {
    /// A value was not a number, or was out of range.
    #[error("`{value}` is not a port number")]
    NotAPort {
        /// What was written.
        value: String,
    },
    /// Port 0 means "any free port" and cannot be forwarded.
    #[error("port 0 is not a port to forward")]
    Zero,
    /// More than one colon, so it is not `local:remote`.
    #[error("`{value}` is not a port or a `local:remote` pair")]
    Shape {
        /// What was written.
        value: String,
    },
}

impl PortSpec {
    /// Parses `3000` or `8080:3000`.
    ///
    /// A bare number uses the same port locally and remotely. This is the
    /// common case and matches VS Code's default.
    pub fn parse(value: &str) -> Result<Self, PortSpecError> {
        let port = |text: &str| -> Result<u16, PortSpecError> {
            let port: u16 = text.trim().parse().map_err(|_| PortSpecError::NotAPort {
                value: text.trim().to_owned(),
            })?;
            if port == 0 {
                return Err(PortSpecError::Zero);
            }
            Ok(port)
        };
        let mut parts = value.split(':');
        let (Some(first), second, None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(PortSpecError::Shape {
                value: value.to_owned(),
            });
        };
        match second {
            Some(second) => Ok(Self {
                local: port(first)?,
                remote: port(second)?,
            }),
            None => {
                let both = port(first)?;
                Ok(Self {
                    local: both,
                    remote: both,
                })
            }
        }
    }
}

impl std::fmt::Display for PortSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "localhost:{} to the remote's {}",
            self.local, self.remote
        )
    }
}

/// The command that turns a remote deco into a pipe to `port` on its loopback.
pub fn forward_command(server_path: &str, port: u16) -> Vec<String> {
    vec![
        server_path.to_owned(),
        "--forward-to".to_owned(),
        // A full address rather than a bare port, so that the remote's error
        // message can name it and the process list shows an unambiguous
        // argument.
        format!("127.0.0.1:{port}"),
        "--stdio".to_owned(),
    ]
}

/// Why a forward could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    /// The local port could not be listened on.
    #[error("could not listen on localhost:{port}: {error}")]
    Listen {
        /// The port that was tried.
        port: u16,
        /// What the operating system said.
        error: io::Error,
    },
}

/// A port on this machine standing in for one on the remote.
///
/// Dropping it stops the listener, so a forward lasts as long as the session
/// that created it rather than the whole process.
#[derive(Debug)]
pub struct Forward {
    spec: PortSpec,
    address: SocketAddr,
    running: Arc<AtomicBool>,
}

impl Forward {
    /// Listens on `spec.local` and gives every connection its own `command`.
    ///
    /// `command` is the transport command that runs [`forward_command`] on the
    /// remote. It is cloned per connection because each connection needs its own
    /// process. A single pipe cannot carry multiple connections without a
    /// multiplexing protocol, and this design intentionally has none.
    pub fn start(command: Command, spec: PortSpec) -> Result<Self, ForwardError> {
        // Bind to loopback, never `0.0.0.0`. The forwarded service runs on the
        // remote machine, and requesting a forward must not expose it to this
        // machine's network.
        let listener =
            TcpListener::bind(("127.0.0.1", spec.local)).map_err(|error| ForwardError::Listen {
                port: spec.local,
                error,
            })?;
        let address = listener
            .local_addr()
            .map_err(|error| ForwardError::Listen {
                port: spec.local,
                error,
            })?;

        let running = Arc::new(AtomicBool::new(true));
        let stop = Arc::clone(&running);
        thread::spawn(move || {
            for stream in listener.incoming() {
                if !stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let command = command.clone();
                // Detached. The accept loop does not track connections, and the
                // thread ends when the socket closes.
                thread::spawn(move || {
                    let _ = carry(&command, stream);
                });
            }
        });

        Ok(Self {
            spec,
            address,
            running,
        })
    }

    /// The port mapping, for display.
    pub fn spec(&self) -> PortSpec {
        self.spec
    }

    /// The address it is actually listening on.
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        // The accept loop is blocked in `accept`, and setting the flag does not
        // wake it. A connection wakes it, and the loop then checks the flag and
        // stops.
        let _ = TcpStream::connect(self.address);
    }
}

/// Copies `from` into `to`, flushing every chunk.
///
/// [`std::io::copy`] is not used because a process's stdout is line buffered.
/// Data without a newline, which is most socket traffic, would stay in the
/// buffer while the client waits. A tunnel must forward bytes immediately.
pub fn pipe(from: &mut dyn io::Read, to: &mut dyn io::Write) -> io::Result<u64> {
    let mut buffer = [0u8; 32 * 1024];
    let mut total = 0;
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        to.write_all(&buffer[..read])?;
        to.flush()?;
        total += read as u64;
    }
}

/// Carries one connection to the remote and back.
fn carry(command: &Command, stream: TcpStream) -> io::Result<()> {
    use std::process::{Command as OsCommand, Stdio};

    let mut child = OsCommand::new(&command.program)
        .args(&command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherited for the same reason as in the client: `ssh` writes its
        // diagnostics there, and capturing them would hide failures.
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut to_remote = child.stdin.take().expect("stdin was piped");
    let mut from_remote = child.stdout.take().expect("stdout was piped");
    let mut from_client = stream.try_clone()?;
    let mut to_client = stream;

    let upstream = thread::spawn(move || {
        let _ = pipe(&mut from_client, &mut to_remote);
        // Dropped so the remote receives end-of-input and closes its socket,
        // instead of both sides waiting for each other.
        drop(to_remote);
    });

    let _ = pipe(&mut from_remote, &mut to_client);
    // When either direction ends, the connection is over. Shutting down the
    // socket unblocks the thread above if it is still reading from it.
    let _ = to_client.shutdown(Shutdown::Both);
    let _ = child.kill();
    let _ = child.wait();
    let _ = upstream.join();
    Ok(())
}

/// Resolves the `--forward-to` argument and rejects non-loopback addresses.
///
/// This runs on the remote and prevents a deco server from giving access to
/// the network it runs in. The name is resolved first and every resulting
/// address is checked. `localhost` is loopback only by convention, and the
/// remote's `/etc/hosts` can map it elsewhere, so checking the name alone is
/// not sufficient.
pub fn resolve_loopback(target: &str) -> Result<SocketAddr, String> {
    use std::net::ToSocketAddrs;

    // `forward_command` never sends a bare port, but it is accepted here for
    // manual use.
    let target = if target.chars().all(|c| c.is_ascii_digit()) && !target.is_empty() {
        format!("127.0.0.1:{target}")
    } else {
        target.to_owned()
    };

    let addresses: Vec<SocketAddr> = target
        .to_socket_addrs()
        .map_err(|error| format!("`{target}` is not an address this can reach: {error}"))?
        .collect();
    let Some(first) = addresses.first().copied() else {
        return Err(format!("`{target}` resolved to no address at all"));
    };
    if let Some(stray) = addresses.iter().find(|address| !address.ip().is_loopback()) {
        return Err(format!(
            "`{target}` is {}, which is not on this machine's loopback. deco forwards \
             loopback ports only: anything else would make this server a way into the \
             network it sits in",
            stray.ip()
        ));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_port_means_the_same_port_at_both_ends() {
        assert_eq!(
            PortSpec::parse("3000").expect("a spec"),
            PortSpec {
                local: 3000,
                remote: 3000
            }
        );
        assert_eq!(
            PortSpec::parse("8080:3000").expect("a spec"),
            PortSpec {
                local: 8080,
                remote: 3000
            }
        );
    }

    #[test]
    fn a_port_that_is_not_one_is_refused_by_name() {
        assert_eq!(
            PortSpec::parse("http"),
            Err(PortSpecError::NotAPort {
                value: "http".to_owned()
            })
        );
        // 65536 is out of range and is reported as an error rather than
        // wrapping to 0.
        assert_eq!(
            PortSpec::parse("65536"),
            Err(PortSpecError::NotAPort {
                value: "65536".to_owned()
            })
        );
        assert_eq!(PortSpec::parse("0"), Err(PortSpecError::Zero));
        assert_eq!(PortSpec::parse("8080:0"), Err(PortSpecError::Zero));
        assert_eq!(
            PortSpec::parse("1:2:3"),
            Err(PortSpecError::Shape {
                value: "1:2:3".to_owned()
            })
        );
    }

    #[test]
    fn the_remote_command_names_a_loopback_address() {
        // A full address rather than a bare port. The remote's error message
        // quotes it, and `ps` on the remote shows it.
        assert_eq!(
            forward_command("deco", 3000),
            ["deco", "--forward-to", "127.0.0.1:3000", "--stdio"]
        );
    }

    #[test]
    fn only_loopback_is_reachable_from_a_forward() {
        assert_eq!(
            resolve_loopback("127.0.0.1:3000").expect("loopback"),
            "127.0.0.1:3000".parse::<SocketAddr>().expect("an address")
        );
        assert_eq!(
            resolve_loopback("3000").expect("a bare port"),
            "127.0.0.1:3000".parse::<SocketAddr>().expect("an address")
        );

        // A remote deco that connected here would give access to the remote's
        // network.
        let error = resolve_loopback("10.0.0.5:5432").expect_err("a refusal");
        assert!(error.contains("loopback"), "{error}");
        assert!(error.contains("10.0.0.5"), "{error}");
    }

    #[test]
    fn a_forward_listens_on_loopback_and_nowhere_else() {
        // No other test catches a change of the bind address to `0.0.0.0`.
        // Binding anywhere other than loopback exposes the remote service to
        // every machine that can route to this one.
        let forward = Forward::start(
            Command {
                program: "true".to_owned(),
                args: Vec::new(),
            },
            PortSpec {
                local: 0,
                remote: 3000,
            },
        )
        .expect("a listener");
        assert!(
            forward.address().ip().is_loopback(),
            "{}",
            forward.address()
        );
    }

    #[test]
    fn a_forward_stops_listening_when_it_is_dropped() {
        // Otherwise the port stays held after the session ends, until the
        // process exits, and the next `--forward 3000` fails.
        let forward = Forward::start(
            Command {
                program: "true".to_owned(),
                args: Vec::new(),
            },
            PortSpec {
                local: 0,
                remote: 3000,
            },
        )
        .expect("a listener");
        let address = forward.address();
        assert!(TcpStream::connect(address).is_ok());
        drop(forward);

        // The accept loop takes a moment to stop. Polling avoids a fixed sleep
        // that would be either flaky or slow.
        let freed = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            TcpListener::bind(address).is_ok()
        });
        assert!(freed, "the port is still held");
    }

    #[test]
    fn a_port_already_in_use_says_so_rather_than_silently_not_forwarding() {
        let held = TcpListener::bind(("127.0.0.1", 0)).expect("a listener");
        let port = held.local_addr().expect("an address").port();
        let error = Forward::start(
            Command {
                program: "true".to_owned(),
                args: Vec::new(),
            },
            PortSpec {
                local: port,
                remote: 3000,
            },
        )
        .expect_err("a refusal");
        assert!(error.to_string().contains(&port.to_string()), "{error}");
    }
}
