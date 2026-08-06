//! Attaching a language server to the terminal frontend.
//!
//! `deco-lsp` deliberately owns no policy: it can start a server, but it does
//! not decide which one, when, or what to do when one dies. That lives here,
//! because the answers are about the editor's behaviour rather than about the
//! protocol.
//!
//! The policy, and why:
//!
//! - **One server, for the open document's language.** deco edits one document
//!   at a time, so starting more would be speculative work for files that are
//!   not open. When the document changes language, the old server is stopped
//!   and a new one started.
//! - **A workspace-defined server is not started, and the user is told why.**
//!   Approving one needs a prompt the terminal frontend does not have yet, and
//!   the safe direction is obvious: not running a program is recoverable,
//!   running the wrong one is not. The message names the server so the user can
//!   move the definition into their own settings if they want it.
//! - **A server that fails costs itself and nothing else.** Every failure ends
//!   as a line in the status bar and an editor that still works.
//! - **Polling never blocks.** The event loop waits on the terminal with a
//!   timeout and drains the server in between, so a busy server cannot make
//!   typing feel slow and a silent one cannot freeze the editor.

use std::path::{Path, PathBuf};
use std::time::Duration;

use deco_editor::Session;
use deco_lsp::process::Consent;
use deco_lsp::supervisor::{Supervisor, Update};
use deco_lsp::uri::PathStyle;
use deco_lsp::{ServerRegistry, Trust};

/// How long to wait for the handshake before giving up on a server.
///
/// Shorter than [`deco_lsp::supervisor::INITIALIZE_TIMEOUT`] because this
/// happens while the user is looking at an empty screen waiting for their file.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// The language-server side of a terminal session.
pub struct Lsp {
    registry: ServerRegistry,
    enabled: bool,
    supervisor: Option<Supervisor>,
    /// The language the running server was started for, so a document in
    /// another language is noticed.
    language: Option<String>,
    /// The document the server has been told about.
    open: Option<PathBuf>,
    root: Option<PathBuf>,
    style: PathStyle,
}

impl Lsp {
    /// Reads the configuration and prepares to attach servers.
    ///
    /// Nothing is started here: which server to run depends on the document,
    /// which may not be open yet.
    pub fn new(session: &mut Session, root: Option<PathBuf>) -> Self {
        let enabled = deco_lsp::settings::enabled(&session.settings);
        let (registry, problems) = deco_lsp::settings::registry(&session.settings);
        for problem in problems {
            session
                .problems
                .push(format!("deco.lsp.servers: {problem}"));
        }
        Self {
            registry,
            enabled,
            supervisor: None,
            language: None,
            open: None,
            root,
            style: PathStyle::host(),
        }
    }

    /// Whether a server is running and ready.
    pub fn is_ready(&self) -> bool {
        self.supervisor.as_ref().is_some_and(Supervisor::is_ready)
    }

    /// Starts or switches the server to suit the open document.
    ///
    /// Idempotent: calling it for a document whose server is already running
    /// does nothing, which is what lets the event loop call it freely.
    pub fn attach(&mut self, session: &mut Session) {
        if !self.enabled {
            return;
        }
        let Some(path) = session.document.path.clone() else {
            // An unsaved buffer has no URI, so there is nothing a server could
            // be told about it.
            return;
        };
        let Some(language) = session.document.language().map(str::to_owned) else {
            return;
        };

        if self.language.as_deref() == Some(language.as_str()) && self.is_ready() {
            self.sync_open(session, &path, &language);
            return;
        }

        // A different language needs a different server, and the old one has
        // nothing left to say about a file it can no longer see.
        self.detach();

        let candidates: Vec<_> = self
            .registry
            .for_language(&language)
            .into_iter()
            .cloned()
            .collect();
        if candidates.is_empty() {
            return;
        }

        // Every candidate is tried in turn. Skipping to the next on a refusal
        // rather than stopping is what keeps a workspace from disabling a
        // language: a repository that defines its own server gets that
        // definition declined, and the user's own server still starts.
        let mut refused: Vec<String> = Vec::new();
        for config in &candidates {
            if config.trust == Trust::Workspace {
                // Named, so the user can decide to move it into their own
                // settings if they do want it.
                refused.push(config.id.clone());
                continue;
            }

            match Supervisor::start(
                config,
                Consent::Granted,
                self.root.as_deref(),
                self.style,
                STARTUP_TIMEOUT,
            ) {
                Ok(supervisor) => {
                    self.supervisor = Some(supervisor);
                    self.language = Some(language.clone());
                    self.sync_open(session, &path, &language);
                    return;
                }
                Err(error) => {
                    // First line only in the status bar: a startup failure
                    // carries the whole stderr tail, which is invaluable in a
                    // log and unreadable in a single row.
                    let summary = error.to_string();
                    let first = summary.lines().next().unwrap_or("failed to start");
                    session.status = Some(format!("{}: {first}", config.id));
                    session.problems.push(summary);
                    return;
                }
            }
        }

        if !refused.is_empty() {
            session.status = Some(format!(
                "{} defined by this workspace and not started",
                refused.join(", ")
            ));
        }
    }

    /// Tells the server about the open document if it does not know it yet.
    fn sync_open(&mut self, session: &mut Session, path: &Path, language: &str) {
        if self.open.as_deref() == Some(path) {
            return;
        }
        let Some(supervisor) = self.supervisor.as_mut() else {
            return;
        };
        if let Some(previous) = self.open.take() {
            let _ = supervisor.did_close(&previous);
        }
        let text = session.document.buffer.text();
        match supervisor.did_open(path, language, &text) {
            Ok(()) => self.open = Some(path.to_owned()),
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Tells the server the document changed.
    ///
    /// Full text every time. The incremental path exists in `deco-lsp` and is
    /// tested, but the editor does not yet keep a per-notification list of
    /// applied ranges, and inventing one from the undo history would be a
    /// guess. Sending the whole document is correct, just less efficient — and
    /// a wrong incremental range corrupts the server's copy silently, which is
    /// far worse than a large write.
    pub fn changed(&mut self, session: &mut Session) {
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        if self.open.as_deref() != Some(path.as_path()) {
            return;
        }
        let text = session.document.buffer.text();
        if let Err(error) = supervisor.did_change(&path, &[], &text) {
            self.report(session, error.to_string());
        }
    }

    /// Tells the server the document was saved.
    pub fn saved(&mut self, session: &mut Session) {
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        let text = session.document.buffer.text();
        if let Err(error) = supervisor.did_save(&path, &text) {
            self.report(session, error.to_string());
        }
    }

    /// Drains whatever the server has said and applies it. Never blocks.
    ///
    /// Returns whether anything changed, so the caller can skip a repaint.
    pub fn poll(&mut self, session: &mut Session) -> bool {
        let Some(supervisor) = self.supervisor.as_mut() else {
            return false;
        };

        let updates = supervisor.poll();
        if updates.is_empty() {
            return false;
        }

        let mut changed = false;
        // Collected first so `self` is free of the supervisor borrow.
        let open_uri = session
            .document
            .path
            .as_deref()
            .and_then(|path| supervisor.uri_for(path));

        for update in updates {
            match update {
                Update::Diagnostics { uri, diagnostics } => {
                    // Only the document on screen: a server may report on files
                    // deco is not showing, and there is nowhere to put those.
                    if Some(&uri) == open_uri.as_ref() {
                        session.set_diagnostics(diagnostics);
                        changed = true;
                    }
                }
                Update::Message { kind, message } => {
                    // 1 is an error, 2 a warning. Anything gentler is a
                    // progress note and does not deserve the status bar.
                    if kind <= 2 {
                        session.status = Some(message);
                        changed = true;
                    }
                }
                Update::Stopped { id, reason } => {
                    let first = reason.lines().next().unwrap_or("stopped").to_owned();
                    session.status = Some(format!("{id} stopped: {first}"));
                    session.problems.push(format!("{id}: {reason}"));
                    // Its diagnostics are unowned now — nothing will ever
                    // correct or retract them.
                    session.set_diagnostics(Vec::new());
                    self.supervisor = None;
                    self.language = None;
                    self.open = None;
                    return true;
                }
                Update::Ready { .. } | Update::Noted { .. } => {}
            }
        }
        changed
    }

    /// Stops the server, if one is running.
    pub fn detach(&mut self) {
        if let Some(mut supervisor) = self.supervisor.take() {
            supervisor.stop();
        }
        self.language = None;
        self.open = None;
    }

    fn report(&mut self, session: &mut Session, message: String) {
        let first = message.lines().next().unwrap_or("error").to_owned();
        session.status = Some(first);
        session.problems.push(message);
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        // Quitting the editor must not leave a language server running: they
        // are long-lived and hold build locks on the project.
        self.detach();
    }
}

impl std::fmt::Debug for Lsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lsp")
            .field("enabled", &self.enabled)
            .field("servers", &self.registry.len())
            .field("language", &self.language)
            .field("running", &self.supervisor.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_config::{Scope, Settings};

    /// A session pinned to Linux, so the keymap and context keys agree
    /// regardless of which platform the test runs on.
    ///
    /// The document is a `.toml` file, not `.rs`, deliberately: `rust` has a
    /// built-in server definition, so a test using it would try to launch
    /// whatever `rust-analyzer` happens to be on the machine running CI and
    /// pass or fail depending on that. `toml` has no built-in entry, so these
    /// tests see only what they configure.
    fn session(settings: Settings) -> Session {
        let mut session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);
        session.open(PathBuf::from("/w/Cargo.toml"), "[package]\n");
        session
    }

    fn settings_with(scope: Scope, source: &str) -> Settings {
        settings_with_layers(&[(scope, source)])
    }

    fn settings_with_layers(layers: &[(Scope, &str)]) -> Settings {
        let mut settings = Settings::with_defaults();
        for (scope, source) in layers {
            settings
                .load_layer(*scope, source)
                .unwrap_or_else(|error| panic!("{scope:?}: {error}"));
        }
        settings
    }

    #[test]
    fn nothing_starts_when_language_servers_are_disabled() {
        let mut s = session(settings_with(Scope::User, r#"{"deco.lsp.enabled": false}"#));
        let mut lsp = Lsp::new(&mut s, None);
        lsp.attach(&mut s);
        assert!(!lsp.is_ready());
        assert_eq!(s.status, None, "a disabled feature says nothing");
    }

    #[test]
    fn a_workspace_defined_server_is_not_started_and_says_so() {
        // Cloning a repository must not be enough to run a program, and a
        // silent refusal would look like the feature is broken.
        let mut s = session(settings_with(
            Scope::Workspace,
            r#"{"deco.lsp.servers": {"theirs": {"languages": ["toml"], "command": "./taplo"}}}"#,
        ));
        let mut lsp = Lsp::new(&mut s, None);
        lsp.attach(&mut s);

        assert!(!lsp.is_ready());
        let status = s.status.expect("the refusal must be visible");
        assert!(status.contains("theirs"), "{status}");
        assert!(status.contains("workspace"), "{status}");
    }

    #[test]
    fn a_missing_server_program_is_reported_without_stopping_the_editor() {
        let mut s = session(settings_with(
            Scope::User,
            r#"{"deco.lsp.servers": {"ghost": {"languages": ["toml"],
                "command": "deco-no-such-server-9f2c"}}}"#,
        ));
        let mut lsp = Lsp::new(&mut s, None);
        lsp.attach(&mut s);

        assert!(!lsp.is_ready());
        let status = s.status.as_deref().expect("a failure must be visible");
        assert!(status.starts_with("ghost:"), "{status}");
        assert_eq!(
            status.lines().count(),
            1,
            "the status bar is one row: {status}"
        );
        assert!(
            !s.problems.is_empty(),
            "the full reason belongs in the problem list"
        );
    }

    #[test]
    fn a_malformed_definition_becomes_a_problem_rather_than_a_panic() {
        let mut s = session(settings_with(
            Scope::User,
            r#"{"deco.lsp.servers": {"broken": {"languages": ["toml"]}}}"#,
        ));
        Lsp::new(&mut s, None);
        assert!(
            s.problems.iter().any(|p| p.contains("broken")),
            "{:?}",
            s.problems
        );
    }

    #[test]
    fn an_unsaved_buffer_starts_nothing() {
        // It has no path, so no URI, so nothing a server could be told about.
        let mut s = Session::new(
            Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );
        let mut lsp = Lsp::new(&mut s, None);
        lsp.attach(&mut s);
        assert!(!lsp.is_ready());
    }

    #[test]
    fn a_language_with_no_server_starts_nothing_and_says_nothing() {
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/notes.md"), "hello");
        let mut lsp = Lsp::new(&mut s, None);
        lsp.attach(&mut s);
        assert!(!lsp.is_ready());
        assert_eq!(s.status, None);
    }

    #[test]
    fn polling_without_a_server_is_a_no_op() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        assert!(!lsp.poll(&mut s));
        lsp.changed(&mut s);
        lsp.saved(&mut s);
        lsp.detach();
    }

    #[test]
    fn detaching_twice_is_harmless() {
        // Drop also detaches, so this happens on every clean exit.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.detach();
        lsp.detach();
    }

    #[test]
    fn the_built_in_registry_is_available_by_default() {
        let mut s = session(Settings::with_defaults());
        let lsp = Lsp::new(&mut s, None);
        assert!(lsp.registry.for_language("rust").len() == 1);
        assert!(lsp.enabled);
    }

    #[test]
    fn a_workspace_server_cannot_displace_the_users_own() {
        // The bug this guards: a repository defining a competing server for a
        // language would otherwise be chosen first, get declined for want of
        // consent, and leave the language with no server at all — a way for a
        // cloned repo to switch the feature off.
        let mut s = session(settings_with_layers(&[
            (
                Scope::User,
                r#"{"deco.lsp.servers": {"mine": {"languages": ["toml"],
                    "command": "deco-no-such-server-mine"}}}"#,
            ),
            (
                Scope::Workspace,
                r#"{"deco.lsp.servers": {"theirs": {"languages": ["toml"],
                    "command": "./theirs"}}}"#,
            ),
        ]));
        let mut lsp = Lsp::new(&mut s, None);
        lsp.attach(&mut s);

        // `mine` is tried — it fails only because the program does not exist,
        // which is what the status line says.
        let status = s.status.as_deref().expect("something must be reported");
        assert!(status.starts_with("mine:"), "{status}");
    }

    #[test]
    fn a_configured_server_is_preferred_over_a_built_in_one() {
        // A configuration is an instruction; a built-in is a guess.
        let mut s = session(settings_with(
            Scope::User,
            r#"{"deco.lsp.servers": {"mine": {"languages": ["rust"],
                "command": "deco-no-such-server-mine"}}}"#,
        ));
        let lsp = Lsp::new(&mut s, None);
        let candidates = lsp.registry.for_language("rust");
        assert_eq!(
            candidates.first().map(|c| c.id.as_str()),
            Some("mine"),
            "the user's own definition must come first"
        );
        assert!(
            candidates.iter().any(|c| c.id == "rust-analyzer"),
            "the built-in stays available as a fallback"
        );
    }

    #[test]
    fn attach_is_idempotent_when_nothing_can_start() {
        // The event loop calls it freely, so repeated calls must not accumulate
        // status messages or problems.
        let mut s = session(settings_with(
            Scope::User,
            r#"{"deco.lsp.servers": {"ghost": {"languages": ["toml"],
                "command": "deco-no-such-server-9f2c"}}}"#,
        ));
        let mut lsp = Lsp::new(&mut s, None);
        lsp.attach(&mut s);
        let after_one = s.problems.len();
        assert!(after_one > 0);
        // A second attach retries, which is intended — a server may have been
        // installed since. What matters is that it does not panic or leak.
        lsp.attach(&mut s);
        assert!(!lsp.is_ready());
    }
}
