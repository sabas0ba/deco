//! The far end: `deco --server --stdio`, running where the files are.
//!
//! [`crate::transport`] has always known how to *start* this — the
//! command it builds ends in `deco --server --stdio` — and there was nothing on
//! the other side of it. This is that side: a loop over [`crate::frame`]
//! messages, answering a small set of methods against one directory.
//!
//! # One directory, and no way out of it
//!
//! A server started with `--workspace /home/u/project` will read and write inside
//! that directory and refuse everything else, by name. This is stricter than VS
//! Code, whose remote server will open any path the account can reach, and the
//! reason to be stricter is what the client is: whatever is on the other end of
//! an SSH connection deco did not authenticate itself. A bug in the frontend, a
//! hijacked session, or a `deco-remote://` link someone else wrote should not be
//! able to ask for `~/.ssh/id_ed25519`.
//!
//! Confinement is checked on the **canonical** path, so a symlink inside the
//! workspace pointing outside it is refused too. Checking the path as written
//! would make `project/link-to-etc/passwd` legal, which is exactly the shape of
//! the mistake this is here to prevent.
//!
//! # What it does not do yet
//!
//! No port forwarding, no provisioning (the binary has to be there already), no
//! language servers or extensions on the remote, and no watching for changes. The
//! methods below are what opening, listing and saving a file need, and nothing
//! else is claimed.

use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};

use serde_json::json;

use crate::frame::{self, Message};

/// The protocol version this server speaks.
///
/// Sent in the handshake and checked by the client, so a new frontend against an
/// old server fails with a sentence rather than by half-working.
pub const PROTOCOL_VERSION: &str = "1";

/// The handshake, which every session begins with.
pub const HANDSHAKE: &str = "$/handshake";

/// How many entries a listing will return.
///
/// The same bound the local file walk uses: a listing is for a picker, and a
/// picker over a hundred thousand files is not a picker.
pub const MAX_LISTED: usize = 10_000;

/// The largest file the server will read or write.
///
/// Below the frame ceiling, because the text has to fit in a frame with room for
/// the JSON around it — and because a client asking for a 4GB file over SSH has
/// asked for something that will not work regardless.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServerError {
    /// The path escapes the workspace.
    #[error("{path} is outside the workspace this server was started for")]
    OutsideWorkspace {
        /// What was asked for, as it was written.
        path: String,
    },
    /// The path could not be resolved at all.
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
    /// The file is not UTF-8.
    ///
    /// Refused rather than replaced with substitution characters: deco would then
    /// write those back on save, quietly corrupting the file.
    #[error("{path} is not valid UTF-8, and deco will not guess at it")]
    NotText {
        /// What was asked for.
        path: String,
    },
    /// The method is not one this server has.
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
}

/// A server bound to one directory.
#[derive(Debug, Clone)]
pub struct Server {
    /// The canonical workspace root. Every path is resolved against it and must
    /// stay inside it.
    root: PathBuf,
}

impl Server {
    /// Binds a server to `root`.
    ///
    /// The root is canonicalised once, here, so that every later comparison is
    /// between two resolved paths — comparing a resolved path against an
    /// unresolved root is how `..` and symlinks get through.
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            root: root.as_ref().canonicalize()?,
        })
    }

    /// The directory this server serves.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a client-supplied path inside the workspace.
    ///
    /// Relative paths are taken as relative to the root, which is how a client
    /// refers to a file it saw in a listing. An absolute path is allowed only if
    /// it is already inside the root — a client that learned a path from a
    /// listing has one, and a client that guessed at `/etc/passwd` gets a refusal
    /// naming the reason.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ServerError> {
        let asked = Path::new(path);
        let joined = if asked.is_absolute() {
            asked.to_path_buf()
        } else {
            self.root.join(asked)
        };

        // Canonicalising needs the file to exist, which a path being written for
        // the first time does not. So the *parent* is canonicalised — it must
        // exist, since a file cannot be created in a directory that does not —
        // and the name is put back on afterwards.
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

    /// Answers one request.
    ///
    /// Returns the reply to send. A notification produces `None`, and an
    /// unrecognised method produces an error reply rather than silence: a client
    /// waiting for an answer it will never get is worse than one told no.
    pub fn handle(&mut self, message: Message) -> Option<Message> {
        let (id, method, params) = match message {
            Message::Request { id, method, params } => (id, method, params),
            // Nothing here is driven by notifications yet, and answering one
            // would be a protocol error.
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
                // What this server can do, so a client need not discover it by
                // being refused.
                "methods": [
                    "fs.read",
                    "fs.write",
                    "fs.list",
                    "fs.search",
                    "fs.stat",
                    "fs.dir",
                    "$/shutdown"
                ],
            })),
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
                std::fs::write(&resolved, text).map_err(|error| ServerError::Unreadable {
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
                    // than as whatever it points at — which may be outside the
                    // workspace, and is a thing the client should learn about
                    // before it follows it.
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    listed.push(json!({
                        "name": entry.file_name().to_string_lossy(),
                        "kind": kind_of(&metadata),
                    }));
                }
                // Sorted, because `read_dir` promises no order and a list that
                // reshuffles between calls is one nothing can be compared against.
                listed.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                Ok(json!({ "entries": listed }))
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
            "$/shutdown" => Ok(json!({ "stopping": true })),
            other => Err(ServerError::UnknownMethod {
                method: other.to_owned(),
            }),
        }
    }

    /// Every file under `from`, as paths relative to the workspace root.
    ///
    /// Relative because that is what the client shows and what it will ask for
    /// next, and because the remote's directory layout is not the frontend's
    /// business.
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
                // The same ones the local walk skips, and for the same reason:
                // nobody opens a file from `.git` on purpose, and walking it is
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

    /// Searches every file in the workspace for `needle`.
    ///
    /// Here rather than on the client because the files are here — that is the
    /// whole of it. A client walking its own disk in a remote session searches
    /// the wrong machine and reports matches in files the editor is not showing,
    /// which is why this used to be refused instead.
    ///
    /// Synchronous and bounded, like the local one it replaces. The bounds are
    /// reported rather than hidden, so "500 matches" and "the first 500 of many"
    /// are distinguishable.
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
            // Size first, so a huge file costs a `stat` rather than a read.
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > MAX_SEARCHED_BYTES {
                continue;
            }
            // Not UTF-8 is how a binary file presents itself here, and skipping
            // it is right: a match inside a PNG is not a search result.
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
                    // Trimmed and cut here rather than on the client: the whole
                    // point of a limit is that the bytes are not sent, and a
                    // minified line is one match and a megabyte.
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
/// The same number the editor's own search stops at, and for the same reason: a
/// term that appears ten thousand times is not being read one occurrence at a
/// time. Enforced here rather than trusted to the client, because the client is
/// whatever is on the other end of a connection this server did not authenticate.
pub const MAX_MATCHES: usize = 500;

/// Largest file a search will read.
///
/// Much smaller than [`MAX_FILE_BYTES`], which is what a person can ask to
/// *open*. A minified bundle or a checked-in database is not what anyone means by
/// "search my project", and reading it is most of what the search would cost.
pub const MAX_SEARCHED_BYTES: u64 = 1 << 20;

/// What kind of thing a directory entry is, in VS Code's numbering.
///
/// `Unknown = 0`, `File = 1`, `Directory = 2`, `SymbolicLink = 64`, and a link is
/// the *sum* — 65 for a link to a file. deco's own protocol carries VS Code's
/// numbers rather than a spelling of its own, because the extension API these
/// eventually reach is VS Code's and translating twice is one translation too
/// many.
fn kind_of(metadata: &std::fs::Metadata) -> u32 {
    let mut kind = if metadata.is_dir() { 2 } else { 1 };
    if metadata.is_symlink() {
        kind += 64;
    }
    kind
}

/// A file's stat, in the shape VS Code's `FileStat` has.
///
/// Times in milliseconds since the epoch, which is what JavaScript counts in. A
/// time the platform will not give up becomes 0 rather than a guess: an extension
/// comparing timestamps should see an obviously absent one, not a plausible wrong
/// one.
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

/// A relative path with `/` separators, whatever this platform uses.
///
/// The wire is one format: a client on Windows talking to a server on Linux must
/// not have to guess which end's separator a path came from.
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
/// stream whose length was wrong, and pretending otherwise would mean reading the
/// next file's contents as a header.
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
        // Canonical, so a client comparing it against a path it was given later
        // is comparing like with like.
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

        // A file that does not exist yet: the parent is what has to be inside the
        // workspace, since the file itself cannot be canonicalised.
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

        // A file that really is one directory up. Without it the interesting
        // spellings below are refused on Windows for *not existing* — the paths
        // are Unix-shaped — and this would pass without checking confinement at
        // all.
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

        // These are Unix paths, so on Windows they are refused for not existing
        // rather than for escaping. Both are refusals, which is what matters
        // here; the exact reason is checked above with a file that is really
        // there.
        for asked in ["/etc/passwd", "/etc/./passwd"] {
            let error = ask(&mut server, "fs.read", json!({ "path": asked }))
                .expect_err(&format!("{asked} should be refused"));
            assert!(
                error.contains("outside the workspace") || error.contains("cannot be read"),
                "{asked}: {error}"
            );
        }
        // And writing, which is the direction that does damage.
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
        // The reason confinement is checked on the canonical path. Checked as
        // written, `escape/passwd` is inside the workspace and reads /etc/passwd.
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
        // What a client has after a listing, or after a handshake told it the root.
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
        // Replacing the invalid bytes would mean writing the replacements back on
        // save, which turns "deco opened my binary" into "deco corrupted my
        // binary".
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
        // A client waiting for a reply that never comes is worse off than one
        // told no.
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
        // Answered first: a client that asked to stop should learn that it did.
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
        // Nothing after it: the second request was never read.
        assert_eq!(frame::read(&mut replies).expect("a frame"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_that_does_not_exist_is_refused_at_startup() {
        // Rather than serving a root that will fail every request afterwards.
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
        // Zero-based, like every other position on this wire.
        assert_eq!(matches[0]["line"], 1);
        assert_eq!(matches[0]["character"], 8);
        // Trimmed by the server: the indentation is not what a person is reading
        // the result for, and sending it is bytes over a link.
        assert_eq!(matches[0]["text"], "let needle = 1;");
        assert_eq!(found["truncated"], false);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_search_honours_the_same_options_the_find_bar_does() {
        // The reason this server depends on `deco-core` at all: one definition of
        // what a match is, so a term that matches in the find bar matches here.
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
        // Enforced here rather than trusted to the client, which is whatever is
        // on the other end of a connection this server did not authenticate.
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
        // Binary: a match inside a PNG is not a search result.
        std::fs::write(root.join("src/blob.bin"), [0xff, 0xfe, b'n', b'e', 0x00]).expect("a file");
        // Over the search limit, which is far smaller than the open limit.
        let big = "needle\n".repeat((MAX_SEARCHED_BYTES as usize / 7) + 10);
        std::fs::write(root.join("src/huge.txt"), big).expect("a file");
        std::fs::write(root.join("src/small.txt"), "needle\n").expect("a file");
        // And a directory the walk does not enter at all.
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

        // And a missing needle is a bad request rather than an empty answer: the
        // client asked something this cannot interpret.
        assert!(ask(&mut server, "fs.search", json!({})).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stat_reports_what_a_thing_is_in_the_numbering_the_editor_uses() {
        let root = workspace("stat");
        let mut server = Server::new(&root).expect("a server");

        let file = ask(&mut server, "fs.stat", json!({ "path": "src/main.rs" })).expect("a stat");
        // VS Code's `FileType`: 1 is a file, 2 is a directory. Carried rather
        // than translated, because the API these reach is VS Code's.
        assert_eq!(file["stat"]["type"], 1);
        assert_eq!(file["stat"]["size"], 13);
        assert!(file["stat"]["mtime"].as_u64().unwrap_or(0) > 0);

        let directory = ask(&mut server, "fs.stat", json!({ "path": "src" })).expect("a stat");
        assert_eq!(directory["stat"]["type"], 2);

        // And the confinement rule is the same one every other method follows.
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
        // One level: `deeper` is named, and what is inside it is not. And sorted,
        // because `read_dir` promises no order.
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
        // A file is not a directory, and the operating system's own words say so
        // better than a paraphrase would.
        assert!(error.contains("cannot be read"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_reported_as_one_rather_than_as_what_it_points_at() {
        // Following it would report on a file that may be outside the workspace
        // entirely, which is the one thing this server exists to be careful about.
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
}
