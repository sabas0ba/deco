//! Running `git status` without making the editor wait for it.
//!
//! [`deco_scm`] is blocking: it spawns `git`, waits, and parses the output. On
//! deco's own checkout that takes a few milliseconds, but on a working tree with
//! a million files it takes much longer. The wait therefore happens on a thread
//! and the result is collected later, as with the language server's stdio pump.
//!
//! This module decides *when* to run:
//!
//! - **Only when the session requests it.** [`Session::scm_wanted`] is set by a
//!   save or a file operation, never by a keystroke. Spawning a process per
//!   character would be too expensive, and a status bar that is briefly stale
//!   after a write is acceptable.
//! - **One at a time.** Two concurrent runs would race to fill the same field,
//!   and the run that finished last would determine what is shown. If something
//!   changes while a run is in flight, the flag is still set when the run
//!   finishes, so the next poll starts a new run.
//! - **Permanent unavailability is remembered.** A missing `git` executable and
//!   a folder that is not a repository do not change during the session, so
//!   they are not checked again on every save. Other errors, such as git
//!   rejecting the command or unparseable output, are transient and are retried.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use deco_editor::Session;
use deco_scm::{
    Branch, CheckoutPlan, Comparison, ComparisonRequest, Git, Operation, ScmError, Status,
};

enum CheckoutQuery {
    Branches,
    Plan(String),
}

enum CheckoutAnswer {
    Branches(Result<Vec<Branch>, ScmError>),
    Plan(Result<CheckoutPlan, ScmError>),
}

/// Work sent to the dedicated remote-SCM connection.
enum RemoteRequest {
    Status,
    Committed(PathBuf),
    Comparison(ComparisonRequest),
    Branches,
    CheckoutPlan(String),
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
    Comparison {
        request: ComparisonRequest,
        result: Result<Comparison, String>,
    },
    Branches(Result<Vec<Branch>, String>),
    CheckoutPlan(Result<CheckoutPlan, String>),
    Applied {
        operation: Operation,
        result: Result<(), String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteInFlight {
    Status,
    Committed,
    Comparison,
    CheckoutQuery,
    Apply,
}

/// A second connection whose worker owns every blocking remote git call.
///
/// File reads and extension requests keep using the session's primary
/// connection. A slow status walk or commit hook on this connection therefore
/// does not block drawing or those requests.
struct Remote {
    requests: Sender<RemoteRequest>,
    responses: Receiver<RemoteResponse>,
    inflight: Option<RemoteInFlight>,
    pending_operation: Option<Operation>,
    pending_comparison: Option<ComparisonRequest>,
    pending_checkout: Option<CheckoutQuery>,
    comparison_supported: bool,
    checkout_supported: bool,
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
        let comparison_supported = client.serves("scm.comparison");
        let checkout_supported = client.serves("scm.branches") && client.serves("scm.checkoutPlan");

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
                    RemoteRequest::Comparison(request) => RemoteResponse::Comparison {
                        result: client
                            .scm_comparison(&request)
                            .map_err(|error| error.to_string()),
                        request,
                    },
                    RemoteRequest::Branches => RemoteResponse::Branches(
                        client.scm_branches().map_err(|error| error.to_string()),
                    ),
                    RemoteRequest::CheckoutPlan(target) => RemoteResponse::CheckoutPlan(
                        client
                            .scm_checkout_plan(&target)
                            .map_err(|error| error.to_string()),
                    ),
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
            pending_comparison: None,
            pending_checkout: None,
            comparison_supported,
            checkout_supported,
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
    /// The working tree to query. `None` when deco was started without a
    /// workspace, because a single file has no repository to report on.
    root: Option<PathBuf>,
    /// The run that has not finished yet.
    inflight: Option<Receiver<Result<Status, ScmError>>>,
    /// A local diff comparison that has not finished yet.
    comparison: Option<Receiver<(ComparisonRequest, Result<Comparison, ScmError>)>>,
    /// A local branch listing or checkout preview that has not finished yet.
    checkout: Option<Receiver<CheckoutAnswer>>,
    /// The repository root, once it has been queried.
    ///
    /// Not the same as [`Scm::root`], which is the folder deco was started in.
    /// Opening a subdirectory of a repository is common, and every path git
    /// reports or accepts is relative to the repository root, so the two must be
    /// kept separate.
    repo_root: Option<PathBuf>,
    /// Why status will never be available, once that is known.
    ///
    /// Stored but not shown. The panel that would hold an output view is
    /// [built and empty](https://github.com/sabas0ba/deco/blob/main/docs/chrome.md),
    /// and a status bar message such as "this folder is not a repository"
    /// would be unnecessary noise for every folder that is not a repository.
    /// The accessor keeps the reason available and lets tests check it.
    unavailable: Option<String>,
    /// Present when git runs through the remote protocol rather than locally.
    remote: Option<Remote>,
    /// The remote workspace, used to keep repository paths in the same
    /// absolute-or-relative form as the session's document paths.
    remote_workspace: Option<PathBuf>,
}

impl Scm {
    /// A runner for `root`, using the executable named by `git.path`.
    ///
    /// The setting comes from VS Code. It is read once here rather than at each
    /// spawn.
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
            comparison: None,
            checkout: None,
            unavailable: None,
            remote: None,
            remote_workspace: None,
        }
    }

    /// A runner whose git process and repository are on the remote machine.
    pub fn remote(client: deco_remote::Client, workspace: PathBuf) -> Self {
        match Remote::new(client) {
            Ok(remote) => Self {
                git: Git::default(),
                root: None,
                repo_root: None,
                inflight: None,
                comparison: None,
                checkout: None,
                unavailable: None,
                remote: Some(remote),
                remote_workspace: Some(workspace),
            },
            Err(error) => Self {
                git: Git::default(),
                root: None,
                repo_root: None,
                inflight: None,
                comparison: None,
                checkout: None,
                unavailable: Some(error),
                remote: None,
                remote_workspace: Some(workspace),
            },
        }
    }

    /// The repository root, queried from git on first use.
    ///
    /// Every path in the view is repository-relative, so operations such as
    /// opening or staging a file need this root rather than the folder deco was
    /// started in. It was previously resolved only inside the gutter's fetch.
    /// With `git.decorations.enabled` off, or a workspace opened with no file,
    /// it was never resolved, and staging `sub/a.rs` from `/repo/sub` used
    /// `/repo/sub/sub/a.rs`.
    fn repository_root(&mut self, session: &mut Session) -> Option<PathBuf> {
        if let Some(found) = self.repo_root.clone() {
            return Some(found);
        }
        let root = self.root.clone()?;
        match self.git.root(&root) {
            Ok(found) => {
                self.repo_root = Some(found.clone());
                // The session also needs it to convert a row's path into a
                // path it can open.
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

    /// Why status is permanently unavailable. `None` while it may still become
    /// available.
    pub fn unavailable(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// Starts a run if one is wanted, and collects one that has finished.
    ///
    /// Returns whether the session changed, so the loop knows when to redraw.
    /// Collecting happens first, so a save made while git was running starts
    /// its own run in the same poll rather than the next one.
    pub fn poll(&mut self, session: &mut Session) -> bool {
        if self.remote.is_some() {
            return self.poll_remote(session);
        }
        // Compatibility with a remote server that predates the SCM methods.
        // There is intentionally no local root to run against, but the new
        // session's request still has to be marked as handled once.
        if self.root.is_none() && self.unavailable.is_some() && session.scm_wanted() {
            session.scm_started();
            return false;
        }
        let changed = self.collect(session);
        self.start(session);
        changed
            | self.collect_comparison(session)
            | self.collect_checkout(session)
            | self.fetch_committed(session)
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
                // Remote documents keep the path form used to open them. The
                // common CLI form is relative (`src/main.rs`). Giving its SCM row
                // an absolute root would open the same file a second time under
                // a different PathBuf. Keep absolute paths only when the active
                // document already uses them.
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
                // A rejected request is not necessarily permanent: an index
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
                // Same as the local path on error: showing no committed text is
                // safer than drawing a gutter against guessed contents.
                session.fill_committed(path, result.unwrap_or(None));
                changed = true;
            }
            Some(RemoteResponse::Comparison { request, result }) => {
                match result {
                    Ok(comparison) => session.open_comparison(request, comparison),
                    Err(error) => session.comparison_failed(&error),
                }
                changed = true;
            }
            Some(RemoteResponse::Branches(result)) => {
                match result {
                    Ok(branches) => session.offer_branches(branches),
                    Err(error) => session.git_checkout_failed(&error),
                }
                changed = true;
            }
            Some(RemoteResponse::CheckoutPlan(result)) => {
                match result {
                    Ok(plan) => session.confirm_checkout(plan),
                    Err(error) => session.git_checkout_failed(&error),
                }
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

        if let Some(query) = remote.pending_checkout.take() {
            let request = match query {
                CheckoutQuery::Branches => RemoteRequest::Branches,
                CheckoutQuery::Plan(target) => RemoteRequest::CheckoutPlan(target),
            };
            if remote.requests.send(request).is_ok() {
                remote.inflight = Some(RemoteInFlight::CheckoutQuery);
            } else {
                self.unavailable = Some("the remote source-control connection stopped".into());
                session.git_checkout_failed("the remote connection stopped");
                changed = true;
            }
            return changed;
        }

        if let Some(request) = remote.pending_comparison.take() {
            if remote
                .requests
                .send(RemoteRequest::Comparison(request.clone()))
                .is_ok()
            {
                remote.inflight = Some(RemoteInFlight::Comparison);
            } else {
                self.unavailable = Some("the remote source-control connection stopped".into());
                session.comparison_failed("the remote connection stopped");
                changed = true;
            }
            return changed;
        }

        if session.scm_wanted() {
            // Cleared before the request begins, so a save made while the remote
            // side is still walking the working tree sets the flag again.
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
    /// Fetches one file per poll rather than all at once, because each fetch is
    /// a separate process. The file being viewed is requested first, so its
    /// gutter fills in immediately and the others follow over the next few
    /// polls. Unlike status, this is blocking: `git show` of one blob reads one
    /// object rather than walking the working tree, so a thread and a second
    /// channel are not needed.
    fn fetch_committed(&mut self, session: &mut Session) -> bool {
        if self.unavailable.is_some() {
            return false;
        }
        let Some(path) = session.committed_wanted() else {
            return false;
        };
        // Queried once and cached. Without it every path would be stripped
        // against the folder deco was started in. That folder is the repository
        // root only when no subdirectory was opened. Otherwise the fetched blob
        // would belong to a different file, with no error.
        let Some(repo_root) = self.repository_root(session) else {
            // Record an empty result rather than leaving the request pending. If
            // the repository root cannot be determined, the committed text
            // cannot be read either, and leaving it pending would repeat the
            // query on every poll for the rest of the session.
            session.fill_committed(path, None);
            return true;
        };
        // The cache is keyed by the editor's path, but git uses paths relative
        // to the repository. A file outside the repository, for example one
        // opened with `ctrl+o`, gets an empty result. Recording that result
        // prevents a query on every poll.
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
        // Resolve the repository root instead of falling back to the workspace
        // folder. Operation paths are repository-relative. Running them from the
        // wrong directory does not necessarily fail: the path may name a missing
        // file or a different existing file.
        let Some(root) = self.repository_root(session) else {
            session.git_operation_failed(operation, "there is no repository here");
            return;
        };
        match self.git.apply(&root, operation) {
            Ok(()) => session.git_operation_done(operation),
            Err(error) => session.git_operation_failed(operation, &error.to_string()),
        }
    }

    /// Starts a source-control comparison without waiting on the event loop.
    pub fn compare(&mut self, session: &mut Session, request: ComparisonRequest) {
        if let Some(remote) = self.remote.as_mut() {
            if !remote.comparison_supported {
                session.comparison_failed(
                    "the remote server does not support diff views — update it to this deco version",
                );
            } else if remote.pending_comparison.is_some()
                || remote.inflight == Some(RemoteInFlight::Comparison)
            {
                session.comparison_failed("another diff is already being opened");
            } else {
                remote.pending_comparison = Some(request);
            }
            return;
        }
        if self.comparison.is_some() {
            session.comparison_failed("another diff is already being opened");
            return;
        }
        let Some(root) = self.repository_root(session) else {
            session.comparison_failed("there is no repository here");
            return;
        };
        let (sender, receiver) = mpsc::channel();
        let git = self.git.clone();
        std::thread::spawn(move || {
            let result = git.comparison(&root, &request);
            let _ = sender.send((request, result));
        });
        self.comparison = Some(receiver);
    }

    /// Fetches local branches for the checkout picker.
    pub fn branches(&mut self, session: &mut Session) {
        self.start_checkout_query(session, CheckoutQuery::Branches);
    }

    /// Fetches the cost of switching to `target` before asking for confirmation.
    pub fn checkout_plan(&mut self, session: &mut Session, target: String) {
        self.start_checkout_query(session, CheckoutQuery::Plan(target));
    }

    fn start_checkout_query(&mut self, session: &mut Session, query: CheckoutQuery) {
        if let Some(remote) = self.remote.as_mut() {
            if !remote.checkout_supported {
                session.git_checkout_failed(
                    "the remote server does not support branch switching — update it to this deco version",
                );
            } else if remote.pending_checkout.is_some()
                || remote.inflight == Some(RemoteInFlight::CheckoutQuery)
            {
                session.git_checkout_failed("another branch request is already running");
            } else {
                remote.pending_checkout = Some(query);
            }
            return;
        }
        if self.checkout.is_some() {
            session.git_checkout_failed("another branch request is already running");
            return;
        }
        let Some(root) = self.repository_root(session) else {
            session.git_checkout_failed("there is no repository here");
            return;
        };
        let (sender, receiver) = mpsc::channel();
        let git = self.git.clone();
        std::thread::spawn(move || {
            let answer = match query {
                CheckoutQuery::Branches => CheckoutAnswer::Branches(git.branches(&root)),
                CheckoutQuery::Plan(target) => {
                    CheckoutAnswer::Plan(git.checkout_plan(&root, &target))
                }
            };
            let _ = sender.send(answer);
        });
        self.checkout = Some(receiver);
    }

    fn collect_checkout(&mut self, session: &mut Session) -> bool {
        let Some(receiver) = self.checkout.as_ref() else {
            return false;
        };
        match receiver.try_recv() {
            Ok(CheckoutAnswer::Branches(Ok(branches))) => {
                self.checkout = None;
                session.offer_branches(branches);
                true
            }
            Ok(CheckoutAnswer::Plan(Ok(plan))) => {
                self.checkout = None;
                session.confirm_checkout(plan);
                true
            }
            Ok(CheckoutAnswer::Branches(Err(error))) | Ok(CheckoutAnswer::Plan(Err(error))) => {
                self.checkout = None;
                session.git_checkout_failed(&error.to_string());
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.checkout = None;
                session.git_checkout_failed("the branch request stopped");
                true
            }
        }
    }

    /// Collects a local source-control comparison, if it has finished.
    fn collect_comparison(&mut self, session: &mut Session) -> bool {
        let Some(receiver) = self.comparison.as_ref() else {
            return false;
        };
        match receiver.try_recv() {
            Ok((request, Ok(comparison))) => {
                self.comparison = None;
                session.open_comparison(request, comparison);
                true
            }
            Ok((_, Err(error))) => {
                self.comparison = None;
                session.comparison_failed(&error.to_string());
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.comparison = None;
                session.comparison_failed("the source-control comparison stopped");
                true
            }
        }
    }

    /// Takes the result, if one is waiting.
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
                // A failed run still completes the session's request. Without
                // this the flag would stay set and every poll would spawn
                // another git process.
                session.fill_scm(None);
                if permanent(&error) {
                    self.unavailable = Some(error.to_string());
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            // The thread exited without sending, which means a panic in the
            // parser. This is a bug, not a repository state. It is treated as
            // an empty result so the editor continues and the flag does not
            // trigger a new run on every poll.
            Err(TryRecvError::Disconnected) => {
                self.inflight = None;
                session.fill_scm(None);
                true
            }
        }
    }

    /// Spawns a run if the session wants one and no run is blocked.
    fn start(&mut self, session: &mut Session) {
        if self.inflight.is_some() || self.unavailable.is_some() || !session.scm_wanted() {
            return;
        }
        let Some(root) = self.root.clone() else {
            // No workspace, so there is nothing to query. The request is still
            // marked as started, otherwise every poll would return here.
            session.scm_started();
            return;
        };
        // Clear the flag before starting the thread, not after it finishes, so
        // a save during this run sets the flag again and is detected.
        session.scm_started();
        let (sender, receiver) = mpsc::channel();
        let git = self.git.clone();
        // Detached: nothing joins the thread. It only uses the channel, and
        // dropping the receiver on shutdown ends communication. A `git status`
        // that outlives the editor by a few milliseconds does not need a
        // shutdown handshake.
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
/// A missing git executable and a missing repository do not change after the
/// next save, so checking again would spawn a process for a known result. Other
/// errors can be temporary: a repository in the middle of a rebase can reject a
/// command, and a `git status` that failed because the index was locked can
/// succeed on the next try.
fn permanent(error: &ScmError) -> bool {
    matches!(error, ScmError::NoBinary(_) | ScmError::NotARepository(_))
}

/// The remote protocol sends errors as text, so the three states that cannot
/// change during this server's lifetime are recognised by their messages here.
fn remote_permanent(error: &str) -> bool {
    error.contains("is not on this machine")
        || error.contains("is not inside a git repository")
        || error.contains("outside the served workspace")
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

    /// Polls until the run finishes, so a test does not depend on how fast `git`
    /// is on the machine running it.
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
        // A contributor's machine may have no git. Skip with a message instead
        // of failing for a reason unrelated to their change.
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

        // A later change must not start a second process.
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

        // A run is in flight, and the file is written during the run.
        scm.poll(&mut session);
        assert!(scm.inflight.is_some(), "the first poll starts one");
        session.scm_changed();

        // When the first run finishes, a second run must start. Otherwise the
        // bar shows a status taken before the write, and no new query runs
        // until the *next* save.
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
        // This file, which is committed, opened with uncommitted content.
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
        // `ctrl+o` can open any path. HEAD has no content for a file outside the
        // repository, and recording an empty result prevents a query on every
        // poll for the rest of the session.
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
        assert!(remote_permanent(
            "Git metadata at /repo/.git lies outside the served workspace /worktree"
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
            comparison: None,
            checkout: None,
            repo_root: None,
            unavailable: None,
            remote: Some(Remote {
                requests: request_tx,
                responses: response_rx,
                inflight: None,
                pending_operation: None,
                pending_comparison: None,
                pending_checkout: None,
                comparison_supported: true,
                checkout_supported: true,
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
