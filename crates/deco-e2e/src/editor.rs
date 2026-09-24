//! A running editor, driven by keystrokes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use deco_editor::Session;
use deco_keymap::keys::{Chord, Key, NamedKey};
use deco_tui::app::{Driver, Flow, Options};

use crate::screen::Screen;
use crate::world::Scenario;

/// The interval between keystrokes, in milliseconds.
///
/// The editor receives the clock with each keystroke, so a scenario sets the
/// typing speed. 20ms is a fast but realistic rate. `deco-core` merges edits
/// within 500ms into one undo step, so a word typed at this rate is undone as
/// one word. A scenario that pressed every key at `now_ms = 0` would not detect
/// a regression in this behaviour.
const KEYSTROKE_MS: u64 = 20;

/// deco, started and waiting for a key.
pub struct Editor {
    session: Session,
    driver: Driver,
    now_ms: u64,
    workspace: PathBuf,
    size: (u16, u16),
    /// Whether the last keystroke asked the editor to quit.
    quit: bool,
}

impl Editor {
    /// Starts the editor in the same order as the binary: configuration, then
    /// files, then the event loop's setup.
    pub(crate) fn start(scenario: &Scenario, cli: deco::cli::Cli) -> anyhow::Result<Self> {
        Self::start_with(scenario, cli, None)
    }

    /// Like [`Editor::start`], with the session's files read through `remote`.
    pub(crate) fn start_with(
        scenario: &Scenario,
        cli: deco::cli::Cli,
        mut remote: Option<deco_tui::RemoteSession>,
    ) -> anyhow::Result<Self> {
        let boot = scenario.boot();
        // The same order as the binary. The remote's machine settings are a
        // settings layer, so they must be fetched before the session resolves
        // the theme and builds the keymap.
        let remote_settings = match remote.as_mut() {
            Some(remote) if remote.client.serves("settings.read") => {
                remote.client.machine_settings()?.1
            }
            _ => None,
        };
        let mut session = deco::startup::session(&cli, &boot, remote_settings.as_deref());
        if let Some(remote) = remote.as_mut() {
            // Fetched through the connection rather than read from disk, as the
            // binary does in a remote session. The documents therefore have the
            // remote side's relative paths.
            for path in &cli.files {
                let text = remote.client.read(&path.display().to_string())?;
                session.open(path.clone(), &text);
            }
        } else {
            deco::startup::open_local(&mut session, &cli.files, &boot)?;
        }
        deco::startup::focus_first(&mut session, cli.files.len());

        // Made absolute here, as the binary's working directory would make it.
        // The driver resolves a relative path against the process's working
        // directory, but a scenario's working directory is a field, not a
        // property of the process.
        let started_with = cli
            .files
            .first()
            .map(|path| deco::startup::absolute(path, boot.cwd.as_deref()));

        let driver = Driver::start(
            &mut session,
            Options {
                started_with,
                remote,
                extension_roots: scenario.extension_roots(),
                // Under the scenario's home, so a permission answer is written
                // to the scenario's own directory. A second launch of the same
                // scenario can check that the answer was saved.
                permissions_file: Some(scenario.home().join("deco/permissions.json")),
                home: Some(scenario.home().to_path_buf()),
                cwd: boot.cwd.clone(),
                size: scenario.terminal_size(),
            },
        );

        Ok(Self {
            session,
            driver,
            now_ms: 0,
            // The directory `on_disk` reads. For a remote session this is the
            // directory the server serves.
            workspace: scenario.served_workspace(),
            size: scenario.terminal_size(),
            quit: false,
        })
    }

    // ---- pressing keys ----------------------------------------------------

    /// Presses one chord, written as in `keybindings.json`: `ctrl+shift+p`,
    /// `f12`, `escape`, `alt+up`.
    ///
    /// The keymap's parser reads the chord, which is then converted to the
    /// event a terminal would send, so the translation layer in
    /// `deco-tui::keys` is also tested. A two-chord sequence such as
    /// `ctrl+k ctrl+t` is two calls, or one call to [`Editor::press_all`].
    pub fn press(&mut self, chord: &str) -> &mut Self {
        let parsed = Chord::parse(chord).unwrap_or_else(|error| panic!("`{chord}`: {error}"));
        self.send(to_event(parsed))
    }

    /// Presses several chords in order, given as one whitespace-separated
    /// string: `editor.press_all("ctrl+k ctrl+t")`.
    pub fn press_all(&mut self, chords: &str) -> &mut Self {
        for chord in chords.split_whitespace() {
            self.press(chord);
        }
        self
    }

    /// Presses the same chord `times` times.
    pub fn press_times(&mut self, chord: &str, times: usize) -> &mut Self {
        for _ in 0..times {
            self.press(chord);
        }
        self
    }

    /// Types text, one character at a time, as a terminal reports it.
    ///
    /// A newline is sent as the Enter key and a tab as the Tab key, because a
    /// keyboard cannot type a literal `\n` into a document.
    pub fn type_text(&mut self, text: &str) -> &mut Self {
        for character in text.chars() {
            let event = match character {
                '\n' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                '\t' => KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                // As crossterm delivers it: the produced character with no
                // inferred modifier. Treating an uppercase letter as Shift is
                // done by `deco-tui::keys`, not by this harness.
                other => KeyEvent::new(KeyCode::Char(other), KeyModifiers::NONE),
            };
            self.send(event);
        }
        self
    }

    /// Opens the command palette, types `name`, and accepts the first match.
    /// This is how a user runs a command that has no keybinding.
    pub fn palette(&mut self, name: &str) -> &mut Self {
        self.press("ctrl+shift+p");
        self.type_text(name);
        self.press("enter")
    }

    /// Opens quick open, types `name`, and accepts the first match.
    pub fn quick_open(&mut self, name: &str) -> &mut Self {
        self.press("ctrl+p");
        self.type_text(name);
        self.press("enter")
    }

    /// Advances the editor's clock by `ms` without any key input.
    ///
    /// This runs the idle path and advances only the clock passed to the
    /// editor; it does not sleep. Use it for `files.autoSave: "afterDelay"`,
    /// where a scenario simulates a minute passing without waiting a minute.
    ///
    /// Do not use it to wait for a language server. The server is a separate
    /// process that needs real time to answer, and a loop of `wait` finishes in
    /// microseconds without collecting anything, which looks the same as a
    /// broken feature. Use [`Editor::settle_until`] instead.
    pub fn wait(&mut self, ms: u64) -> &mut Self {
        self.now_ms += ms;
        self.driver
            .idle(&mut self.session, self.now_ms)
            .expect("the idle path should not fail");
        self
    }

    /// Polls until `ready` returns true, or panics with a message naming `what`.
    ///
    /// A language server is a separate process connected through a pipe, so its
    /// response arrives at an unpredictable time and no keystroke can force it.
    /// This runs the editor's idle path in a loop, the same poll that collects
    /// diagnostics while the user is idle.
    ///
    /// Unlike [`Editor::wait`], it sleeps in real time, because a subprocess
    /// needs real time. The editor's clock advances only slightly, so waiting
    /// for a response does not unintentionally trigger the auto-save delay.
    ///
    /// The time limit is long and a timeout panics with details. A short limit
    /// would make a scenario fail on a loaded machine and pass on an idle one.
    #[track_caller]
    pub fn settle_until(&mut self, what: &str, ready: impl Fn(&Editor) -> bool) -> &mut Self {
        const STEP: Duration = Duration::from_millis(5);
        const BUDGET: Duration = Duration::from_secs(10);

        let started = Instant::now();
        loop {
            if ready(self) {
                return self;
            }
            if started.elapsed() > BUDGET {
                panic!(
                    "waited {BUDGET:?} for {what} and it never happened.\n\
                     status: {:?}\nproblems: {:?}",
                    self.status(),
                    self.problems()
                );
            }
            std::thread::sleep(STEP);
            self.now_ms += STEP.as_millis() as u64;
            self.driver
                .idle(&mut self.session, self.now_ms)
                .expect("the idle path should not fail");
        }
    }

    /// Waits until the language server has started and completed initialization.
    ///
    /// Every language server scenario calls this first. Capabilities are
    /// unknown until the handshake completes, and without them keys such as
    /// `f12` are not bound.
    #[track_caller]
    pub fn settle_lsp(&mut self) -> &mut Self {
        self.settle_until("the language server to be ready", |editor| {
            editor.driver.lsp().is_ready()
        })
    }

    /// The terminal was resized.
    pub fn resize(&mut self, width: u16, height: u16) -> &mut Self {
        self.size = (width, height);
        self.driver.resize(&mut self.session, width, height);
        self
    }

    fn send(&mut self, event: KeyEvent) -> &mut Self {
        self.now_ms += KEYSTROKE_MS;
        let Some(chord) = deco_tui::keys::chord_from_event(event) else {
            // A terminal event without a key. The editor ignores it, so this
            // does too.
            return self;
        };
        match self
            .driver
            .key(&mut self.session, chord, self.now_ms)
            .expect("a keystroke should not fail")
        {
            Flow::Quit => self.quit = true,
            Flow::Continue => {}
        }
        self
    }

    // ---- looking at it ----------------------------------------------------

    /// The screen as it would be painted now.
    pub fn screen(&mut self) -> Screen {
        Screen::of(self.driver.frame(&mut self.session), self.size)
    }

    /// The status message, if there is one.
    pub fn status(&self) -> Option<&str> {
        self.session.status.as_deref()
    }

    /// The problems reported by startup and the editor.
    pub fn problems(&self) -> &[String] {
        &self.session.problems
    }

    /// The text of the document that is showing.
    pub fn text(&self) -> String {
        self.session.document.buffer.text()
    }

    /// The path of the document that is showing.
    pub fn path(&self) -> Option<&Path> {
        self.session.document.path.as_deref()
    }

    /// Whether the showing document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.session.document.dirty
    }

    /// Whether the last keystroke asked the editor to quit.
    pub fn has_quit(&self) -> bool {
        self.quit
    }

    /// The driver, for assertions about frontend state rather than the session:
    /// a received hover, an open completion list, a ready language server.
    pub fn driver(&self) -> &Driver {
        &self.driver
    }

    /// The session, for assertions that cannot be made on the screen.
    pub fn session(&self) -> &Session {
        &self.session
    }

    // ---- looking at the disk ----------------------------------------------

    /// A file in the workspace, as it is on disk right now.
    ///
    /// Use this to check a save. The editor's in-memory document does not show
    /// what was written to disk.
    pub fn on_disk(&self, relative: &str) -> String {
        let path = self.workspace.join(relative);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
    }

    /// A file in the workspace as raw bytes, for checking line endings and
    /// encodings that a `String` comparison would hide.
    pub fn on_disk_bytes(&self, relative: &str) -> Vec<u8> {
        let path = self.workspace.join(relative);
        std::fs::read(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
    }

    /// Whether a file exists in the workspace.
    pub fn exists(&self, relative: &str) -> bool {
        self.workspace.join(relative).exists()
    }

    /// Writes a file directly on disk, as another program would, without going
    /// through the editor.
    pub fn change_on_disk(&self, relative: &str, contents: &str) {
        let path = self.workspace.join(relative);
        std::fs::write(&path, contents).unwrap_or_else(|error| {
            panic!("writing {}: {error}", path.display());
        });
    }

    /// The workspace directory, for scenarios that need absolute paths.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

/// The terminal event that produces `chord`.
///
/// The inverse of `deco_tui::keys::chord_from_event`. A scenario writes a
/// keystroke as in `keybindings.json`, and this converts it to the event a
/// terminal would send, so the real translation runs on input.
fn to_event(chord: Chord) -> KeyEvent {
    let mut modifiers = KeyModifiers::NONE;
    if chord.modifiers.ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if chord.modifiers.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    if chord.modifiers.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if chord.modifiers.meta {
        modifiers |= KeyModifiers::SUPER;
    }
    let code = match chord.key {
        Key::Char(c) => KeyCode::Char(c),
        Key::Named(NamedKey::Enter) => KeyCode::Enter,
        Key::Named(NamedKey::Tab) => KeyCode::Tab,
        Key::Named(NamedKey::Backspace) => KeyCode::Backspace,
        Key::Named(NamedKey::Delete) => KeyCode::Delete,
        Key::Named(NamedKey::Insert) => KeyCode::Insert,
        Key::Named(NamedKey::Escape) => KeyCode::Esc,
        Key::Named(NamedKey::Left) => KeyCode::Left,
        Key::Named(NamedKey::Right) => KeyCode::Right,
        Key::Named(NamedKey::Up) => KeyCode::Up,
        Key::Named(NamedKey::Down) => KeyCode::Down,
        Key::Named(NamedKey::Home) => KeyCode::Home,
        Key::Named(NamedKey::End) => KeyCode::End,
        Key::Named(NamedKey::PageUp) => KeyCode::PageUp,
        Key::Named(NamedKey::PageDown) => KeyCode::PageDown,
        Key::Named(NamedKey::F(n)) => KeyCode::F(n),
        other => panic!("no terminal event stands for {other:?}"),
    };
    KeyEvent::new(code, modifiers)
}
