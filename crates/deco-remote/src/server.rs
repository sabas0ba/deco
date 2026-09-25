//! The remote server: `deco --server --stdio`, running where the files are.
//!
//! [`crate::transport`] builds the command that starts this server; the command
//! ends in `deco --server --stdio`. This module implements the server: a loop
//! over [`crate::frame`] messages that answers a small set of methods against
//! one directory.
//!
//! # Confinement to one directory
//!
//! A server started with `--workspace /home/u/project` reads and writes only
//! inside that directory and rejects other paths with an error naming them.
//! This is stricter than VS Code, whose remote server opens any path the account
//! can reach. The reason is that deco does not authenticate the client itself;
//! the client is whatever is on the other end of the SSH connection. A frontend
//! bug, a hijacked session, or a `deco-remote://` link written by someone else
//! must not be able to read `~/.ssh/id_ed25519`.
//!
//! Confinement is checked on the **canonical** path, so a symlink inside the
//! workspace that points outside it is also rejected. Checking the path as
//! written would allow `project/link-to-etc/passwd`.
//!
//! ## Exception: machine settings
//!
//! `settings.read` returns a file outside the workspace: this machine's
//! `machine-settings.json`. It takes **no path**. A client cannot name a file;
//! it can only request this machine's settings, and receives the file at the
//! one path the server computes itself. The confinement rule limits which paths
//! a client can reach, and that set is unchanged: reads can still target only
//! the workspace directory.
//!
//! The server also does not apply the settings it returns. It resolves no
//! theme, starts no language server, and the file does not change how `fs.read`
//! behaves. It returns the bytes; the client decides what to do with them and
//! treats them as untrusted. The restriction is on the server applying the
//! settings, not on reading them.
//!
//! # Not yet implemented
//!
//! Extension hosts and file watching are not implemented. The methods below
//! cover opening, listing and saving files, source control on the machine that
//! holds the repository, and reading this machine's settings.

use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};

use deco_scm::{ComparisonRequest, Git, Operation, ScmError};
use serde_json::json;

use crate::frame::{self, Message};

/// The protocol version this server speaks.
///
/// Sent in the handshake and checked by the client, so a new frontend connected
/// to an old server fails with a clear error instead of partially working.
pub const PROTOCOL_VERSION: &str = "1";

/// The handshake method, sent first in every session.
pub const HANDSHAKE: &str = "$/handshake";

/// How many entries a listing will return.
///
/// The same limit as the local file walk. Listings feed a picker, which is not
/// usable with a hundred thousand files.
pub const MAX_LISTED: usize = 10_000;

/// The largest file the server will read or write.
///
/// Lower than the frame limit, because the text must fit in a frame together
/// with the surrounding JSON. Transferring a multi-gigabyte file over SSH would
/// not work in any case.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServerError {
    /// The path escapes the workspace.
    #[error("{path} is outside the workspace this server was started for")]
    OutsideWorkspace {
        /// The requested path, as written.
        path: String,
    },
    /// The path could not be resolved.
    #[error("{path} cannot be read: {reason}")]
    Unreadable {
        /// What was asked for.
        path: String,
        /// What the operating system said.
        reason: String,
    },
    /// The file is larger than the server will send.
    #[error("{path} is {size} bytes, over the {MAX_FILE_BYTES} byte limit")]
    TooLarge {
        /// What was asked for.
        path: String,
        /// Its size.
        size: u64,
    },
    /// A source-control comparison would not fit safely in one response frame.
    #[error("comparison for {path} is {size} bytes, over the {MAX_FILE_BYTES} byte limit")]
    ComparisonTooLarge {
        /// The repository-relative path being compared.
        path: String,
        /// The serialized comparison size.
        size: u64,
    },
    /// The file is not UTF-8.
    ///
    /// Rejected rather than decoded with replacement characters, which deco would
    /// write back on save and thereby corrupt the file.
    #[error("{path} is not valid UTF-8, and deco will not guess at it")]
    NotText {
        /// What was asked for.
        path: String,
    },
    /// The path is inside the workspace and could not be written.
    ///
    /// A separate variant from the read error, so that a failed write is not
    /// reported as "cannot be read" and misdirect diagnosis.
    #[error("{path} cannot be written: {reason}")]
    Unwritable {
        /// What was asked for.
        path: String,
        /// What the operating system said.
        reason: String,
    },
    /// The server does not implement the method.
    #[error("this server does not implement {method}")]
    UnknownMethod {
        /// What was asked for.
        method: String,
    },
    /// A parameter was missing or the wrong shape.
    #[error("{method} needs {what}")]
    BadParams {
        /// The method.
        method: String,
        /// What it wanted.
        what: String,
    },
    /// Git failed to answer or carry out a source-control request.
    #[error("source control is unavailable: {reason}")]
    SourceControl {
        /// What git said.
        reason: String,
    },
    /// The repository containing the workspace begins outside the directory
    /// this server is allowed to reach.
    #[error("the repository at {repository} begins outside the served workspace {workspace}")]
    RepositoryOutsideWorkspace {
        /// Where git said the repository begins.
        repository: String,
        /// The server's confinement boundary.
        workspace: String,
    },
    /// Git would keep index, refs, or objects outside the directory this
    /// server is allowed to change.
    #[error("Git metadata at {metadata} lies outside the served workspace {workspace}")]
    RepositoryMetadataOutsideWorkspace {
        /// The canonical administrative or common directory Git reported.
        metadata: String,
        /// The server's confinement boundary.
        workspace: String,
    },
    /// A source-control path is in the workspace but not in its repository.
    #[error("{path} is not inside the repository served by this session")]
    OutsideRepository {
        /// What was asked for.
        path: String,
    },
}

/// A server bound to one directory.
#[derive(Debug, Clone)]
pub struct Server {
    /// The canonical workspace root. Every path is resolved against it and must
    /// stay inside it.
    root: PathBuf,
    /// The file `settings.read` returns, determined at startup.
    ///
    /// Stored rather than computed per request so that the machine-settings path
    /// stays fixed for the lifetime of the connection. Otherwise a client could
    /// receive different paths within one session.
    machine_settings: Option<PathBuf>,
    /// Git on the machine that holds the files.
    git: Git,
}

impl Server {
    /// Binds a server to `root`.
    ///
    /// The root is canonicalised once here, so every later comparison is between
    /// two resolved paths. Comparing a resolved path with an unresolved root
    /// would let `..` and symlinks bypass the check.
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            root: root.as_ref().canonicalize()?,
            machine_settings: machine_settings_path(),
            git: Git::default(),
        })
    }

    /// The same server, serving `path` as its machine settings.
    ///
    /// For tests and embedders. The default path is read from the process
    /// environment, which a test cannot change without affecting other tests
    /// running in parallel.
    pub fn serving_machine_settings(mut self, path: Option<PathBuf>) -> Self {
        self.machine_settings = path;
        self
    }

    /// The directory this server serves.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a client-supplied path inside the workspace.
    ///
    /// Relative paths are resolved against the root, matching the paths a client
    /// receives from a listing. An absolute path is allowed only if it is inside
    /// the root. A path such as `/etc/passwd` is rejected with an error that
    /// states the reason.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ServerError> {
        let asked = Path::new(path);
        let joined = if asked.is_absolute() {
            asked.to_path_buf()
        } else {
            self.root.join(asked)
        };

        // Canonicalisation requires the file to exist, which is not the case for
        // a new file. In that case the parent is canonicalised instead, since a
        // file can only be created in an existing directory, and the file name
        // is appended afterwards.
        let (base, name) = match joined.file_name() {
            Some(name) if joined.exists() => (joined.clone(), Some(name.to_owned())),
            Some(name) => (
                joined.parent().unwrap_or(&self.root).to_path_buf(),
                Some(name.to_owned()),
            ),
            None => (joined.clone(), None),
        };
        let resolved = if joined.exists() {
            joined.canonicalize()
        } else {
            base.canonicalize()
        }
        .map_err(|error| ServerError::Unreadable {
            path: path.to_owned(),
            reason: error.to_string(),
        })?;

        let full = if joined.exists() {
            resolved
        } else {
            match name {
                Some(name) => resolved.join(name),
                None => resolved,
            }
        };

        // `starts_with` on components, not on text: `/home/u/project-secrets`
        // starts with the string `/home/u/project` and is not inside it.
        if !full.starts_with(&self.root) {
            return Err(ServerError::OutsideWorkspace {
                path: path.to_owned(),
            });
        }
        Ok(full)
    }

    /// The repository the served workspace belongs to, without weakening the
    /// server's one-directory boundary.
    ///
    /// Opening a subdirectory of a repository is useful locally, where the
    /// process already has the user's full filesystem access. A remote server
    /// must not reveal or change paths above the workspace it was given, so a
    /// repository that starts above the workspace is rejected instead of
    /// extending the session's access.
    fn repository_root(&self) -> Result<PathBuf, ServerError> {
        let repository = self
            .git
            .root(&self.root)
            .map_err(|error| ServerError::SourceControl {
                reason: error.to_string(),
            })?;
        if !repository.starts_with(&self.root) {
            return Err(ServerError::RepositoryOutsideWorkspace {
                repository: repository.display().to_string(),
                workspace: self.root.display().to_string(),
            });
        }
        let administrative = self
            .git
            .administrative_directories(&repository)
            .map_err(|error| ServerError::SourceControl {
                reason: error.to_string(),
            })?;
        for directory in administrative {
            let directory =
                directory
                    .canonicalize()
                    .map_err(|error| ServerError::SourceControl {
                        reason: format!("{} cannot be resolved: {error}", directory.display()),
                    })?;
            if !directory.starts_with(&self.root) {
                return Err(ServerError::RepositoryMetadataOutsideWorkspace {
                    metadata: directory.display().to_string(),
                    workspace: self.root.display().to_string(),
                });
            }
        }
        Ok(repository)
    }

    /// Resolves one client path and expresses it relative to `repository`.
    fn repository_path(&self, repository: &Path, asked: &str) -> Result<PathBuf, ServerError> {
        let resolved = self.resolve(asked)?;
        resolved
            .strip_prefix(repository)
            .map(Path::to_path_buf)
            .map_err(|_| ServerError::OutsideRepository {
                path: asked.to_owned(),
            })
    }

    /// Checks every path an operation names against the server boundary before
    /// handing the operation to git.
    fn confine_operation(
        &self,
        repository: &Path,
        operation: &Operation,
    ) -> Result<(), ServerError> {
        let paths: Vec<&Path> = match operation {
            Operation::Stage(path) => vec![path],
            Operation::Unstage { path, original } => {
                let mut paths = vec![path.as_path()];
                if let Some(original) = original {
                    paths.push(original);
                }
                paths
            }
            Operation::StageAll | Operation::Commit(_) | Operation::Checkout(_) => Vec::new(),
        };
        for relative in paths {
            let candidate = repository.join(relative);
            let text = candidate
                .to_str()
                .ok_or_else(|| ServerError::OutsideRepository {
                    path: relative.display().to_string(),
                })?;
            self.resolve(text)?;
        }
        Ok(())
    }

    /// Checks every path a comparison names before Git or the filesystem sees it.
    fn confine_comparison(
        &self,
        repository: &Path,
        request: &ComparisonRequest,
    ) -> Result<(), ServerError> {
        for relative in std::iter::once(request.path.as_path()).chain(request.original.as_deref()) {
            let candidate = repository.join(relative);
            let text = candidate
                .to_str()
                .ok_or_else(|| ServerError::OutsideRepository {
                    path: relative.display().to_string(),
                })?;
            self.named(text)?;
        }
        Ok(())
    }

    /// Answers one request.
    ///
    /// Returns the reply to send. A notification produces `None`. An
    /// unrecognised method produces an error reply rather than no reply, so the
    /// client does not wait indefinitely.
    pub fn handle(&mut self, message: Message) -> Option<Message> {
        let (id, method, params) = match message {
            Message::Request { id, method, params } => (id, method, params),
            // No notifications are handled yet, and replying to one would be a
            // protocol error.
            Message::Notification { .. } | Message::Response { .. } => return None,
        };

        let result = self.call(&method, &params);
        Some(match result {
            Ok(value) => Message::Response {
                id,
                result: Some(value),
                error: None,
            },
            Err(error) => Message::Response {
                id,
                result: None,
                error: Some(error.to_string()),
            },
        })
    }

    /// The methods themselves.
    fn call(
        &mut self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, ServerError> {
        let path = |what: &str| -> Result<String, ServerError> {
            params[what]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ServerError::BadParams {
                    method: method.to_owned(),
                    what: format!("a `{what}` string"),
                })
        };

        match method {
            HANDSHAKE => Ok(json!({
                "protocol": PROTOCOL_VERSION,
                "workspace": self.root.display().to_string(),
                // Supported methods, so a client does not have to probe for
                // them by sending requests.
                "methods": [
                    "fs.read",
                    "fs.write",
                    "fs.list",
                    "fs.search",
                    "fs.stat",
                    "fs.dir",
                    "fs.mkdir",
                    "fs.delete",
                    "fs.rename",
                    "fs.copy",
                    "settings.read",
                    "scm.status",
                    "scm.committed",
                    "scm.comparison",
                    "scm.branches",
                    "scm.checkoutPlan",
                    "scm.apply",
                    "$/shutdown"
                ],
            })),
            // Returns this machine's settings without applying them.
            //
            // This is the only method that reads a path outside the workspace.
            // It takes **no path**: a client cannot choose a file, only request
            // this machine's settings, and receives the file at the one path
            // this server computes. An unauthenticated client still cannot
            // access arbitrary files.
            //
            // The server does not apply what it reads. It does not resolve a
            // theme, start a language server, or change how `fs.read` behaves;
            // it returns the bytes and the client decides. The method is
            // allowed only because the server does not act on the file.
            //
            // The client treats the result as an untrusted layer, so a server
            // definition received this way still requires confirmation.
            "settings.read" => {
                let paths = self.machine_settings.clone();
                let path = paths
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                // A missing file returns `null` rather than an error, because
                // having no machine settings is normal. The path is returned in
                // both cases so `--print-config` can show where the server
                // looked.
                let text = match paths.as_ref().map(std::fs::read_to_string) {
                    Some(Ok(text)) => Some(text),
                    Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Some(Err(error)) => {
                        return Err(ServerError::Unreadable {
                            path: path.clone(),
                            reason: error.to_string(),
                        })
                    }
                    None => None,
                };
                Ok(json!({ "path": path, "text": text }))
            }
            "fs.read" => {
                let asked = path("path")?;
                let resolved = self.resolve(&asked)?;
                let size = std::fs::metadata(&resolved)
                    .map_err(|error| ServerError::Unreadable {
                        path: asked.clone(),
                        reason: error.to_string(),
                    })?
                    .len();
                if size > MAX_FILE_BYTES {
                    return Err(ServerError::TooLarge { path: asked, size });
                }
                let bytes = std::fs::read(&resolved).map_err(|error| ServerError::Unreadable {
                    path: asked.clone(),
                    reason: error.to_string(),
                })?;
                let text =
                    String::from_utf8(bytes).map_err(|_| ServerError::NotText { path: asked })?;
                Ok(json!({ "text": text }))
            }
            "fs.write" => {
                let asked = path("path")?;
                let text = params["text"]
                    .as_str()
                    .ok_or_else(|| ServerError::BadParams {
                        method: method.to_owned(),
                        what: "a `text` string".to_owned(),
                    })?;
                let resolved = self.resolve(&asked)?;
                std::fs::write(&resolved, text).map_err(|error| ServerError::Unwritable {
                    path: asked,
                    reason: error.to_string(),
                })?;
                Ok(json!({ "bytes": text.len() }))
            }
            "fs.list" => {
                let asked = params["path"].as_str().unwrap_or(".").to_owned();
                let resolved = self.resolve(&asked)?;
                Ok(json!({ "files": self.list(&resolved) }))
            }
            "fs.stat" => {
                let asked = path("path")?;
                let resolved = self.resolve(&asked)?;
                let metadata = std::fs::symlink_metadata(&resolved).map_err(|error| {
                    ServerError::Unreadable {
                        path: asked.clone(),
                        reason: error.to_string(),
                    }
                })?;
                Ok(json!({ "stat": stat_of(&metadata) }))
            }
            "fs.dir" => {
                let asked = path("path")?;
                let resolved = self.resolve(&asked)?;
                let entries =
                    std::fs::read_dir(&resolved).map_err(|error| ServerError::Unreadable {
                        path: asked.clone(),
                        reason: error.to_string(),
                    })?;
                let mut listed: Vec<serde_json::Value> = Vec::new();
                for entry in entries.flatten() {
                    if listed.len() >= MAX_LISTED {
                        break;
                    }
                    // `symlink_metadata`, so a link is reported as a link rather
                    // than as its target. The target may be outside the
                    // workspace, and the client should know that before
                    // following it.
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    listed.push(json!({
                        "name": entry.file_name().to_string_lossy(),
                        "kind": kind_of(&metadata),
                    }));
                }
                // Sorted because `read_dir` guarantees no order, and the result
                // should be stable between calls.
                listed.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                Ok(json!({ "entries": listed }))
            }
            "fs.mkdir" => {
                let asked = path("path")?;
                let resolved = self.resolve_for_creation(&asked)?;
                std::fs::create_dir_all(&resolved).map_err(|error| ServerError::Unwritable {
                    path: asked,
                    reason: error.to_string(),
                })?;
                Ok(json!({ "created": true }))
            }
            "fs.delete" => {
                let asked = path("path")?;
                // Confinement is checked first. Because `resolve` follows the
                // last component, this also rejects a link pointing outside the
                // workspace.
                self.resolve(&asked)?;
                // The path is then removed as named, not as resolved. For a
                // symbolic link inside the workspace these differ: deleting the
                // resolved path would remove the link's target and keep the link.
                let resolved = self.named(&asked)?;
                let recursive = params["recursive"].as_bool().unwrap_or(false);
                let metadata = std::fs::symlink_metadata(&resolved).map_err(|error| {
                    ServerError::Unreadable {
                        path: asked.clone(),
                        reason: error.to_string(),
                    }
                })?;
                // A non-empty directory is removed only when the caller sets
                // `recursive`. Without it, `remove_dir` is used, which fails for
                // a directory with contents.
                let outcome = if metadata.is_dir() && !metadata.is_symlink() {
                    if recursive {
                        std::fs::remove_dir_all(&resolved)
                    } else {
                        std::fs::remove_dir(&resolved)
                    }
                } else {
                    // A symbolic link is removed as a link and never followed.
                    // Its target may be outside the workspace, and deleting
                    // through the link would bypass confinement.
                    std::fs::remove_file(&resolved)
                };
                outcome.map_err(|error| ServerError::Unwritable {
                    path: asked,
                    reason: error.to_string(),
                })?;
                Ok(json!({ "deleted": true }))
            }
            "fs.rename" | "fs.copy" => {
                // Both source and target are resolved and therefore confined. A
                // source outside the workspace would import an outside file, and
                // a target outside would export a workspace file.
                let from = params["source"]
                    .as_str()
                    .ok_or_else(|| ServerError::BadParams {
                        method: method.to_owned(),
                        what: "a `source` string".to_owned(),
                    })?
                    .to_owned();
                let to = params["target"]
                    .as_str()
                    .ok_or_else(|| ServerError::BadParams {
                        method: method.to_owned(),
                        what: "a `target` string".to_owned(),
                    })?
                    .to_owned();
                let source = self.resolve(&from)?;
                let target = self.resolve(&to)?;
                let outcome = if method == "fs.rename" {
                    std::fs::rename(&source, &target).map(|()| 0)
                } else {
                    std::fs::copy(&source, &target)
                };
                outcome.map_err(|error| ServerError::Unwritable {
                    path: to,
                    reason: error.to_string(),
                })?;
                Ok(json!({ "moved": true }))
            }
            "fs.search" => {
                let needle = params["needle"]
                    .as_str()
                    .ok_or_else(|| ServerError::BadParams {
                        method: method.to_owned(),
                        what: "a `needle` string".to_owned(),
                    })?;
                let options = deco_core::search::SearchOptions {
                    case_sensitive: params["caseSensitive"].as_bool().unwrap_or(true),
                    whole_word: params["wholeWord"].as_bool().unwrap_or(false),
                };
                Ok(self.search(needle, options))
            }
            "scm.status" => {
                let repository = self.repository_root()?;
                let status =
                    self.git
                        .status(&repository)
                        .map_err(|error| ServerError::SourceControl {
                            reason: error.to_string(),
                        })?;
                Ok(json!({ "root": repository, "status": status }))
            }
            "scm.committed" => {
                let asked = path("path")?;
                let repository = self.repository_root()?;
                let relative = self.repository_path(&repository, &asked)?;
                let text = self
                    .git
                    .committed(&repository, &relative)
                    .map_err(|error| ServerError::SourceControl {
                        reason: error.to_string(),
                    })?;
                Ok(json!({ "text": text }))
            }
            "scm.comparison" => {
                let request: ComparisonRequest = serde_json::from_value(params["request"].clone())
                    .map_err(|_| ServerError::BadParams {
                        method: method.to_owned(),
                        what: "a `request` object".to_owned(),
                    })?;
                let repository = self.repository_root()?;
                self.confine_comparison(&repository, &request)?;
                let comparison = self
                    .git
                    .comparison(&repository, &request)
                    .map_err(|error| ServerError::SourceControl {
                        reason: error.to_string(),
                    })?;
                let size = serde_json::to_vec(&comparison)
                    .map_err(|error| ServerError::SourceControl {
                        reason: format!("comparison could not be serialized: {error}"),
                    })?
                    .len() as u64;
                if size > MAX_FILE_BYTES {
                    return Err(ServerError::ComparisonTooLarge {
                        path: request.path.display().to_string(),
                        size,
                    });
                }
                Ok(json!({ "comparison": comparison }))
            }
            "scm.branches" => {
                let repository = self.repository_root()?;
                let branches =
                    self.git
                        .branches(&repository)
                        .map_err(|error| ServerError::SourceControl {
                            reason: error.to_string(),
                        })?;
                Ok(json!({ "branches": branches }))
            }
            "scm.checkoutPlan" => {
                let target = params["target"]
                    .as_str()
                    .ok_or_else(|| ServerError::BadParams {
                        method: method.to_owned(),
                        what: "a `target` string".to_owned(),
                    })?;
                let repository = self.repository_root()?;
                let plan = self
                    .git
                    .checkout_plan(&repository, target)
                    .map_err(|error| ServerError::SourceControl {
                        reason: error.to_string(),
                    })?;
                Ok(json!({ "plan": plan }))
            }
            "scm.apply" => {
                let operation: Operation = serde_json::from_value(params["operation"].clone())
                    .map_err(|_| ServerError::BadParams {
                        method: method.to_owned(),
                        what: "an `operation` object".to_owned(),
                    })?;
                let repository = self.repository_root()?;
                self.confine_operation(&repository, &operation)?;
                self.git
                    .apply(&repository, &operation)
                    .map_err(|error: ScmError| ServerError::SourceControl {
                        reason: error.to_string(),
                    })?;
                Ok(json!({ "applied": true }))
            }
            "$/shutdown" => Ok(json!({ "stopping": true })),
            other => Err(ServerError::UnknownMethod {
                method: other.to_owned(),
            }),
        }
    }

    /// Every file under `from`, as paths relative to the workspace root.
    ///
    /// Paths are relative because the client displays and requests relative
    /// paths, and it does not need the remote's directory layout.
    fn list(&self, from: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![from.to_path_buf()];
        while let Some(directory) = stack.pop() {
            if found.len() >= MAX_LISTED {
                break;
            }
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            let mut here: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
            here.sort();
            for entry in here {
                if found.len() >= MAX_LISTED {
                    break;
                }
                let name = entry.file_name().unwrap_or_default().to_string_lossy();
                // The same entries the local walk skips, for the same reason:
                // files in `.git` are rarely opened directly, and walking it is
                // most of the cost of walking a repository.
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                if entry.is_dir() {
                    stack.push(entry);
                } else if let Ok(relative) = entry.strip_prefix(&self.root) {
                    found.push(slashed(relative));
                }
            }
        }
        found.sort();
        found
    }

    /// The path as it was named, with everything above the last component
    /// resolved and confined.
    ///
    /// [`Server::resolve`] returns the final target, which is correct for reading
    /// and writing but not for deleting: a link resolves to its target, while a
    /// delete refers to the link itself. This method canonicalises the parent,
    /// which prevents an intermediate link from leading outside, and appends the
    /// final name unchanged.
    fn named(&self, path: &str) -> Result<PathBuf, ServerError> {
        let asked = Path::new(path);
        let joined = if asked.is_absolute() {
            asked.to_path_buf()
        } else {
            self.root.join(asked)
        };
        let (Some(parent), Some(name)) = (joined.parent(), joined.file_name()) else {
            // No last component to preserve, so this is the same as `resolve`.
            return self.resolve(path);
        };
        let parent = parent
            .canonicalize()
            .map_err(|error| ServerError::Unreadable {
                path: path.to_owned(),
                reason: error.to_string(),
            })?;
        if !parent.starts_with(&self.root) {
            return Err(ServerError::OutsideWorkspace {
                path: path.to_owned(),
            });
        }
        Ok(parent.join(name))
    }

    /// Resolves a path that does not exist yet, and whose parents may not exist
    /// either.
    ///
    /// [`Server::resolve`] canonicalises the parent, which may not exist for a
    /// nested `createDirectory`. This method canonicalises and confines the
    /// deepest existing ancestor instead, and appends the missing tail to it.
    ///
    /// The tail cannot escape the workspace. The path is folded first, so each
    /// `..` is resolved against the preceding components and cannot remain in
    /// the part appended after the confinement check.
    fn resolve_for_creation(&self, path: &str) -> Result<PathBuf, ServerError> {
        let asked = Path::new(path);
        let joined = if asked.is_absolute() {
            asked.to_path_buf()
        } else {
            self.root.join(asked)
        };
        // Folded lexically: `a/../b` becomes `b`. A `..` with nothing before it
        // is kept, so the confinement check below rejects it.
        let mut folded = PathBuf::new();
        for component in joined.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if !folded.pop() {
                        folded.push(component);
                    }
                }
                other => folded.push(other),
            }
        }

        // Find the deepest existing ancestor, which can be canonicalised.
        let mut existing = folded.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !existing.exists() {
            let Some(name) = existing.file_name().map(|name| name.to_owned()) else {
                break;
            };
            tail.push(name);
            if !existing.pop() {
                break;
            }
        }
        let base = existing
            .canonicalize()
            .map_err(|error| ServerError::Unwritable {
                path: path.to_owned(),
                reason: error.to_string(),
            })?;
        if !base.starts_with(&self.root) {
            return Err(ServerError::OutsideWorkspace {
                path: path.to_owned(),
            });
        }
        let mut resolved = base;
        for name in tail.into_iter().rev() {
            resolved.push(name);
        }
        Ok(resolved)
    }

    /// Searches every file in the workspace for `needle`.
    ///
    /// The search runs on the server because the files are here. A client that
    /// searched its own disk in a remote session would search the wrong machine
    /// and report matches in files the editor is not showing; before this
    /// method existed, remote search was rejected for that reason.
    ///
    /// Synchronous and bounded, like the local search it replaces. Truncation is
    /// reported, so "500 matches" and "the first 500 of many" can be told apart.
    fn search(&self, needle: &str, options: deco_core::search::SearchOptions) -> serde_json::Value {
        let mut matches = Vec::new();
        let mut truncated = false;
        let mut files_searched = 0usize;
        if needle.is_empty() {
            return json!({ "matches": matches, "truncated": false, "filesSearched": 0 });
        }

        for relative in self.list(&self.root) {
            if matches.len() >= MAX_MATCHES {
                truncated = true;
                break;
            }
            let path = self.root.join(&relative);
            // Check the size first, so a huge file costs a `stat` rather than a
            // read.
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > MAX_SEARCHED_BYTES {
                continue;
            }
            // Files that are not UTF-8 are treated as binary and skipped; a
            // match inside a PNG is not a useful result.
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            files_searched += 1;

            let buffer = deco_core::buffer::Buffer::from_text(&text);
            for range in deco_core::search::find_all(&buffer, needle, options) {
                if matches.len() >= MAX_MATCHES {
                    truncated = true;
                    break;
                }
                let line = buffer
                    .line_content(range.start.line as usize)
                    .map(|line| line.to_string().trim().to_owned())
                    .unwrap_or_default();
                matches.push(json!({
                    "path": relative,
                    "line": range.start.line,
                    "character": range.start.character,
                    // Trimmed and truncated here rather than on the client, so
                    // the bytes are not sent. A minified line can be one match
                    // and a megabyte long.
                    "text": line.chars().take(200).collect::<String>(),
                }));
            }
        }
        json!({
            "matches": matches,
            "truncated": truncated,
            "filesSearched": files_searched,
        })
    }
}

/// How many matches a search reports before it stops.
///
/// The same limit as the editor's local search: a term with ten thousand
/// occurrences is not reviewed one by one. It is enforced by the server rather
/// than the client, because the server does not authenticate the client.
pub const MAX_MATCHES: usize = 500;

/// Largest file a search will read.
///
/// Much smaller than [`MAX_FILE_BYTES`], the limit for opening a file. Minified
/// bundles and checked-in databases are rarely what a project search is for, and
/// reading them would dominate the cost of the search.
pub const MAX_SEARCHED_BYTES: u64 = 1 << 20;

/// The path of this machine's settings for a connected session.
///
/// This is the only path in this file not derived from `--workspace`, and the
/// reason `settings.read` takes no argument: the server computes the path, so
/// the client cannot choose it. It is `machine-settings.json` rather than
/// `settings.json`; see [`deco_config::paths::ConfigPaths::machine_settings`]
/// for why the two are separate.
///
/// `None` when the environment lacks the variables needed to locate a home or
/// configuration directory. This is not an error; it is treated the same as
/// having no machine settings, which is the common case.
fn machine_settings_path() -> Option<PathBuf> {
    use deco_config::paths::{ConfigPaths, Env, Layout};
    ConfigPaths::deco(&Env::from_process(), Layout::host()).map(|paths| paths.machine_settings)
}

/// The type of a directory entry, using VS Code's `FileType` values.
///
/// `Unknown = 0`, `File = 1`, `Directory = 2`, `SymbolicLink = 64`. A link is
/// the sum, for example 65 for a link to a file. The protocol uses VS Code's
/// values directly because they are passed to VS Code's extension API, which
/// avoids a second translation.
fn kind_of(metadata: &std::fs::Metadata) -> u32 {
    let mut kind = if metadata.is_dir() { 2 } else { 1 };
    if metadata.is_symlink() {
        kind += 64;
    }
    kind
}

/// A file's stat, in the shape VS Code's `FileStat` has.
///
/// Times are in milliseconds since the epoch, as used by JavaScript. A time the
/// platform does not provide is reported as 0 rather than estimated, so an
/// extension comparing timestamps sees a clearly missing value instead of a
/// plausible wrong one.
fn stat_of(metadata: &std::fs::Metadata) -> serde_json::Value {
    let millis = |time: std::io::Result<std::time::SystemTime>| -> u64 {
        time.ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_millis() as u64)
            .unwrap_or(0)
    };
    json!({
        "type": kind_of(metadata),
        "ctime": millis(metadata.created()),
        "mtime": millis(metadata.modified()),
        "size": metadata.len(),
    })
}

/// A relative path with `/` separators, regardless of this platform's separator.
///
/// The wire format uses one separator, so a Windows client connected to a Linux
/// server does not have to guess which separator a path uses.
fn slashed(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Reads requests from `input` and writes replies to `output` until the stream
/// ends or `$/shutdown` is answered.
///
/// A frame that cannot be read at all ends the session: unlike the extension
/// host's line protocol, there is no way to resynchronise a length-prefixed
/// stream with an invalid length. Continuing could interpret payload bytes as
/// the next message header.
pub fn serve(
    input: &mut impl BufRead,
    output: &mut impl Write,
    server: &mut Server,
) -> Result<(), frame::FrameError> {
    loop {
        let Some(message) = frame::read(input)? else {
            return Ok(());
        };
        let stopping =
            matches!(&message, Message::Request { method, .. } if method == "$/shutdown");
        if let Some(reply) = server.handle(message) {
            frame::write(output, &reply)?;
        }
        if stopping {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A workspace with a file or two in it.
    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "deco-server-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("a directory");
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("a file");
        std::fs::write(root.join("README.md"), "# hello\n").expect("a file");
        root
    }

    fn request(id: u64, method: &str, params: serde_json::Value) -> Message {
        Message::Request {
            id,
            method: method.to_owned(),
            params,
        }
    }

    /// The result of a request, or the error string.
    fn ask(
        server: &mut Server,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match server.handle(request(1, method, params)) {
            Some(Message::Response {
                result: Some(value),
                ..
            }) => Ok(value),
            Some(Message::Response {
                error: Some(error), ..
            }) => Err(error),
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    /// A server whose machine settings are a file this test controls.
    fn with_machine_settings(root: &Path, name: &str, text: Option<&str>) -> Server {
        let path = root.join(format!("{name}-machine-settings.json"));
        match text {
            Some(text) => std::fs::write(&path, text).expect("a file"),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        Server::new(root)
            .expect("a server")
            .serving_machine_settings(Some(path))
    }

    #[test]
    fn machine_settings_are_handed_over_with_the_path_they_came_from() {
        let root = workspace("machine-settings");
        let mut server = with_machine_settings(&root, "present", Some(r#"{"a": 1}"#));
        let said = ask(&mut server, "settings.read", json!({})).expect("a reply");
        assert_eq!(said["text"], r#"{"a": 1}"#);
        // The path is also returned, so `--print-config` can show where the
        // remote side looked.
        assert!(
            said["path"]
                .as_str()
                .expect("a path")
                .ends_with("present-machine-settings.json"),
            "{said}"
        );
    }

    #[test]
    fn a_machine_with_no_settings_is_null_rather_than_an_error() {
        // The common case. An error here would make every connection to an
        // unconfigured machine appear to fail.
        let root = workspace("machine-settings-absent");
        let mut server = with_machine_settings(&root, "absent", None);
        let said = ask(&mut server, "settings.read", json!({})).expect("a reply");
        assert!(said["text"].is_null(), "{said}");
        assert!(!said["path"].as_str().expect("a path").is_empty());
    }

    #[test]
    fn a_server_with_nowhere_to_look_answers_the_same_as_one_with_nothing_there() {
        // No home directory means no configuration directory. This is not an
        // error; the server reports that there are no machine settings.
        let root = workspace("machine-settings-nowhere");
        let mut server = Server::new(&root)
            .expect("a server")
            .serving_machine_settings(None);
        let said = ask(&mut server, "settings.read", json!({})).expect("a reply");
        assert!(said["text"].is_null(), "{said}");
        assert_eq!(said["path"], "");
    }

    #[test]
    fn reading_machine_settings_takes_no_path_from_the_client() {
        // This method is allowed only because of this property. Every other
        // method is confined to the workspace; this one reads outside it, so the
        // client must not be able to choose the path. A `path` parameter is
        // ignored, and the file it names is outside the workspace.
        let root = workspace("machine-settings-unsteerable");
        let outside = root.join("..").join("secret.json");
        std::fs::write(&outside, r#"{"stolen": true}"#).expect("a file");
        let mut server = with_machine_settings(&root, "fixed", Some(r#"{"a": 1}"#));

        let said = ask(
            &mut server,
            "settings.read",
            json!({ "path": outside.display().to_string() }),
        )
        .expect("a reply");
        assert_eq!(said["text"], r#"{"a": 1}"#, "the client steered the read");
        assert!(said["path"]
            .as_str()
            .expect("a path")
            .ends_with("fixed-machine-settings.json"));
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn the_handshake_says_what_it_speaks_and_where_it_is() {
        let root = workspace("handshake");
        let mut server = Server::new(&root).expect("a server");
        let said = ask(&mut server, HANDSHAKE, json!({})).expect("a handshake");
        assert_eq!(said["protocol"], PROTOCOL_VERSION);
        assert!(said["methods"]
            .as_array()
            .expect("methods")
            .contains(&json!("fs.read")));
        // Canonical, so a client can compare it directly with paths it receives
        // later.
        assert_eq!(
            said["workspace"].as_str().map(PathBuf::from),
            Some(root.canonicalize().expect("canonical"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_inside_the_workspace_is_read_and_written() {
        let root = workspace("read-write");
        let mut server = Server::new(&root).expect("a server");

        let said = ask(&mut server, "fs.read", json!({ "path": "src/main.rs" })).expect("read");
        assert_eq!(said["text"], "fn main() {}\n");

        ask(
            &mut server,
            "fs.write",
            json!({ "path": "src/main.rs", "text": "fn main() { println!(); }\n" }),
        )
        .expect("write");
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.rs")).expect("the file"),
            "fn main() { println!(); }\n"
        );

        // For a file that does not exist yet, the parent must be inside the
        // workspace, because the file itself cannot be canonicalised.
        ask(
            &mut server,
            "fs.write",
            json!({ "path": "src/new.rs", "text": "// new\n" }),
        )
        .expect("write a new file");
        assert!(root.join("src/new.rs").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_path_outside_the_workspace_is_refused_however_it_is_spelled() {
        let root = workspace("outside");
        let mut server = Server::new(&root).expect("a server");

        // Create a real file one directory up. Otherwise the Unix-style paths
        // below would be rejected on Windows because they do not exist, and the
        // test would pass without checking confinement.
        std::fs::write(
            root.parent().expect("a parent").join("secrets.txt"),
            "secret\n",
        )
        .expect("a file");
        for asked in ["../secrets.txt", "src/../../secrets.txt"] {
            let error = ask(&mut server, "fs.read", json!({ "path": asked }))
                .expect_err(&format!("{asked} should be refused"));
            assert!(error.contains("outside the workspace"), "{asked}: {error}");
        }

        // These are Unix paths, so on Windows they are rejected because they do
        // not exist rather than because they escape. Either rejection is
        // acceptable here; the exact reason is checked above with a real file.
        for asked in ["/etc/passwd", "/etc/./passwd"] {
            let error = ask(&mut server, "fs.read", json!({ "path": asked }))
                .expect_err(&format!("{asked} should be refused"));
            assert!(
                error.contains("outside the workspace") || error.contains("cannot be read"),
                "{asked}: {error}"
            );
        }
        // Writes outside the workspace are also rejected.
        let error = ask(
            &mut server,
            "fs.write",
            json!({ "path": "../escaped.txt", "text": "x" }),
        )
        .expect_err("writing outside should be refused");
        assert!(error.contains("outside the workspace"), "{error}");
        assert!(!root
            .parent()
            .expect("a parent")
            .join("escaped.txt")
            .exists());
        let _ = std::fs::remove_file(root.parent().expect("a parent").join("secrets.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_out_of_the_workspace_is_refused_too() {
        // This is why confinement is checked on the canonical path. Checked as
        // written, `escape/passwd` is inside the workspace but reads /etc/passwd.
        let root = workspace("symlink");
        std::os::unix::fs::symlink("/etc", root.join("escape")).expect("a symlink");
        let mut server = Server::new(&root).expect("a server");
        let error = ask(&mut server, "fs.read", json!({ "path": "escape/passwd" }))
            .expect_err("a symlink out should be refused");
        assert!(error.contains("outside the workspace"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_sibling_directory_with_the_same_prefix_is_not_inside() {
        // `/home/u/project-secrets` starts with the *text* `/home/u/project`.
        let root = workspace("prefix");
        let sibling = root.with_file_name(format!(
            "{}-secrets",
            root.file_name().expect("a name").to_string_lossy()
        ));
        std::fs::create_dir_all(&sibling).expect("a directory");
        std::fs::write(sibling.join("keys.txt"), "secret\n").expect("a file");

        let mut server = Server::new(&root).expect("a server");
        let error = ask(
            &mut server,
            "fs.read",
            json!({ "path": sibling.join("keys.txt").display().to_string() }),
        )
        .expect_err("a sibling is not inside");
        assert!(error.contains("outside the workspace"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    #[test]
    fn an_absolute_path_inside_the_workspace_is_allowed() {
        // A client obtains such paths from a listing or from the handshake's root.
        let root = workspace("absolute");
        let mut server = Server::new(&root).expect("a server");
        let inside = root
            .canonicalize()
            .expect("canonical")
            .join("README.md")
            .display()
            .to_string();
        let said = ask(&mut server, "fs.read", json!({ "path": inside })).expect("read");
        assert_eq!(said["text"], "# hello\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn listing_is_relative_slash_separated_and_skips_the_usual_directories() {
        let root = workspace("list");
        std::fs::create_dir_all(root.join(".git")).expect("a directory");
        std::fs::write(root.join(".git/config"), "x").expect("a file");
        std::fs::create_dir_all(root.join("target/debug")).expect("a directory");
        std::fs::write(root.join("target/debug/deco"), "x").expect("a file");

        let mut server = Server::new(&root).expect("a server");
        let said = ask(&mut server, "fs.list", json!({})).expect("a listing");
        let files: Vec<&str> = said["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|value| value.as_str().expect("a string"))
            .collect();
        assert_eq!(files, vec!["README.md", "src/main.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_is_not_text_is_refused_rather_than_mangled() {
        // Replacing the invalid bytes would write the replacements back on save
        // and corrupt the file.
        let root = workspace("binary");
        std::fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).expect("a file");
        let mut server = Server::new(&root).expect("a server");
        let error =
            ask(&mut server, "fs.read", json!({ "path": "blob.bin" })).expect_err("not text");
        assert!(error.contains("UTF-8"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_method_this_server_does_not_have_is_an_error_and_not_silence() {
        // An error reply prevents the client from waiting indefinitely.
        let root = workspace("unknown");
        let mut server = Server::new(&root).expect("a server");
        let error = ask(&mut server, "fs.deleteEverything", json!({})).expect_err("unknown");
        assert!(error.contains("does not implement"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_request_missing_its_parameters_says_which_one() {
        let root = workspace("params");
        let mut server = Server::new(&root).expect("a server");
        let error = ask(&mut server, "fs.read", json!({})).expect_err("no path");
        assert!(error.contains("path"), "{error}");
        let error = ask(&mut server, "fs.write", json!({ "path": "a.txt" })).expect_err("no text");
        assert!(error.contains("text"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_notification_is_not_answered() {
        let root = workspace("notify");
        let mut server = Server::new(&root).expect("a server");
        assert_eq!(
            server.handle(Message::Notification {
                method: "$/hello".to_owned(),
                params: json!({}),
            }),
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_session_reads_requests_until_the_stream_ends() {
        let root = workspace("session");
        let mut server = Server::new(&root).expect("a server");

        let mut input = Vec::new();
        for message in [
            request(1, HANDSHAKE, json!({})),
            request(2, "fs.read", json!({ "path": "README.md" })),
        ] {
            frame::write(&mut input, &message).expect("a frame");
        }
        let mut output = Vec::new();
        serve(&mut Cursor::new(input), &mut output, &mut server).expect("a session");

        let mut replies = Cursor::new(output);
        let first = frame::read(&mut replies)
            .expect("a frame")
            .expect("a reply");
        assert!(matches!(first, Message::Response { id: 1, .. }));
        let second = frame::read(&mut replies)
            .expect("a frame")
            .expect("a reply");
        match second {
            Message::Response { id, result, .. } => {
                assert_eq!(id, 2);
                assert_eq!(result.expect("a result")["text"], "# hello\n");
            }
            other => panic!("expected a response, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shutdown_is_answered_and_then_the_session_ends() {
        // The request is answered first, so the client learns that the server
        // is stopping.
        let root = workspace("shutdown");
        let mut server = Server::new(&root).expect("a server");
        let mut input = Vec::new();
        frame::write(&mut input, &request(1, "$/shutdown", json!({}))).expect("a frame");
        frame::write(&mut input, &request(2, HANDSHAKE, json!({}))).expect("a frame");

        let mut output = Vec::new();
        serve(&mut Cursor::new(input), &mut output, &mut server).expect("a session");

        let mut replies = Cursor::new(output);
        assert!(matches!(
            frame::read(&mut replies).expect("a frame"),
            Some(Message::Response { id: 1, .. })
        ));
        // No further replies: the second request was never read.
        assert_eq!(frame::read(&mut replies).expect("a frame"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_that_does_not_exist_is_refused_at_startup() {
        // Rejected at startup instead of serving a root on which every request
        // would fail.
        assert!(Server::new("/nowhere/at/all/really").is_err());
    }

    #[test]
    fn a_search_finds_matches_and_reports_where_they_are() {
        let root = workspace("search");
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    let needle = 1;\n}\n",
        )
        .expect("a file");
        let mut server = Server::new(&root).expect("a server");

        let found = ask(&mut server, "fs.search", json!({ "needle": "needle" }))
            .expect("a search should succeed");
        let matches = found["matches"].as_array().expect("matches");
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0]["path"], "src/main.rs");
        // Zero-based, like every other position in this protocol.
        assert_eq!(matches[0]["line"], 1);
        assert_eq!(matches[0]["character"], 8);
        // Trimmed by the server: indentation is not useful in a result and would
        // add bytes to the transfer.
        assert_eq!(matches[0]["text"], "let needle = 1;");
        assert_eq!(found["truncated"], false);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_search_honours_the_same_options_the_find_bar_does() {
        // This server depends on `deco-core` so that it shares the find bar's
        // definition of a match.
        let root = workspace("search-options");
        std::fs::write(root.join("src/main.rs"), "Needle needles needle\n").expect("a file");
        let mut server = Server::new(&root).expect("a server");

        let count = |server: &mut Server, params: serde_json::Value| {
            ask(server, "fs.search", params).expect("a search")["matches"]
                .as_array()
                .expect("matches")
                .len()
        };

        // Case-sensitive by default, and `needles` contains `needle`.
        assert_eq!(count(&mut server, json!({ "needle": "needle" })), 2);
        assert_eq!(
            count(
                &mut server,
                json!({ "needle": "needle", "caseSensitive": false })
            ),
            3
        );
        // Whole word drops the one inside `needles`.
        assert_eq!(
            count(
                &mut server,
                json!({ "needle": "needle", "wholeWord": true })
            ),
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_search_stops_at_its_limit_and_says_so() {
        // Enforced by the server rather than the client, because the server
        // does not authenticate the client.
        let root = workspace("search-limit");
        let line = "needle\n".repeat(MAX_MATCHES + 50);
        std::fs::write(root.join("src/main.rs"), line).expect("a file");
        let mut server = Server::new(&root).expect("a server");

        let found = ask(&mut server, "fs.search", json!({ "needle": "needle" })).expect("a search");
        assert_eq!(
            found["matches"].as_array().expect("matches").len(),
            MAX_MATCHES
        );
        assert_eq!(found["truncated"], true);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_search_skips_what_it_should_not_read() {
        let root = workspace("search-skips");
        // Binary files are skipped.
        std::fs::write(root.join("src/blob.bin"), [0xff, 0xfe, b'n', b'e', 0x00]).expect("a file");
        // Over the search limit, which is far smaller than the open limit.
        let big = "needle\n".repeat((MAX_SEARCHED_BYTES as usize / 7) + 10);
        std::fs::write(root.join("src/huge.txt"), big).expect("a file");
        std::fs::write(root.join("src/small.txt"), "needle\n").expect("a file");
        // A directory the walk skips.
        std::fs::create_dir_all(root.join(".git")).expect("a directory");
        std::fs::write(root.join(".git/config"), "needle\n").expect("a file");
        let mut server = Server::new(&root).expect("a server");

        let found = ask(&mut server, "fs.search", json!({ "needle": "needle" })).expect("a search");
        let paths: Vec<&str> = found["matches"]
            .as_array()
            .expect("matches")
            .iter()
            .map(|entry| entry["path"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(paths, ["src/small.txt"], "{paths:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_search_for_nothing_is_not_a_search_for_everything() {
        let root = workspace("search-empty");
        let mut server = Server::new(&root).expect("a server");
        let found = ask(&mut server, "fs.search", json!({ "needle": "" })).expect("a search");
        assert!(found["matches"].as_array().expect("matches").is_empty());

        // A missing needle is a bad request rather than an empty result.
        assert!(ask(&mut server, "fs.search", json!({})).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stat_reports_what_a_thing_is_in_the_numbering_the_editor_uses() {
        let root = workspace("stat");
        let mut server = Server::new(&root).expect("a server");

        let file = ask(&mut server, "fs.stat", json!({ "path": "src/main.rs" })).expect("a stat");
        // VS Code's `FileType`: 1 is a file, 2 is a directory. The values are
        // passed through unchanged because they reach VS Code's API.
        assert_eq!(file["stat"]["type"], 1);
        assert_eq!(file["stat"]["size"], 13);
        assert!(file["stat"]["mtime"].as_u64().unwrap_or(0) > 0);

        let directory = ask(&mut server, "fs.stat", json!({ "path": "src" })).expect("a stat");
        assert_eq!(directory["stat"]["type"], 2);

        // The same confinement rule applies as for every other method.
        let error = ask(&mut server, "fs.stat", json!({ "path": "../secrets.txt" }))
            .expect_err("a refusal");
        assert!(error.contains("outside the workspace"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_listing_is_one_level_and_in_a_settled_order() {
        let root = workspace("dir");
        std::fs::create_dir_all(root.join("src/deeper")).expect("a directory");
        std::fs::write(root.join("src/a.rs"), "x").expect("a file");
        let mut server = Server::new(&root).expect("a server");

        let listed = ask(&mut server, "fs.dir", json!({ "path": "src" })).expect("a listing");
        let entries: Vec<(String, u64)> = listed["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|entry| {
                (
                    entry["name"].as_str().unwrap_or_default().to_owned(),
                    entry["kind"].as_u64().unwrap_or(0),
                )
            })
            .collect();
        // One level: `deeper` is listed, but its contents are not. Entries are
        // sorted because `read_dir` guarantees no order.
        assert_eq!(
            entries,
            [
                ("a.rs".to_owned(), 1),
                ("deeper".to_owned(), 2),
                ("main.rs".to_owned(), 1),
            ]
        );

        let error =
            ask(&mut server, "fs.dir", json!({ "path": "src/main.rs" })).expect_err("a refusal");
        // A file is not a directory. The error includes the operating system's
        // message rather than a paraphrase.
        assert!(error.contains("cannot be read"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_reported_as_one_rather_than_as_what_it_points_at() {
        // Following the link would report on a file that may be outside the
        // workspace.
        let root = workspace("link");
        std::os::unix::fs::symlink(root.join("src/main.rs"), root.join("src/link.rs"))
            .expect("a symlink");
        let mut server = Server::new(&root).expect("a server");

        let listed = ask(&mut server, "fs.dir", json!({ "path": "src" })).expect("a listing");
        let link = listed["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|entry| entry["name"] == "link.rs")
            .expect("the link");
        // 65: a symbolic link (64) to a file (1).
        assert_eq!(link["kind"], 65);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_is_created_and_a_file_is_moved_and_removed() {
        let root = workspace("writes");
        let mut server = Server::new(&root).expect("a server");

        ask(&mut server, "fs.mkdir", json!({ "path": "made/deeper" })).expect("a directory");
        assert!(root.join("made/deeper").is_dir());

        ask(
            &mut server,
            "fs.rename",
            json!({ "source": "src/main.rs", "target": "made/moved.rs" }),
        )
        .expect("a rename");
        assert!(!root.join("src/main.rs").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("made/moved.rs")).expect("the file"),
            "fn main() {}\n"
        );

        ask(
            &mut server,
            "fs.copy",
            json!({ "source": "made/moved.rs", "target": "made/copy.rs" }),
        )
        .expect("a copy");
        assert!(root.join("made/moved.rs").exists());
        assert!(root.join("made/copy.rs").exists());

        ask(&mut server, "fs.delete", json!({ "path": "made/copy.rs" })).expect("a delete");
        assert!(!root.join("made/copy.rs").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_with_something_in_it_needs_the_caller_to_say_recursive() {
        // Only the caller's `recursive` flag permits deleting a directory's
        // contents; the server never sets it.
        let root = workspace("delete-recursive");
        let mut server = Server::new(&root).expect("a server");

        let error = ask(&mut server, "fs.delete", json!({ "path": "src" })).expect_err("a refusal");
        assert!(error.contains("cannot be written"), "{error}");
        assert!(
            root.join("src/main.rs").exists(),
            "nothing should have gone"
        );

        ask(
            &mut server,
            "fs.delete",
            json!({ "path": "src", "recursive": true }),
        )
        .expect("a recursive delete");
        assert!(!root.join("src").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_move_is_confined_at_both_ends() {
        // A source outside the workspace would import an outside file, and a
        // target outside would export a workspace file. Both are rejected with
        // an error naming the path.
        let root = workspace("move-outside");
        std::fs::write(
            root.parent().expect("a parent").join("outside-move.txt"),
            "secret\n",
        )
        .expect("a file");
        let mut server = Server::new(&root).expect("a server");

        let error = ask(
            &mut server,
            "fs.rename",
            json!({ "source": "src/main.rs", "target": "../escaped.rs" }),
        )
        .expect_err("a refusal");
        assert!(error.contains("outside the workspace"), "{error}");

        let error = ask(
            &mut server,
            "fs.rename",
            json!({ "source": "../outside-move.txt", "target": "src/taken.rs" }),
        )
        .expect_err("a refusal");
        assert!(error.contains("outside the workspace"), "{error}");

        assert!(root.join("src/main.rs").exists());
        assert!(!root.join("src/taken.rs").exists());
        assert!(root
            .parent()
            .expect("a parent")
            .join("outside-move.txt")
            .exists());
        let _ = std::fs::remove_file(root.parent().expect("a parent").join("outside-move.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_workspace_cannot_be_deleted_through() {
        // Intentionally stricter than necessary. Removing the link would only
        // change a directory entry inside the workspace, but every path this
        // server acts on is confined after canonicalisation, and no operation
        // is exempt from that rule.
        //
        // The critical property is that the link's target is never deleted.
        let root = workspace("delete-link");
        let outside = root.parent().expect("a parent").join("kept.txt");
        std::fs::write(&outside, "still here\n").expect("a file");
        std::os::unix::fs::symlink(&outside, root.join("src/link.txt")).expect("a symlink");
        let mut server = Server::new(&root).expect("a server");

        let error = ask(&mut server, "fs.delete", json!({ "path": "src/link.txt" }))
            .expect_err("a refusal");
        assert!(error.contains("outside the workspace"), "{error}");
        assert!(outside.exists(), "the link's target must be untouched");

        // A link inside the workspace is removed as a link, and its target is
        // kept.
        std::os::unix::fs::symlink(root.join("src/main.rs"), root.join("src/inside.rs"))
            .expect("a symlink");
        ask(&mut server, "fs.delete", json!({ "path": "src/inside.rs" })).expect("a delete");
        assert!(!root.join("src/inside.rs").exists());
        assert!(root.join("src/main.rs").exists());

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }
}
