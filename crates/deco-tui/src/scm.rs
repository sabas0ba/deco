//! Running `git status` without making the editor wait for it.
//!
//! [`deco_scm`] blocks: it spawns `git`, waits, and parses what came back. On
//! deco's own checkout that is a few milliseconds, and on a working tree with
//! a million files it is not — so the wait happens on a thread and the answer
//! is collected later, the same bargain the language server's stdio pump
//! makes.
//!
//! What this adds on top of the crate is the *when*:
//!
//! - **Only when the session says so.** [`Session::scm_wanted`] is set by a
//!   save or a file operation, never by a keystroke. A process per character
//!   would be absurd, and a status bar that is a moment stale after a write is
//!   not.
//! - **One at a time.** A second run while the first is still going would
//!   race to fill the same field, and the loser's answer would be the one on
//!   screen. If something changes while a run is in flight the flag is still
//!   set when it lands, so the next poll starts a fresh one.
//! - **An absence is remembered.** No `git` on the machine, and no repository
//!   here, are permanent for the session: asking again on every save would be
//!   a spawn per save to learn what was already known. Anything else — git
//!   refusing, output that did not parse — is transient and is retried.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use deco_editor::Session;
use deco_scm::{Git, Operation, ScmError, Status};

/// Work sent to the dedicated remote-SCM connection.
enum RemoteRequest {
    Status,
    Committed(PathBuf),
    Apply(Operation),
    Stop,
}

/// One answer from that connection.
enum RemoteResponse {
    Status(Result<(PathBuf, Status), String>),
    Committed {
        path: PathBuf,
        result: Result<Option<String>, String>,
    },
    Applied {
        operation: Operation,
        result: Result<(), String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteInFlight {
    Status,
    Committed,
    Apply,
}

/// A second connection whose worker owns every blocking remote git call.
///
/// File reads and extension requests keep using the session's primary
/// connection. A status walk or a commit hook on this one can therefore take
/// its time without stopping the editor from painting or serving either.
struct Remote {
    requests: Sender<RemoteRequest>,
    responses: Receiver<RemoteResponse>,
    inflight: Option<RemoteInFlight>,
    pending_operation: Option<Operation>,
}

impl Remote {
    fn new(mut client: deco_remote::Client) -> Result<Self, String> {
        let missing: Vec<&str> = ["scm.status", "scm.committed", "scm.apply"]
            .into_iter()
            .filter(|method| !client.serves(method))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "the remote server does not support {} — update it to this deco version",
                missing.join(", ")
            ));
        }

        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let response = match request {
                    RemoteRequest::Status => RemoteResponse::Status(
                        client.scm_status().map_err(|error| error.to_string()),
                    ),
                    RemoteRequest::Committed(path) => RemoteResponse::Committed {
                        result: client
                            .scm_committed(&path)
                            .map_err(|error| error.to_string()),
                        path,
                    },
                    RemoteRequest::Apply(operation) => RemoteResponse::Applied {
                        result: client
                            .scm_apply(&operation)
                            .map_err(|error| error.to_string()),
                        operation,
                    },
                    RemoteRequest::Stop => {
                        client.shutdown();
                        break;
                    }
                };
                if response_tx.send(response).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            requests: request_tx,
            responses: response_rx,
            inflight: None,
            pending_operation: None,
        })
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let _ = self.requests.send(RemoteRequest::Stop);
    }
}

/// Runs `git status` for one workspace, off the event loop.
pub struct Scm {
    git: Git,
    /// The working tree to ask about. `None` when deco was started without a
    /// workspace — a lone file has no repository to report on.
    root: Option<PathBuf>,
    /// The run that has not answered yet.
    inflight: Option<Receiver<Result<Status, ScmError>>>,
    /// Where the repository begins, once it has been asked.
    ///
    /// Not the same as [`Scm::root`], which is the folder deco was started in.
    /// Opening a subdirectory of a repository is ordinary, and every path git
    /// reports — and every path it will answer about — is relative to the
    /// repository, so the two have to be told apart.
    repo_root: Option<PathBuf>,
    /// Why there will never be an answer, once that is known.
    ///
    /// Kept rather than shown. There is nowhere to put it yet: the panel that
    /// would hold an output view is
    /// [built and empty](https://github.com/sabas0ba/deco/blob/main/docs/chrome.md),
    /// and a message on the status bar for "this folder is not a repository"
    /// would be a line of noise for everyone who opened a folder that is not
    /// one. A reader exists so the reason is not lost, and so a test can
    /// assert deco knew why rather than merely showing nothing.
    unavailable: Option<String>,
    /// Present when git lives behind the remote protocol rather than here.
    remote: Option<Remote>,
    /// The far end's workspace, used to keep repository paths in the same
    /// absolute-or-relative coordinates as the session's document paths.
    remote_workspace: Option<PathBuf>,
}

impl Scm {
    /// A runner for `root`, using whatever `git.path` named.
    ///
    /// The setting is VS Code's, and reading it here rather than at each spawn
    /// means a machine with git somewhere unusual is configured once.
    pub fn new(settings: &deco_config::Settings, root: Option<PathBuf>) -> Self {
        let program = settings
            .get_str("git.path", None)
            .filter(|path| !path.trim().is_empty());
        Self {
            git: match program {
                Some(path) => Git::new(path),
                None => Git::default(),
            },
            root,
            repo_root: None,
            inflight: None,
            unavailable: None,
            remote: None,
            remote_workspace: None,
        }
    }

    /// A runner whose git process and repository are on the far end.
    pub fn remote(client: deco_remote::Client, workspace: PathBuf) -> Self {
        match Remote::new(client) {
            Ok(remote) => Self {
                git: Git::default(),
                root: None,
                repo_root: None,
                inflight: None,
                unavailable: None,
                remote: Some(remote),
                remote_workspace: Some(workspace),
            },
            Err(error) => Self {
                git: Git::default(),
                root: None,
                repo_root: None,
                inflight: None,
                unavailable: Some(error),
                remote: None,
                remote_workspace: Some(workspace),
            },
        }
    }

    /// Where the repository begins, asking git once if nobody has yet.
    ///
    /// Every path the view holds is repository-relative, so anything that acts
    /// on one — opening it, staging it — needs this rather than the folder
    /// deco was started in. Resolving it lazily *inside the gutter's fetch*
    /// was the bug: with `git.decorations.enabled` off, or a workspace opened
    /// with no file, nothing ever asked, and staging `sub/a.rs` from
    /// `/repo/sub` would have gone looking for `/repo/sub/sub/a.rs`.
    fn repository_root(&mut self, session: &mut Session) -> Option<PathBuf> {
        if let Some(found) = self.repo_root.clone() {
            return Some(found);
        }
        let root = self.root.clone()?;
        match self.git.root(&root) {
            Ok(found) => {
                self.repo_root = Some(found.clone());
                // The session needs it too, to turn a row's path back into
                // something it can open.
                session.set_repository_root(Some(found.clone()));
                Some(found)
            }
            Err(error) => {
                if permanent(&error) {
                    self.unavailable = Some(error.to_string());
                }
                None
            }
        }
    }

    /// Why there is no status, when that is settled. `None` while it might yet
    /// work.
    pub fn unavailable(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// Starts a run if one is wanted, and collects one that has finished.
    ///
    /// Returns whether the session changed, which is what the loop redraws on.
    /// Collecting first so that a save made while git was thinking starts its
    /// own run on this same poll rather than the next one.
    pub fn poll(&mut self, session: &mut Session) -> bool {
        if self.remote.is_some() {
            return self.poll_remote(session);
        }
        // Compatibility with a remote server that predates the SCM methods:
        // there is deliberately no local root to run against, but the fresh
        // session's question still has to be marked answered once.
        if self.root.is_none() && self.unavailable.is_some() && session.scm_wanted() {
            session.scm_started();
            return false;
        }
        let changed = self.collect(session);
        self.start(session);
        changed | self.fetch_committed(session)
    }

    /// Advances the one-at-a-time queue owned by the remote worker.
    fn poll_remote(&mut self, session: &mut Session) -> bool {
        let mut changed = false;
        let response = {
            let remote = self.remote.as_mut().expect("checked by the caller");
            match remote.responses.try_recv() {
                Ok(response) => {
                    remote.inflight = None;
                    Some(response)
                }
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    remote.inflight = None;
                    self.unavailable = Some("the remote source-control connection stopped".into());
                    session.fill_scm(None);
                    changed = true;
                    None
                }
            }
        };

        match response {
            Some(RemoteResponse::Status(Ok((root, status)))) => {
                // Remote documents keep the spelling used to open them. The
                // common CLI form is relative (`src/main.rs`); handing its SCM
                // row an absolute root would open the same file a second time
                // under a different PathBuf. Preserve absolute coordinates only
                // when the active document already uses them.
                let root = match (&self.remote_workspace, &session.document.path) {
                    (Some(workspace), Some(path)) if !path.is_absolute() => root
                        .strip_prefix(workspace)
                        .map(PathBuf::from)
                        .unwrap_or(root),
                    _ => root,
                };
                self.repo_root = Some(root.clone());
                session.set_repository_root(Some(root));
                session.fill_scm(Some(status));
                changed = true;
            }
            Some(RemoteResponse::Status(Err(error))) => {
                // A refusal is one answer, not a permanent absence: an index
                // lock or an in-progress rebase may be gone by the next save.
                session.fill_scm(None);
                if error.contains("begins outside the served workspace") {
                    session.status = Some(format!("remote source control refused: {error}"));
                }
                if remote_permanent(&error) {
                    self.unavailable = Some(error);
                }
                changed = true;
            }
            Some(RemoteResponse::Committed { path, result }) => {
                // The local path does the same on error: no committed text is
                // safer than drawing a gutter against guessed contents.
                session.fill_committed(path, result.unwrap_or(None));
                changed = true;
            }
            Some(RemoteResponse::Applied { operation, result }) => {
                match result {
                    Ok(()) => session.git_operation_done(&operation),
                    Err(error) => session.git_operation_failed(&operation, &error),
                }
                changed = true;
            }
            None => {}
        }

        if self.unavailable.is_some() {
            return changed;
        }

        let remote = self.remote.as_mut().expect("checked by the caller");
        if remote.inflight.is_some() {
            return changed;
        }

        if let Some(operation) = remote.pending_operation.take() {
            if remote
                .requests
                .send(RemoteRequest::Apply(operation.clone()))
                .is_ok()
            {
                remote.inflight = Some(RemoteInFlight::Apply);
            } else {
                self.unavailable = Some("the remote source-control connection stopped".into());
                session.git_operation_failed(&operation, "the remote connection stopped");
                changed = true;
            }
            return changed;
        }

        if session.scm_wanted() {
            // Taken before the request begins, preserving a save that happens
            // while the remote is still walking the working tree.
            session.scm_started();
            if remote.requests.send(RemoteRequest::Status).is_ok() {
                remote.inflight = Some(RemoteInFlight::Status);
            } else {
                self.unavailable = Some("the remote source-control connection stopped".into());
                session.fill_scm(None);
                changed = true;
            }
            return changed;
        }

        if let Some(path) = session.committed_wanted() {
            if remote
                .requests
                .send(RemoteRequest::Committed(path.clone()))
                .is_ok()
            {
                remote.inflight = Some(RemoteInFlight::Committed);
            } else {
                self.unavailable = Some("the remote source-control connection stopped".into());
                session.fill_committed(path, None);
                changed = true;
            }
        }

        changed
    }

    /// Fetches the committed text of one file the session is missing.
    ///
    /// One per poll rather than all at once: this is a process each, and the
    /// file being looked at is the first one asked about, so the gutter that
    /// matters fills in immediately and the rest follow over the next few
    /// turns. Blocking, unlike the status — `git show` of one blob is a read
    /// of one object rather than a walk of the working tree, and putting it on
    /// a thread would mean a second channel for a wait that does not happen.
    fn fetch_committed(&mut self, session: &mut Session) -> bool {
        if self.unavailable.is_some() {
            return false;
        }
        let Some(path) = session.committed_wanted() else {
            return false;
        };
        // Asked once and kept. Without it every path would be stripped against
        // the folder deco was started in, which is only the repository root
        // when nobody opened a subdirectory — and when they did, the blob
        // fetched would be a different file's, silently.
        let Some(repo_root) = self.repository_root(session) else {
            // Answered rather than left standing: a repository that cannot say
            // where it begins cannot say what a file used to hold either, and
            // the alternative is asking on every poll for the rest of the
            // session.
            session.fill_committed(path, None);
            return true;
        };
        // The cache is keyed by the path the editor holds; git answers about
        // paths relative to the repository. A file outside it — opened with
        // `ctrl+o` — has no answer here, and saying so is what stops it being
        // asked about on every poll.
        let text = match path.strip_prefix(&repo_root) {
            Ok(relative) => self.git.committed(&repo_root, relative).unwrap_or(None),
            Err(_) => None,
        };
        session.fill_committed(path, text);
        true
    }

    /// Carries out a repository change the session asked for.
    ///
    /// Blocking. This prevents a second repository write while the view still
    /// describes the state before the first one. A stage-all or a commit hook
    /// can take longer than an index write; moving writes off the event loop
    /// requires an in-flight state and shutdown/cancellation handling, not
    /// only another detached thread.
    pub fn apply(&mut self, session: &mut Session, operation: &deco_scm::Operation) {
        if let Some(remote) = self.remote.as_mut() {
            if remote.pending_operation.is_some() || remote.inflight == Some(RemoteInFlight::Apply)
            {
                session.git_operation_failed(operation, "another repository operation is running");
            } else {
                remote.pending_operation = Some(operation.clone());
            }
            return;
        }
        // Resolved rather than fallen back to the workspace folder. The paths
        // in an operation are repository-relative, and running them from the
        // wrong directory does not fail loudly — it names a file that is not
        // there, or worse, one that is.
        let Some(root) = self.repository_root(session) else {
            session.git_operation_failed(operation, "there is no repository here");
            return;
        };
        match self.git.apply(&root, operation) {
            Ok(()) => session.git_operation_done(operation),
            Err(error) => session.git_operation_failed(operation, &error.to_string()),
        }
    }

    /// Takes the answer, if there is one waiting.
    fn collect(&mut self, session: &mut Session) -> bool {
        let Some(receiver) = self.inflight.as_ref() else {
            return false;
        };
        match receiver.try_recv() {
            Ok(Ok(status)) => {
                self.inflight = None;
                session.fill_scm(Some(status));
                true
            }
            Ok(Err(error)) => {
                self.inflight = None;
                // A run that failed still answers the question the session
                // asked. Without this the flag would stay set and every poll
                // would spawn another git.
                session.fill_scm(None);
                if permanent(&error) {
                    self.unavailable = Some(error.to_string());
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            // The thread died without sending — a panic in the parser, which
            // is a bug rather than a state. Treated as an answer of "nothing"
            // so the editor carries on and the flag does not spin.
            Err(TryRecvError::Disconnected) => {
                self.inflight = None;
                session.fill_scm(None);
                true
            }
        }
    }

    /// Spawns a run, if the session wants one and nothing stands in the way.
    fn start(&mut self, session: &mut Session) {
        if self.inflight.is_some() || self.unavailable.is_some() || !session.scm_wanted() {
            return;
        }
        let Some(root) = self.root.clone() else {
            // No workspace, so nothing to ask about — and the question is
            // marked asked rather than left standing, or every poll would come
            // back here.
            session.scm_started();
            return;
        };
        // Before the thread rather than after it answers: a save while this
        // run is in flight has to set the flag again and be noticed.
        session.scm_started();
        let (sender, receiver) = mpsc::channel();
        let git = self.git.clone();
        // Detached: nothing joins it. The channel is the only thing the thread
        // touches, and dropping the receiver on shutdown is what tells it that
        // nobody is listening — a `git status` outliving the editor by a few
        // milliseconds is not worth a shutdown handshake.
        std::thread::spawn(move || {
            let _ = sender.send(git.status(&root));
        });
        self.inflight = Some(receiver);
    }

    /// Stops the dedicated remote server connection, if there is one.
    pub fn shutdown(&mut self) {
        self.remote = None;
    }
}

/// Whether this is a state rather than a failure.
///
/// No git and no repository will still be true after the next save, so asking
/// again would spawn a process to learn what is known. Everything else might
/// not be: a repository mid-rebase can refuse, and a `git status` that failed
/// once because the index was locked will work on the next try.
fn permanent(error: &ScmError) -> bool {
    matches!(error, ScmError::NoBinary(_) | ScmError::NotARepository(_))
}

/// The remote protocol carries refusals as text, so the three states that
/// cannot change during this server's lifetime are recognised at this edge.
fn remote_permanent(error: &str) -> bool {
    error.contains("is not on this machine")
        || error.contains("is not inside a git repository")
        || error.contains("begins outside the served workspace")
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_config::Settings;

    fn session() -> Session {
        Session::new(
            Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        )
    }

    /// Drives polls until the run lands, so a test does not depend on how fast
    /// `git` is on the machine running it.
    fn settle(scm: &mut Scm, session: &mut Session) {
        for _ in 0..2_000 {
            scm.poll(session);
            if scm.inflight.is_none() && !session.scm_wanted() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the status never landed");
    }

    #[test]
    fn decos_own_checkout_reaches_the_session() {
        let mut session = session();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut scm = Scm::new(&session.settings, Some(root.clone()));
        // A contributor's machine may have no git, and failing there for a
        // reason that is not their change is worse than saying why it skipped.
        if matches!(scm.git.status(&root), Err(ScmError::NoBinary(_))) {
            eprintln!("skipped: no git on this machine");
            return;
        }
        settle(&mut scm, &mut session);

        let status = session.scm_status().expect("this crate is in a repository");
        assert!(!status.head.label().is_empty());
        assert_eq!(scm.unavailable(), None);
    }

    #[test]
    fn a_missing_git_is_asked_about_once() {
        let mut session = session();
        let mut scm = Scm::new(&session.settings, Some(PathBuf::from(".")));
        scm.git = Git::new("git-that-is-not-installed-anywhere");
        settle(&mut scm, &mut session);

        assert_eq!(session.scm_status(), None);
        assert!(
            scm.unavailable()
                .is_some_and(|why| why.contains("git-that")),
            "the reason is kept even though there is nowhere to show it"
        );

        // Whatever happens next, no second process.
        session.scm_changed();
        scm.poll(&mut session);
        assert!(
            scm.inflight.is_none(),
            "a machine without git does not grow one between saves"
        );
    }

    #[test]
    fn without_a_workspace_nothing_is_spawned() {
        let mut session = session();
        let mut scm = Scm::new(&session.settings, None);
        assert!(session.scm_wanted(), "a fresh session wants to know");

        scm.poll(&mut session);
        assert!(scm.inflight.is_none(), "there is nothing to ask about");
        assert!(
            !session.scm_wanted(),
            "and the question is answered rather than asked again on every poll"
        );
    }

    #[test]
    fn git_enabled_false_stops_the_process_rather_than_hiding_it() {
        let mut settings = Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            "git.enabled",
            serde_json::Value::Bool(false),
        );
        let mut session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);
        let mut scm = Scm::new(
            &session.settings,
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
        );

        scm.poll(&mut session);
        assert!(
            scm.inflight.is_none(),
            "turning the feature off must not spawn git and then discard it"
        );
        assert_eq!(session.scm_status(), None);
    }

    #[test]
    fn a_save_while_git_is_thinking_gets_its_own_run() {
        let mut session = session();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut scm = Scm::new(&session.settings, Some(root.clone()));
        if matches!(scm.git.status(&root), Err(ScmError::NoBinary(_))) {
            eprintln!("skipped: no git on this machine");
            return;
        }

        // A run is in flight, and the file is written while it is.
        scm.poll(&mut session);
        assert!(scm.inflight.is_some(), "the first poll starts one");
        session.scm_changed();

        // Whenever that first run lands, the second must follow it — otherwise
        // the bar sits showing a status taken before the write, and nothing
        // asks again until the *next* save.
        for _ in 0..2_000 {
            scm.poll(&mut session);
            if session.scm_status().is_some() && scm.inflight.is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the save made during the first run never got one of its own");
    }

    #[test]
    fn a_files_committed_text_reaches_the_session() {
        let mut session = session();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut scm = Scm::new(&session.settings, Some(root.clone()));
        if matches!(scm.git.status(&root), Err(ScmError::NoBinary(_))) {
            eprintln!("skipped: no git on this machine");
            return;
        }
        // This very file, committed, with an edit that is not.
        let path = root.join("src/scm.rs");
        session.open(path.clone(), "// nothing like the committed text\n");

        assert_eq!(session.committed_wanted(), Some(path.clone()));
        scm.poll(&mut session);
        session.refresh_diffs();

        let diff = session
            .diff_marks(&path)
            .expect("the committed text has arrived");
        assert!(
            !diff.is_empty(),
            "a buffer holding one line that is not what was committed differs"
        );
        assert_eq!(
            session.committed_wanted(),
            None,
            "and it is not asked about again"
        );
    }

    #[test]
    fn a_file_outside_the_workspace_is_answered_rather_than_re_asked() {
        let mut session = session();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut scm = Scm::new(&session.settings, Some(root.clone()));
        if matches!(scm.git.status(&root), Err(ScmError::NoBinary(_))) {
            eprintln!("skipped: no git on this machine");
            return;
        }
        // `ctrl+o` reaches anywhere. Nothing here can say what HEAD had for it,
        // and answering "nothing" is what stops the question being asked on
        // every poll for the rest of the session.
        let outside = PathBuf::from("/etc/hostname");
        session.open(outside.clone(), "elsewhere\n");
        scm.poll(&mut session);
        assert_eq!(session.committed_wanted(), None);
    }

    #[test]
    fn a_refusal_is_tried_again_but_an_absence_is_not() {
        assert!(permanent(&ScmError::NoBinary("git".into())));
        assert!(permanent(&ScmError::NotARepository(PathBuf::from("/tmp"))));
        assert!(
            !permanent(&ScmError::Refused {
                code: Some(128),
                message: "index.lock exists".into(),
            }),
            "a locked index is a moment, not a machine"
        );
        assert!(!permanent(&ScmError::Unusable("interrupted".into())));
        assert!(remote_permanent(
            "source control is unavailable: `git` is not on this machine"
        ));
        assert!(remote_permanent(
            "the repository at /repo begins outside the served workspace /repo/sub"
        ));
        assert!(!remote_permanent("git exited with 128: index.lock exists"));
    }

    #[test]
    fn a_remote_status_and_write_are_collected_without_blocking_the_caller() {
        let mut session = session();
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        let mut scm = Scm {
            git: Git::default(),
            root: None,
            inflight: None,
            repo_root: None,
            unavailable: None,
            remote: Some(Remote {
                requests: request_tx,
                responses: response_rx,
                inflight: None,
                pending_operation: None,
            }),
            remote_workspace: Some(PathBuf::from("/remote/project")),
        };

        session.open(
            PathBuf::from("/remote/project/src/main.rs"),
            "fn main() {}\n",
        );

        scm.poll(&mut session);
        assert!(matches!(request_rx.try_recv(), Ok(RemoteRequest::Status)));
        assert!(!session.scm_wanted(), "the request is marked in flight");

        let status = deco_scm::parse(
            "# branch.oid 0123456789012345678901234567890123456789\0\
             # branch.head main\0\
             1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb src/main.rs\0",
        )
        .expect("git's own format");
        response_tx
            .send(RemoteResponse::Status(Ok((
                PathBuf::from("/remote/project"),
                status,
            ))))
            .unwrap();
        assert!(scm.poll(&mut session));
        assert_eq!(
            session.repository_root(),
            Some(std::path::Path::new("/remote/project"))
        );
        assert_eq!(session.scm_status().map(Status::changed), Some(1));

        let committed = match request_rx.try_recv() {
            Ok(RemoteRequest::Committed(path)) => path,
            _ => panic!("the open file's committed text should be next"),
        };
        assert_eq!(committed, PathBuf::from("/remote/project/src/main.rs"));
        response_tx
            .send(RemoteResponse::Committed {
                path: committed,
                result: Ok(Some("fn main() {}\n".to_owned())),
            })
            .unwrap();
        assert!(scm.poll(&mut session));

        let operation = Operation::Stage(PathBuf::from("src/main.rs"));
        scm.apply(&mut session, &operation);
        assert!(
            request_rx.try_recv().is_err(),
            "apply only queues; the next poll sends it"
        );
        scm.poll(&mut session);
        assert!(matches!(
            request_rx.try_recv(),
            Ok(RemoteRequest::Apply(ref sent)) if sent == &operation
        ));
        response_tx
            .send(RemoteResponse::Applied {
                operation: operation.clone(),
                result: Ok(()),
            })
            .unwrap();
        assert!(scm.poll(&mut session));
        assert_eq!(session.status.as_deref(), Some("staged main.rs"));
        assert!(!session.scm_wanted(), "the fresh status starts immediately");
        assert!(matches!(request_rx.try_recv(), Ok(RemoteRequest::Status)));
    }
}
