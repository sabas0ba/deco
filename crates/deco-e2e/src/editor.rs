//! A running editor, driven by keystrokes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use deco_editor::Session;
use deco_keymap::keys::{Chord, Key, NamedKey};
use deco_tui::app::{Driver, Flow, Options};

use crate::screen::Screen;
use crate::world::Scenario;

/// How much later the next keystroke is, in milliseconds.
///
/// The editor's clock is handed to it per keystroke, so a scenario decides how
/// fast its user types. 20ms is a brisk but ordinary rate, and it matters:
/// `deco-core` coalesces edits within 500ms into one undo step, so typing a word
/// at this rate undoes as a word — which is what a person expects, and what a
/// scenario that pressed every key at `now_ms = 0` would never notice breaking.
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
    /// Starts the editor the way the binary does: configuration, then files,
    /// then the event loop's own setup.
    pub(crate) fn start(scenario: &Scenario, cli: deco::cli::Cli) -> anyhow::Result<Self> {
        Self::start_with(scenario, cli, None)
    }

    /// The same, with the session's files on the other end of `remote`.
    pub(crate) fn start_with(
        scenario: &Scenario,
        cli: deco::cli::Cli,
        mut remote: Option<deco_tui::RemoteSession>,
    ) -> anyhow::Result<Self> {
        let boot = scenario.boot();
        let mut session = deco::startup::session(&cli, &boot);
        if let Some(remote) = remote.as_mut() {
            // Fetched through the connection rather than read from disk, which is
            // what the binary does in a remote session — and what makes the
            // documents here carry the far end's relative paths.
            for path in &cli.files {
                let text = remote.client.read(&path.display().to_string())?;
                session.open(path.clone(), &text);
            }
        } else {
            deco::startup::open_local(&mut session, &cli.files, &boot)?;
        }
        deco::startup::focus_first(&mut session, cli.files.len());

        // Absolute, which is what the binary's own working directory would have
        // made of it: the driver resolves this one against the process's working
        // directory, and a scenario's working directory is a field rather than a
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
                home: Some(scenario.home().to_path_buf()),
                cwd: boot.cwd.clone(),
                size: scenario.terminal_size(),
            },
        );

        Ok(Self {
            session,
            driver,
            now_ms: 0,
            // Where `on_disk` looks. For a remote session that is the directory
            // the *server* is serving, which may not be this machine's.
            workspace: scenario.served_workspace(),
            size: scenario.terminal_size(),
            quit: false,
        })
    }

    // ---- pressing keys ----------------------------------------------------

    /// Presses one chord, spelled the way `keybindings.json` spells it —
    /// `ctrl+shift+p`, `f12`, `escape`, `alt+up`.
    ///
    /// The spelling is parsed by the keymap's own parser and then turned back
    /// into the terminal event a terminal would send, so the translation layer
    /// in `deco-tui::keys` is on the path too. A two-chord sequence such as
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
    /// A newline is the Enter key, and a tab is the Tab key — because that is
    /// what pressing them produces, and because typing a literal `\n` into a
    /// document is not something a keyboard can do.
    pub fn type_text(&mut self, text: &str) -> &mut Self {
        for character in text.chars() {
            let event = match character {
                '\n' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                '\t' => KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                // Exactly what crossterm delivers: the character that was
                // produced, with no modifier inferred from it. An uppercase
                // letter implying Shift is `deco-tui::keys`' rule to apply, not
                // this harness's to pre-empt.
                other => KeyEvent::new(KeyCode::Char(other), KeyModifiers::NONE),
            };
            self.send(event);
        }
        self
    }

    /// Opens the command palette, types `name`, and accepts the first match —
    /// which is how a command with no keybinding is actually run.
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

    /// Time passing on the editor's own clock, with nobody at the keyboard.
    ///
    /// This is the idle path, and it advances only the clock the editor is
    /// handed — it does not sleep. That makes it exactly right for
    /// `files.autoSave: "afterDelay"`, where a scenario wants to say "a minute
    /// went by" without taking a minute.
    ///
    /// It is exactly wrong for waiting on a language server, which is a separate
    /// process that needs real time to answer: a loop of `wait` runs in
    /// microseconds and collects nothing, which looks identical to a broken
    /// feature. [`Editor::settle_until`] is the one to reach for there.
    pub fn wait(&mut self, ms: u64) -> &mut Self {
        self.now_ms += ms;
        self.driver
            .idle(&mut self.session, self.now_ms)
            .expect("the idle path should not fail");
        self
    }

    /// Polls until `ready` holds, or fails saying what it was waiting for.
    ///
    /// A language server is a separate process on the other end of a pipe, so its
    /// answer arrives when it arrives — there is no keystroke that makes it have
    /// happened. This is the editor's own idle path, run in a loop: the same poll
    /// that collects diagnostics while a person sits still.
    ///
    /// It sleeps for real, unlike [`Editor::wait`], because real time is what a
    /// subprocess needs. The clock the editor is handed advances only a little,
    /// so that settling for an answer cannot silently trip the auto-save delay.
    ///
    /// The budget is generous and the failure is loud: a scenario that gave up
    /// after two polls would be a scenario that fails on a loaded machine and
    /// passes on a quiet one.
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

    /// Waits for the language server to have started and said hello.
    ///
    /// Every scenario about a server needs this first: until the handshake is
    /// done there are no capabilities, and until there are capabilities `f12` is
    /// not bound to anything.
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
            // A terminal event that carries no key. The editor would ignore it,
            // and so does this.
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

    /// Everything startup and the editor have complained about.
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

    /// The driver, for the assertions that are about what the frontend is
    /// holding rather than about the session — a hover that has arrived, a
    /// completion list that is open, a language server that is ready.
    pub fn driver(&self) -> &Driver {
        &self.driver
    }

    /// The session itself, for the assertions a screen cannot make.
    pub fn session(&self) -> &Session {
        &self.session
    }

    // ---- looking at the disk ----------------------------------------------

    /// A file in the workspace, as it is on disk right now.
    ///
    /// This is the assertion that matters after a save: the editor's own idea of
    /// the document proves nothing about what a `cat` would show.
    pub fn on_disk(&self, relative: &str) -> String {
        let path = self.workspace.join(relative);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
    }

    /// A file in the workspace as raw bytes, for the endings and encodings a
    /// `String` comparison would paper over.
    pub fn on_disk_bytes(&self, relative: &str) -> Vec<u8> {
        let path = self.workspace.join(relative);
        std::fs::read(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
    }

    /// Whether a file exists in the workspace.
    pub fn exists(&self, relative: &str) -> bool {
        self.workspace.join(relative).exists()
    }

    /// Changes a file behind the editor's back, the way another program would.
    pub fn change_on_disk(&self, relative: &str, contents: &str) {
        let path = self.workspace.join(relative);
        std::fs::write(&path, contents).unwrap_or_else(|error| {
            panic!("writing {}: {error}", path.display());
        });
    }

    /// The workspace directory, for the paths a scenario has to spell in full.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

/// The terminal event that produces `chord`.
///
/// The inverse of `deco_tui::keys::chord_from_event`, and only that: a scenario
/// spells a keystroke the way `keybindings.json` does, and this turns it into what
/// a terminal would have sent so that the real translation runs on the way in.
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
        Key::Named(NamedKey::Space) => KeyCode::Char(' '),
        other => panic!("no terminal event stands for {other:?}"),
    };
    KeyEvent::new(code, modifiers)
}
