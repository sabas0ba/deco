//! The near end: starting a server over a transport and talking to it.
//!
//! [`transport`](crate::transport) builds the command, [`server`](crate::server)
//! answers it, and this is what runs the one and calls the other. It is
//! deliberately the smallest thing that can open and save a file:
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use deco_remote::{client::Client, command_for, server_command, Authority, TransportOptions};
//!
//! let authority = Authority::parse("ssh-remote+myhost")?;
//! let command = command_for(
//!     &authority,
//!     &server_command("deco", Some("/home/u/project")),
//!     &TransportOptions::default(),
//! )?;
//! let mut client = Client::start(&command)?;
//! let hello = client.handshake()?;
//! let text = client.read("src/main.rs")?;
//! # let _ = (hello, text);
//! # Ok(())
//! # }
//! ```
//!
//! # Why this blocks
//!
//! Every call here waits for its reply. The server never speaks first — there are
//! no notifications in this protocol — so a request is always answered by the
//! next frame, and a reader thread would buy nothing but a channel to wait on.
//!
//! What it costs is honest to state: a file opened over a slow link holds the
//! editor for as long as the link takes. That is acceptable for opening and
//! saving, which are the two things a person waits for anyway, and it is not
//! acceptable for anything on a keystroke path — so nothing here is on one.
//! `TransportOptions` sets an SSH connect timeout, which is what stops an
//! unreachable host from hanging forever.

use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command as OsCommand, Stdio};

use serde_json::{json, Value};

use crate::frame::{self, Message};
use crate::server::{HANDSHAKE, PROTOCOL_VERSION};
use crate::transport::Command;

/// What went wrong talking to a server.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The transport command could not be started at all.
    #[error("could not start `{program}`: {error}")]
    Start {
        /// The program that was tried.
        program: String,
        /// What the operating system said.
        error: std::io::Error,
    },
    /// The connection failed while a request was in flight.
    #[error("the connection to the server failed: {0}")]
    Connection(#[from] frame::FrameError),
    /// The server closed without answering.
    ///
    /// The usual cause is the far end not having a `deco` at all, so the
    /// transport ran something that printed to stderr and exited.
    #[error("the server stopped without answering{}", .stderr.as_ref().map(|e| format!("; it said: {e}")).unwrap_or_default())]
    Closed {
        /// Whatever the far end put on stderr, if anything.
        stderr: Option<String>,
    },
    /// The server refused the request.
    #[error("{0}")]
    Refused(String),
    /// The reply was not the shape this version expects.
    #[error("the server's answer to {method} had no {field}")]
    Malformed {
        /// The method that was called.
        method: String,
        /// What was missing.
        field: &'static str,
    },
    /// The two ends do not speak the same protocol.
    #[error(
        "this deco speaks remote protocol {PROTOCOL_VERSION} and the server speaks \
         {theirs}; update whichever is older"
    )]
    Protocol {
        /// The version the server named.
        theirs: String,
    },
}

/// What a server said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// The directory it serves, as *it* spells it.
    pub workspace: String,
    /// The methods it says it has.
    pub methods: Vec<String>,
}

/// A connection to a server started over a transport.
pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    /// Starts `command` and connects to it over its stdin and stdout.
    ///
    /// Stderr is left inherited on purpose: `ssh` writes its own diagnostics
    /// there — host key prompts, "permission denied", "connection refused" — and
    /// swallowing them would turn every connection problem into deco's vaguest
    /// error message.
    pub fn start(command: &Command) -> Result<Self, ClientError> {
        let mut child = OsCommand::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| ClientError::Start {
                program: command.program.clone(),
                error,
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or(ClientError::Closed { stderr: None })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ClientError::Closed { stderr: None })?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    /// Sends a request and waits for its reply.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        frame::write(
            &mut self.stdin,
            &Message::Request {
                id,
                method: method.to_owned(),
                params,
            },
        )?;

        loop {
            let Some(message) = frame::read(&mut self.stdout)? else {
                return Err(ClientError::Closed { stderr: None });
            };
            match message {
                // Answering the request that was asked. A reply to some other id
                // cannot happen in this protocol, but reading on rather than
                // trusting that keeps a future server that pipelines from
                // confusing this one.
                Message::Response {
                    id: answered,
                    result,
                    error,
                } if answered == id => {
                    return match (result, error) {
                        (_, Some(error)) => Err(ClientError::Refused(error)),
                        (Some(value), None) => Ok(value),
                        (None, None) => Ok(Value::Null),
                    };
                }
                _ => continue,
            }
        }
    }

    /// Asks what the server is, and refuses a version this deco does not speak.
    ///
    /// Called once, before anything else: a protocol mismatch found here is a
    /// sentence about versions, and the same mismatch found later is a file that
    /// mysteriously will not open.
    pub fn handshake(&mut self) -> Result<Handshake, ClientError> {
        let said = self.request(HANDSHAKE, json!({}))?;
        let theirs = said["protocol"]
            .as_str()
            .ok_or(ClientError::Malformed {
                method: HANDSHAKE.to_owned(),
                field: "protocol",
            })?
            .to_owned();
        if theirs != PROTOCOL_VERSION {
            return Err(ClientError::Protocol { theirs });
        }
        Ok(Handshake {
            workspace: said["workspace"].as_str().unwrap_or_default().to_owned(),
            methods: said["methods"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Reads a file from the server's workspace.
    pub fn read(&mut self, path: &str) -> Result<String, ClientError> {
        let said = self.request("fs.read", json!({ "path": path }))?;
        said["text"]
            .as_str()
            .map(str::to_owned)
            .ok_or(ClientError::Malformed {
                method: "fs.read".to_owned(),
                field: "text",
            })
    }

    /// Writes a file into the server's workspace.
    pub fn write(&mut self, path: &str, text: &str) -> Result<(), ClientError> {
        self.request("fs.write", json!({ "path": path, "text": text }))?;
        Ok(())
    }

    /// Lists the server's workspace, as paths relative to its root.
    pub fn list(&mut self) -> Result<Vec<String>, ClientError> {
        let said = self.request("fs.list", json!({}))?;
        Ok(said["files"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Asks the server to stop, then waits for it briefly.
    ///
    /// Errors are dropped: this runs while the editor is quitting, and there is
    /// nobody left to tell. What matters is that the far end is asked rather than
    /// left holding a workspace open on someone else's machine.
    pub fn shutdown(&mut self) {
        let _ = self.request("$/shutdown", json!({}));
        // Closing stdin is what a server that never got the message will notice.
        let _ = self.stdin.flush();
        let _ = self.child.wait();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_that_does_not_exist_says_which_one() {
        // Matched rather than unwrapped: a `Client` owns a child process and has
        // no `Debug`, so `expect_err` cannot print one.
        match Client::start(&Command {
            program: "deco-no-such-transport".to_owned(),
            args: Vec::new(),
        }) {
            Err(error) => assert!(
                error.to_string().contains("deco-no-such-transport"),
                "{error}"
            ),
            Ok(_) => panic!("nothing should have started"),
        }
    }

    #[test]
    fn a_protocol_mismatch_names_both_versions() {
        // Constructed rather than provoked: there is only one version of this
        // protocol so far, and the check has to be right before there is a second.
        let error = ClientError::Protocol {
            theirs: "99".to_owned(),
        };
        let said = error.to_string();
        assert!(said.contains(PROTOCOL_VERSION), "{said}");
        assert!(said.contains("99"), "{said}");
        assert!(said.contains("update"), "{said}");
    }

    #[test]
    fn a_server_that_says_nothing_is_a_closed_connection_and_not_a_hang() {
        let error = ClientError::Closed { stderr: None };
        assert!(error.to_string().contains("stopped without answering"));
        let error = ClientError::Closed {
            stderr: Some("ssh: connect to host x port 22: Connection refused".to_owned()),
        };
        // The transport's own diagnostic is what a person needs here, so it is
        // carried rather than replaced.
        assert!(error.to_string().contains("Connection refused"));
    }

    /// The rest of the tests need a server binary to talk to, which only the
    /// `deco` crate builds. They live in `crates/deco/tests/remote_session.rs`
    /// for that reason: a test that cannot name its counterpart is a test that
    /// mocks it, and the whole point here is not mocking it.
    #[test]
    fn the_client_is_exercised_against_a_real_server_elsewhere() {
        assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deco/tests/remote_session.rs")
            .exists());
    }
}
