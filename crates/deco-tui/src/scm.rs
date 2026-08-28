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
use std::sync::mpsc::{self, Receiver, TryRecvError};

use deco_editor::Session;
use deco_scm::{Git, ScmError, Status};

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
        let changed = self.collect(session);
        self.start(session);
        changed | self.fetch_committed(session)
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
        let Some(root) = self.root.clone() else {
            return false;
        };
        let Some(path) = session.committed_wanted() else {
            return false;
        };
        // Asked once and kept. Without it every path would be stripped against
        // the folder deco was started in, which is only the repository root
        // when nobody opened a subdirectory — and when they did, the blob
        // fetched would be a different file's, silently.
        if self.repo_root.is_none() {
            match self.git.root(&root) {
                Ok(found) => self.repo_root = Some(found),
                Err(error) => {
                    if permanent(&error) {
                        self.unavailable = Some(error.to_string());
                    }
                    // Answered rather than left standing: a repository that
                    // cannot say where it begins cannot say what a file used
                    // to hold either, and the alternative is asking on every
                    // poll for the rest of the session.
                    session.fill_committed(path, None);
                    return true;
                }
            }
        }
        let repo_root = self.repo_root.clone().unwrap_or_else(|| root.clone());
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
    }
}
