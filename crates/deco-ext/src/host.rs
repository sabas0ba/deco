//! Starting and talking to the Node extension host process.
//!
//! # Defence in depth
//!
//! Three independent layers stand between an extension and the machine, so that
//! a bug in any one of them is not on its own sufficient:
//!
//! 1. **Node's permission model** (`--permission`, Node 22.13+) blocks filesystem,
//!    child-process and worker access at the runtime level. This is the only
//!    layer the extension cannot talk its way around, because it is enforced
//!    below JavaScript.
//! 2. **The host bootstrap** removes the network globals and refuses to load the
//!    `fs`, `net`, `http`, `child_process` and related built-ins, so the failure
//!    mode is a clear error rather than a permission trap.
//! 3. **The capability broker** in deco checks every brokered request that does
//!    get through, which is where the user's actual consent lives.
//!
//! Node's permission model does not cover the network, which is why layer 2 is
//! not redundant.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use crate::protocol::Message;

/// Resource bounds applied to the host process.
#[derive(Debug, Clone, Copy)]
pub struct HostLimits {
    /// V8 heap cap in megabytes.
    pub max_old_space_mb: u64,
    /// How long to wait for the host's `$/ready` before giving up.
    pub startup_timeout_ms: u64,
    /// How many requests may be in flight before the host is considered stuck.
    pub max_pending_requests: usize,
}

impl Default for HostLimits {
    fn default() -> Self {
        Self {
            max_old_space_mb: 512,
            startup_timeout_ms: 10_000,
            max_pending_requests: 1024,
        }
    }
}

/// Everything needed to start a host process.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Absolute path to the `node` binary.
    pub node: PathBuf,
    /// Absolute path to deco's host bootstrap script.
    pub bootstrap: PathBuf,
    /// Directories the host is allowed to read: its own code and the installed
    /// extensions. Nothing else is readable, including the user's home.
    pub readable_roots: Vec<PathBuf>,
    /// Working directory for the host.
    pub cwd: PathBuf,
    /// Resource bounds.
    pub limits: HostLimits,
    /// Whether to pass `--permission`. Requires Node 22.13 or newer, where the
    /// permission model became stable and the flag lost its `--experimental-`
    /// prefix; older Node rejects the flag outright, so callers that support
    /// older runtimes must turn it off and rely on layers 2 and 3 alone.
    pub node_permission_model: bool,
    /// Whether extensions may use `eval` and `new Function`.
    ///
    /// Off by default. Some bundled extensions do need it, so it is a setting
    /// rather than a hard rule — but it is off unless asked for, because code
    /// generated at runtime is exactly what a supply-chain payload uses.
    pub allow_code_generation: bool,
}

/// A resolved command line for the host process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec {
    /// The program to run.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// The **complete** environment. The parent's environment is not inherited.
    pub env: BTreeMap<String, String>,
    /// Working directory.
    pub cwd: PathBuf,
}

/// Builds the command line for a host process.
///
/// The environment is constructed from nothing rather than filtered from the
/// parent's. A denylist would need updating every time a new tool invents a
/// `*_TOKEN` variable; an allowlist of three entries does not.
pub fn build_spec(config: &HostConfig, extension_id: &str) -> HostSpec {
    let mut args: Vec<String> = Vec::new();

    args.push(format!(
        "--max-old-space-size={}",
        config.limits.max_old_space_mb
    ));

    if !config.allow_code_generation {
        args.push("--disallow-code-generation-from-strings".to_owned());
    }

    if config.node_permission_model {
        args.push("--permission".to_owned());
        // No --allow-child-process and no --allow-worker: spawning is brokered
        // through deco or it does not happen.
        for root in &config.readable_roots {
            args.push(format!("--allow-fs-read={}", root.display()));
        }
    }

    args.push(config.bootstrap.display().to_string());

    let mut env = BTreeMap::new();
    env.insert("DECO_EXTENSION_ID".to_owned(), extension_id.to_owned());
    env.insert("DECO_HOST_PROTOCOL".to_owned(), PROTOCOL_VERSION.to_owned());
    // Node refuses to start on Windows without this one.
    if cfg!(windows) {
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            env.insert(
                "SystemRoot".to_owned(),
                system_root.to_string_lossy().into_owned(),
            );
        }
    }

    HostSpec {
        program: config.node.clone(),
        args,
        env,
        cwd: config.cwd.clone(),
    }
}

/// The protocol version deco speaks. The host refuses to start if it does not
/// match, so a stale bootstrap script fails loudly instead of subtly.
pub const PROTOCOL_VERSION: &str = "1";

/// A line-delimited JSON connection to a host process.
///
/// Generic over its streams so the whole request/response path can be tested
/// against in-memory buffers rather than a real process.
pub struct Connection<R: Read, W: Write> {
    reader: BufReader<R>,
    writer: W,
}

impl<R: Read, W: Write> Connection<R, W> {
    /// Wraps a reader and writer.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    /// Sends a message.
    pub fn send(&mut self, message: &Message) -> std::io::Result<()> {
        self.writer.write_all(message.encode().as_bytes())?;
        self.writer.flush()
    }

    /// Reads the next message, or `None` at end of stream.
    ///
    /// A line that is not valid JSON is skipped rather than fatal: the host's
    /// stdout can pick up stray output from a misbehaving extension, and
    /// dropping the connection over it would be a denial of service any
    /// extension could trigger with one `console.log`.
    pub fn receive(&mut self) -> std::io::Result<Option<Message>> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            if line.trim().is_empty() {
                continue;
            }
            match Message::decode(&line) {
                Ok(message) => return Ok(Some(message)),
                Err(_) => continue,
            }
        }
    }
}

/// Failure to start a host.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The node binary was not where it was expected.
    #[error("node was not found at {path}")]
    NodeNotFound {
        /// The path that was tried.
        path: PathBuf,
    },
    /// The bootstrap script was missing.
    #[error("the extension host bootstrap was not found at {path}")]
    BootstrapNotFound {
        /// The path that was tried.
        path: PathBuf,
    },
    /// The process could not be started.
    #[error("could not start the extension host: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Checks that everything [`build_spec`] refers to actually exists.
///
/// Called before spawning so a missing runtime is reported as a clear message
/// rather than as an opaque failure from the OS.
pub fn verify(config: &HostConfig) -> Result<(), HostError> {
    if !config.node.exists() {
        return Err(HostError::NodeNotFound {
            path: config.node.clone(),
        });
    }
    if !config.bootstrap.exists() {
        return Err(HostError::BootstrapNotFound {
            path: config.bootstrap.clone(),
        });
    }
    Ok(())
}

/// Starts a host process with stdin and stdout piped.
pub fn spawn(config: &HostConfig, extension_id: &str) -> Result<std::process::Child, HostError> {
    verify(config)?;
    let spec = build_spec(config, extension_id);

    let mut command = std::process::Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // `env_clear` is what makes the allowlist above meaningful; without it
        // the inserts below would merely add to an inherited environment.
        .env_clear();
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    Ok(command.spawn()?)
}

/// Whether `path` looks like it is inside one of `roots`.
pub fn within_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|root| crate::capability::is_within(path, root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Request, Response};
    use serde_json::json;

    fn config() -> HostConfig {
        HostConfig {
            node: PathBuf::from("/usr/bin/node"),
            bootstrap: PathBuf::from("/opt/deco/host/bootstrap.js"),
            readable_roots: vec![
                PathBuf::from("/opt/deco/host"),
                PathBuf::from("/home/u/.config/deco/extensions/acme.ext"),
            ],
            cwd: PathBuf::from("/home/u/.config/deco/extensions/acme.ext"),
            limits: HostLimits::default(),
            node_permission_model: true,
            allow_code_generation: false,
        }
    }

    #[test]
    fn the_spec_caps_the_heap() {
        let spec = build_spec(&config(), "acme.ext");
        assert!(spec.args.iter().any(|a| a == "--max-old-space-size=512"));
    }

    #[test]
    fn the_spec_enables_nodes_permission_model_without_process_or_worker_access() {
        let spec = build_spec(&config(), "acme.ext");
        assert!(spec.args.iter().any(|a| a == "--permission"));
        assert!(spec
            .args
            .iter()
            .any(|a| a == "--allow-fs-read=/opt/deco/host"));
        assert!(
            !spec
                .args
                .iter()
                .any(|a| a.starts_with("--allow-child-process")),
            "the host must not be able to spawn processes directly"
        );
        assert!(!spec.args.iter().any(|a| a.starts_with("--allow-worker")));
        assert!(
            !spec.args.iter().any(|a| a.starts_with("--allow-fs-write")),
            "all writes go through the broker"
        );
    }

    #[test]
    fn code_generation_is_disabled_by_default_and_can_be_re_enabled() {
        let spec = build_spec(&config(), "acme.ext");
        assert!(spec
            .args
            .iter()
            .any(|a| a == "--disallow-code-generation-from-strings"));

        let mut config = config();
        config.allow_code_generation = true;
        let spec = build_spec(&config, "acme.ext");
        assert!(!spec
            .args
            .iter()
            .any(|a| a == "--disallow-code-generation-from-strings"));
    }

    #[test]
    fn the_permission_model_can_be_turned_off_for_older_node() {
        let mut config = config();
        config.node_permission_model = false;
        let spec = build_spec(&config, "acme.ext");
        assert!(!spec.args.iter().any(|a| a == "--permission"));
        // The heap cap is not part of the permission model, so it stays.
        assert!(spec
            .args
            .iter()
            .any(|a| a.starts_with("--max-old-space-size")));
    }

    #[test]
    fn the_bootstrap_is_the_last_argument() {
        let spec = build_spec(&config(), "acme.ext");
        assert_eq!(spec.args.last().unwrap(), "/opt/deco/host/bootstrap.js");
    }

    #[test]
    fn the_environment_is_built_from_nothing() {
        let spec = build_spec(&config(), "acme.ext");
        let expected: Vec<&str> = if cfg!(windows) {
            vec!["DECO_EXTENSION_ID", "DECO_HOST_PROTOCOL", "SystemRoot"]
        } else {
            vec!["DECO_EXTENSION_ID", "DECO_HOST_PROTOCOL"]
        };
        for key in spec.env.keys() {
            assert!(
                expected.contains(&key.as_str()),
                "unexpected environment entry {key}"
            );
        }
        assert_eq!(
            spec.env.get("DECO_EXTENSION_ID").map(String::as_str),
            Some("acme.ext")
        );
    }

    #[test]
    fn the_environment_carries_no_credentials_from_the_parent() {
        // Set a variable of the shape secrets usually take and confirm it does
        // not reach the host.
        std::env::set_var("DECO_TEST_FAKE_TOKEN", "super-secret");
        let spec = build_spec(&config(), "acme.ext");
        assert!(!spec.env.contains_key("DECO_TEST_FAKE_TOKEN"));
        assert!(!spec.env.values().any(|v| v.contains("super-secret")));
        for leaky in [
            "HOME",
            "PATH",
            "SSH_AUTH_SOCK",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
        ] {
            assert!(
                !spec.env.contains_key(leaky),
                "{leaky} leaked into the host environment"
            );
        }
        std::env::remove_var("DECO_TEST_FAKE_TOKEN");
    }

    #[test]
    fn verify_reports_a_missing_runtime_clearly() {
        let mut config = config();
        config.node = PathBuf::from("/nonexistent/node");
        assert!(matches!(
            verify(&config),
            Err(HostError::NodeNotFound { .. })
        ));
    }

    #[test]
    fn a_connection_round_trips_messages() {
        let request = Message::Request(Request {
            id: 1,
            method: "fs.readFile".into(),
            params: json!({"path": "/w/a.txt"}),
        });
        let mut outbound: Vec<u8> = Vec::new();
        {
            let mut connection = Connection::new(&[][..], &mut outbound);
            connection.send(&request).unwrap();
        }
        let mut connection = Connection::new(&outbound[..], Vec::new());
        assert_eq!(connection.receive().unwrap(), Some(request));
        assert_eq!(connection.receive().unwrap(), None);
    }

    #[test]
    fn stray_output_between_messages_is_skipped() {
        // An extension calling console.log writes to the same stream.
        let mut input = String::new();
        input.push_str("this is not json\n");
        input.push('\n');
        input.push_str(&Message::Response(Response::ok(9, json!("ok"))).encode());

        let mut connection = Connection::new(input.as_bytes(), Vec::new());
        match connection.receive().unwrap() {
            Some(Message::Response(response)) => assert_eq!(response.id, 9),
            other => panic!("expected the response to survive, got {other:?}"),
        }
    }

    #[test]
    fn within_any_checks_every_root() {
        let roots = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        assert!(within_any(Path::new("/a/x"), &roots));
        assert!(within_any(Path::new("/b/y/z"), &roots));
        assert!(!within_any(Path::new("/c"), &roots));
        assert!(!within_any(Path::new("/a/../c"), &roots));
    }
}
