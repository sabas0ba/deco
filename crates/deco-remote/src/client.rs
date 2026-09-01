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
use std::path::{Path, PathBuf};
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

impl Handshake {
    /// Whether the server answers `method`.
    ///
    /// The handshake lists what a server has so a client need not discover it
    /// by being refused — which matters once a refusal and an ordinary "no"
    /// would look the same. `settings.read` on a server that predates it is
    /// the case: asking and catching the refusal would also swallow a genuine
    /// failure to read the file.
    pub fn serves(&self, method: &str) -> bool {
        self.methods.iter().any(|known| known == method)
    }
}

/// One place a search term was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The file, relative to the workspace the server serves.
    pub path: String,
    /// Zero-based line.
    pub line: u32,
    /// Zero-based character within the line.
    pub character: u32,
    /// The line's text, trimmed and cut by the server.
    pub text: String,
}

impl Match {
    /// Reads one match, or nothing if the server sent something else.
    ///
    /// Skipped rather than failing the whole search: one malformed entry in five
    /// hundred is not a reason to show none of them.
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            path: value["path"].as_str()?.to_owned(),
            line: value["line"].as_u64()? as u32,
            character: value["character"].as_u64().unwrap_or(0) as u32,
            text: value["text"].as_str().unwrap_or_default().to_owned(),
        })
    }
}

/// What a search found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Search {
    /// The matches, in the order the server walked them.
    pub matches: Vec<Match>,
    /// Whether a limit stopped the search early.
    pub truncated: bool,
    /// How many files were read.
    pub files_searched: usize,
}

/// A connection to a server started over a transport.
pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// What the handshake said this server has, once it has been asked.
    ///
    /// Kept so that a caller which did not perform the handshake can still ask
    /// what the far end supports. Without it, "does this server have
    /// `settings.read`?" could only be answered by calling it and reading the
    /// refusal — which is indistinguishable from the file being unreadable.
    served: Vec<String>,
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
            served: Vec::new(),
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
        let hello = Handshake {
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
        };
        self.served.clone_from(&hello.methods);
        Ok(hello)
    }

    /// Whether the handshake said this server has `method`.
    ///
    /// False before the handshake, which is the safe direction: nothing optional
    /// should be attempted against a server that has not said what it is.
    pub fn serves(&self, method: &str) -> bool {
        self.served.iter().any(|known| known == method)
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

    /// What the remote says about one path.
    ///
    /// The value is passed through as the server sent it — VS Code's `FileStat`
    /// shape — because the only consumer is an extension expecting exactly that,
    /// and a type here would be a third spelling of the same four fields.
    pub fn stat(&mut self, path: &str) -> Result<Value, ClientError> {
        let said = self.request("fs.stat", json!({ "path": path }))?;
        said.get("stat").cloned().ok_or(ClientError::Malformed {
            method: "fs.stat".to_owned(),
            field: "stat",
        })
    }

    /// The remote machine's own settings, and the path they came from.
    ///
    /// `None` for the text when the machine has no `machine-settings.json`,
    /// which is the ordinary case — the path is still returned, so
    /// `--print-config` can say where the far end looked rather than leaving
    /// "why is my remote setting not applying" to guesswork.
    ///
    /// **What comes back is not trusted.** It is written where anyone with an
    /// account on that machine can write it, so it is loaded as
    /// [`Scope::Remote`] and everything that treats that scope as untrusted —
    /// the extension sandbox, and language-server definitions, which have to be
    /// confirmed — keeps doing so.
    ///
    /// Ask [`Handshake::serves`] before calling this: a server too old to know
    /// the method would refuse it, and a refusal here is a real failure worth
    /// reporting rather than something to read as "no settings".
    ///
    /// [`Scope::Remote`]: https://docs.rs/deco-config
    pub fn machine_settings(&mut self) -> Result<(String, Option<String>), ClientError> {
        let said = self.request("settings.read", json!({}))?;
        Ok((
            said["path"].as_str().unwrap_or_default().to_owned(),
            said["text"].as_str().map(str::to_owned),
        ))
    }

    /// What is directly inside one directory on the remote.
    ///
    /// Not [`Client::list`], which walks the whole workspace for quick open. This
    /// is one level, which is what a `readDirectory` means.
    pub fn read_directory(&mut self, path: &str) -> Result<Vec<(String, u32)>, ClientError> {
        let said = self.request("fs.dir", json!({ "path": path }))?;
        Ok(said["entries"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|entry| {
                        Some((
                            entry["name"].as_str()?.to_owned(),
                            entry["kind"].as_u64().unwrap_or(0) as u32,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Creates a directory on the remote, and any parent it needs.
    pub fn create_directory(&mut self, path: &str) -> Result<(), ClientError> {
        self.request("fs.mkdir", json!({ "path": path }))?;
        Ok(())
    }

    /// Removes a path on the remote.
    ///
    /// `recursive` is the caller's own word, passed through: without it a
    /// directory with anything in it is refused by the operating system, which is
    /// the distinction between "delete this" and "delete everything under this".
    pub fn delete(&mut self, path: &str, recursive: bool) -> Result<(), ClientError> {
        self.request("fs.delete", json!({ "path": path, "recursive": recursive }))?;
        Ok(())
    }

    /// Moves or copies one path to another, both on the remote.
    pub fn transfer(&mut self, source: &str, target: &str, copy: bool) -> Result<(), ClientError> {
        let method = if copy { "fs.copy" } else { "fs.rename" };
        self.request(method, json!({ "source": source, "target": target }))?;
        Ok(())
    }

    /// Searches the remote's workspace for `needle`.
    ///
    /// The matching happens on the far end because the files do. What comes back
    /// is bounded by the server rather than by this end — see
    /// [`server::MAX_MATCHES`](crate::server::MAX_MATCHES) — so a workspace with
    /// a million occurrences cannot make it send them.
    pub fn search(
        &mut self,
        needle: &str,
        options: deco_core::search::SearchOptions,
    ) -> Result<Search, ClientError> {
        let said = self.request(
            "fs.search",
            json!({
                "needle": needle,
                "caseSensitive": options.case_sensitive,
                "wholeWord": options.whole_word,
            }),
        )?;
        let matches = said["matches"]
            .as_array()
            .map(|values| values.iter().filter_map(Match::from_json).collect())
            .unwrap_or_default();
        Ok(Search {
            matches,
            truncated: said["truncated"].as_bool().unwrap_or(false),
            files_searched: said["filesSearched"].as_u64().unwrap_or(0) as usize,
        })
    }

    /// What git says about the repository on the far end.
    ///
    /// The root is returned with the status because every path in that status
    /// is relative to the repository rather than to the served workspace.
    pub fn scm_status(&mut self) -> Result<(PathBuf, deco_scm::Status), ClientError> {
        let said = self.request("scm.status", json!({}))?;
        let root = said["root"]
            .as_str()
            .map(PathBuf::from)
            .ok_or(ClientError::Malformed {
                method: "scm.status".to_owned(),
                field: "root",
            })?;
        let status =
            serde_json::from_value(said["status"].clone()).map_err(|_| ClientError::Malformed {
                method: "scm.status".to_owned(),
                field: "status",
            })?;
        Ok((root, status))
    }

    /// What `HEAD` held for one file on the far end.
    pub fn scm_committed(&mut self, path: &Path) -> Result<Option<String>, ClientError> {
        let said = self.request(
            "scm.committed",
            json!({ "path": path.display().to_string() }),
        )?;
        match said.get("text") {
            Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            _ => Err(ClientError::Malformed {
                method: "scm.committed".to_owned(),
                field: "text",
            }),
        }
    }

    /// Both repository states needed for one source-control diff on the far end.
    pub fn scm_comparison(
        &mut self,
        request: &deco_scm::ComparisonRequest,
    ) -> Result<deco_scm::Comparison, ClientError> {
        let said = self.request("scm.comparison", json!({ "request": request }))?;
        serde_json::from_value(said["comparison"].clone()).map_err(|_| ClientError::Malformed {
            method: "scm.comparison".to_owned(),
            field: "comparison",
        })
    }

    /// Carries out one source-control operation on the far end.
    pub fn scm_apply(&mut self, operation: &deco_scm::Operation) -> Result<(), ClientError> {
        self.request("scm.apply", json!({ "operation": operation }))?;
        Ok(())
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
