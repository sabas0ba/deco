//! Reaching a port on the remote from this machine.
//!
//! A dev server running on the remote's `:3000` is not reachable from here —
//! that is the whole problem, and it is why every remote editor grows port
//! forwarding.
//!
//! # Why deco is its own tunnel
//!
//! `ssh -L` exists and does this well, and it is used by nothing here, because
//! it is available on exactly one of deco's three transports. `docker exec`
//! cannot forward a port at all, and a WSL distribution has no `-L` either. A
//! feature that worked over SSH and not over containers would be the kind of
//! half-thing this project keeps refusing to ship.
//!
//! So the remote's deco is the tunnel: [`forward_command`] runs
//! `deco --forward-to 127.0.0.1:3000 --stdio`, which connects to that port and
//! pipes it to its own stdin and stdout. Every transport can already carry a
//! program's stdio — that is how the file server works — so this works over all
//! three of them, with no `socat`, no `nc`, and nothing on the remote that deco
//! did not put there.
//!
//! The cost is a process per connection. Over SSH that would be an
//! authentication round-trip each time, which is why
//! [`TransportOptions::multiplex`](crate::TransportOptions) defaults on: with a
//! control socket the second connection onwards is local work.
//!
//! # What it will not reach
//!
//! Only loopback addresses on the remote. `deco --forward-to 10.0.0.5:5432`
//! is refused, because a deco that connects anywhere its host can reach is a
//! proxy into the remote's private network, and that is an authority nobody
//! asked it to have. It is the same rule the file server follows about paths.
//!
//! The near end listens on loopback too, and that is the more important half:
//! binding `0.0.0.0` would put the remote's database on this machine's network.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crate::transport::Command;

/// Which port here stands for which port there.
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
    /// A number was not a number, or was out of range.
    #[error("`{value}` is not a port number")]
    NotAPort {
        /// What was written.
        value: String,
    },
    /// Port 0 means "any free port", which is not a thing to forward.
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
    /// A bare number means the same port at both ends, which is what a person
    /// means nine times out of ten and is what VS Code shows by default.
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
        // Spelled out rather than passed as a bare port so that the remote's
        // refusal has an address to name, and so that the argument means the
        // same thing read from a process list.
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
/// Lives until it is dropped, which is what stops the listener: a forward is
/// tied to the session that asked for it rather than to the process.
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
    /// process — a single pipe cannot carry two conversations without a protocol
    /// on top, and the point of this design is that there is no protocol on top.
    pub fn start(command: Command, spec: PortSpec) -> Result<Self, ForwardError> {
        // Loopback, never `0.0.0.0`: the far end of this is a service on someone
        // else's machine, and putting it on this machine's network is not
        // something a person asked for by typing a port number.
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
                // Detached: a connection outlives the accept loop's interest in
                // it, and there is nothing to join — when the socket closes the
                // thread ends.
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

    /// What this forward is, for saying so.
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
        // The accept loop is blocked in `accept`, and a flag alone will not wake
        // it. Connecting to it does, and the loop then sees the flag and stops.
        let _ = TcpStream::connect(self.address);
    }
}

/// Copies `from` into `to`, flushing every chunk.
///
/// [`std::io::copy`] is the obvious thing to write here and is wrong at one end
/// of this: a process's stdout is line buffered, so a reply with no newline in
/// it — which is most of what a socket carries — sits in the buffer while the
/// client waits for it. A tunnel cannot hold bytes back until it sees a line.
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
        // Inherited for the reason the client's is: `ssh` writes its diagnosis
        // there, and swallowing it would turn every failure into silence.
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut to_remote = child.stdin.take().expect("stdin was piped");
    let mut from_remote = child.stdout.take().expect("stdout was piped");
    let mut from_client = stream.try_clone()?;
    let mut to_client = stream;

    let upstream = thread::spawn(move || {
        let _ = pipe(&mut from_client, &mut to_remote);
        // Dropped so the remote sees end-of-input and closes its own socket,
        // rather than both ends waiting for the other.
        drop(to_remote);
    });

    let _ = pipe(&mut from_remote, &mut to_client);
    // Whichever direction ended, the connection is over. Shutting the socket
    // down is what unblocks the thread above if it is still reading from it.
    let _ = to_client.shutdown(Shutdown::Both);
    let _ = child.kill();
    let _ = child.wait();
    let _ = upstream.join();
    Ok(())
}

/// Resolves what `--forward-to` was given, refusing anything not loopback.
///
/// This runs on the *remote*, and is the rule that keeps a deco server from
/// being a way into the network it sits in. A name is resolved first and then
/// every address it resolved to is checked, because `localhost` is only
/// loopback by convention — a remote's `/etc/hosts` can say otherwise, and a
/// check on the spelling rather than the address would miss it.
pub fn resolve_loopback(target: &str) -> Result<SocketAddr, String> {
    use std::net::ToSocketAddrs;

    // A bare port is the same shorthand `forward_command` avoids writing, taken
    // here because a person may well type it by hand.
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
        // 65536 does not fit in a port, and saying so beats wrapping to 0.
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
        // Not a bare port: the address is what the remote's refusal quotes, and
        // it is what someone reading `ps` on that machine sees.
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

        // The refusal that matters: a remote deco that dialled this would be a
        // route into whatever network the remote sits in.
        let error = resolve_loopback("10.0.0.5:5432").expect_err("a refusal");
        assert!(error.contains("loopback"), "{error}");
        assert!(error.contains("10.0.0.5"), "{error}");
    }

    #[test]
    fn a_forward_stops_listening_when_it_is_dropped() {
        // Otherwise a session that ends leaves the port held until the process
        // does, and the next `--forward 3000` fails for no visible reason.
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

        // The accept loop needs a moment to notice, and polling for it beats a
        // sleep that is either flaky or slow.
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
