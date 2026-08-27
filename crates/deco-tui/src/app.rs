//! The terminal event loop.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use crossterm::event::{self, Event};
use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::{cursor, execute, queue, terminal};
use deco_editor::{Outcome, Session};
use deco_keymap::keys::Chord;
use deco_theme::Rgba;

use crate::keys::chord_from_event;
use crate::lsp::Lsp;
use crate::render::{self, Frame};

/// Restores the terminal when it goes out of scope.
///
/// A panic inside the event loop must not leave the user in a raw-mode
/// alternate screen with no echo, so teardown is tied to the stack rather than
/// to reaching the end of `run`.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("could not put the terminal into raw mode")?;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Nothing useful can be done about a failure here, and returning early
        // would skip the rest of the restoration.
        //
        // The cursor shape is restored along with the screen: `editor.cursorStyle`
        // is deco's business while deco is running, and leaving somebody's shell
        // with an editor's caret shape is not.
        let _ = execute!(
            io::stdout(),
            cursor::SetCursorStyle::DefaultUserShape,
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// The caret shape `editor.cursorStyle` asks for, or `None` to leave the
/// terminal's own alone.
///
/// `None` when the setting was never written down. A terminal's caret is already
/// configured — a block, in most — and replacing it with VS Code's default on behalf
/// of somebody who never mentioned it would be deco overruling a preference it was
/// not asked about. Setting the key, even to its default value, is asking.
///
/// Two shapes have no terminal equivalent and collapse: DECSCUSR has a bar, a block
/// and an underline, and no thin or hollow variant of any of them. So `line-thin`
/// draws as `line` and `block-outline` as `block`, which is closer than refusing.
fn wanted_cursor_style(session: &Session) -> Option<deco_config::CursorStyle> {
    let language = session.document.language();
    let scope = session.settings.source_of("editor.cursorStyle", language)?;
    if scope == deco_config::Scope::Default {
        return None;
    }
    Some(session.document.settings.cursor_style)
}

/// The DECSCUSR shape for a style.
fn to_decscusr(style: deco_config::CursorStyle) -> cursor::SetCursorStyle {
    use deco_config::CursorStyle;
    // Blinking, which is VS Code's `editor.cursorBlinking` default. deco does not
    // resolve that setting, so there is one answer rather than a choice, and this is
    // the one that matches the editor being imitated.
    match style {
        CursorStyle::Line | CursorStyle::LineThin => cursor::SetCursorStyle::BlinkingBar,
        CursorStyle::Block | CursorStyle::BlockOutline => cursor::SetCursorStyle::BlinkingBlock,
        CursorStyle::Underline | CursorStyle::UnderlineThin => {
            cursor::SetCursorStyle::BlinkingUnderScore
        }
    }
}

fn to_crossterm(color: Rgba) -> Color {
    Color::Rgb {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}

/// Writes a frame to the terminal.
fn paint(out: &mut impl Write, frame: &Frame, style: Option<cursor::SetCursorStyle>) -> Result<()> {
    queue!(out, cursor::Hide, cursor::MoveTo(0, 0))?;
    for (row_index, row) in frame.rows.iter().enumerate() {
        queue!(out, cursor::MoveTo(0, row_index as u16))?;
        for span in &row.spans {
            queue!(
                out,
                SetForegroundColor(to_crossterm(span.fg)),
                SetBackgroundColor(to_crossterm(span.bg))
            )?;
            // Nothing reaches the terminal without being made printable first. A
            // document's own text is already substituted by the renderer; this catches
            // the rest — a file name with an escape byte in it, a search result
            // carrying a line of somebody else's file — because a terminal interprets
            // what it is written, and `\x1b]52;c;…` writes the clipboard.
            out.write_all(render::sanitise(&span.text).as_bytes())?;
        }
        queue!(out, ResetColor)?;
    }
    if let Some((x, y)) = frame.cursor {
        if let Some(style) = style {
            queue!(out, style)?;
        }
        queue!(out, cursor::MoveTo(x, y), cursor::Show)?;
    }
    out.flush()?;
    Ok(())
}

/// Whether `files.autoSave: "afterDelay"` is due.
///
/// A pure function of the three facts that decide it, so the rule is testable without
/// a terminal and without waiting a second for one.
///
/// The clock is this side's: `deco-editor` is handed `now_ms` per keystroke and owns
/// no timer, which is what keeps every command deterministic under test. An idle timer
/// therefore lives in the event loop, where the idle already happens — the poll that
/// lets a language server's diagnostics arrive is the same poll that notices the delay
/// has passed.
fn auto_save_due(settings: &deco_config::EditorSettings, idle_ms: u64, dirty: bool) -> bool {
    dirty
        && settings.auto_save == deco_config::AutoSave::AfterDelay
        && idle_ms >= settings.auto_save_delay
}

/// Runs the editor until the user quits.
pub fn run(session: &mut Session, path: Option<PathBuf>) -> Result<()> {
    run_with(session, path, None)
}

/// The remote's matches as palette entries, minus what `files.exclude` hides.
///
/// The server applies its own skip list — `.git`, `node_modules`, `target` — and
/// knows nothing of this user's settings, so the rest of the filtering happens
/// here. Which means a `files.exclude` pattern can make a search report fewer
/// than the server counted; the count shown is the one after filtering, because
/// that is the one on screen.
fn remote_matches(
    found: &deco_remote::Search,
    settings: &deco_config::Settings,
) -> Vec<deco_editor::commands::PaletteEntry> {
    found
        .matches
        .iter()
        .filter(|entry| !crate::files::excluded_by_settings(settings, &entry.path))
        .map(|entry| {
            deco_editor::commands::PaletteEntry::at(
                // The id is what opening one asks the server for, and in a
                // remote session that is the path relative to its workspace.
                &entry.path,
                &format!("{}:{}: {}", entry.path, entry.line + 1, entry.text),
                deco_core::position::Position::new(entry.line, entry.character),
            )
        })
        .collect()
}

/// The distinct files a set of matches named, in the order they first appeared.
///
/// A search reports one entry per *match*, and a file with twenty of them is
/// still one file to open and one transaction to build. Deduplicated in place
/// rather than through a set, so the order the search found them in survives —
/// which is the order the files are opened in, and so the order of the tabs.
fn matched_paths(matches: impl Iterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for path in matches {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// A session whose files live on another machine.
///
/// Carries what the editor needs beyond the connection itself: language servers
/// have to be started over there too, and that needs the transport as well as
/// the directory the far end is serving.
pub struct RemoteSession {
    /// The connection files are read and written through.
    pub client: deco_remote::Client,
    /// Where language servers run, which is the same machine.
    pub location: crate::lsp::Location,
}

/// The editor, optionally against a remote workspace.
///
/// `remote` present means every file the session reads and writes lives on the
/// other end of it. That is a mode rather than a per-document property, because
/// deco does not open local and remote files in one window: the workspace is one
/// place, and half of one would make every path ambiguous.
///
/// Nothing is left doing the wrong thing quietly any more: language servers run
/// on the machine holding the files and project search happens there too, which
/// in both cases is the only place that could work.
pub fn run_with(
    session: &mut Session,
    path: Option<PathBuf>,
    remote: Option<RemoteSession>,
) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let started = Instant::now();
    let mut out = io::stdout();

    let size = terminal::size().unwrap_or((80, 24));
    let mut driver = Driver::start(
        session,
        Options {
            started_with: path,
            remote,
            size,
            ..Options::default()
        },
    );

    loop {
        if driver.needs_redraw() {
            let frame = driver.frame(session);
            paint(
                &mut out,
                &frame,
                wanted_cursor_style(session).map(to_decscusr),
            )?;
        }

        // Waiting with a timeout rather than blocking on `event::read`, so a
        // language server's diagnostics arrive while the user is idle instead of
        // on their next keystroke. The interval is a compromise: short enough
        // that results feel immediate, long enough that an idle editor is not
        // spinning.
        if !event::poll(LSP_POLL_INTERVAL)? {
            driver.idle(session, elapsed_ms(started))?;
            continue;
        }
        driver.poll(session, elapsed_ms(started));

        match event::read()? {
            Event::Key(key) => {
                let Some(chord) = chord_from_event(key) else {
                    continue;
                };
                if driver.key(session, chord, elapsed_ms(started))? == Flow::Quit {
                    break;
                }
            }
            Event::Resize(width, height) => driver.resize(session, width, height),
            _ => {}
        }
    }

    // Before the terminal guard restores the screen, so a server that takes a
    // moment to stop does so while the editor still looks alive.
    driver.shutdown();
    Ok(())
}

/// Milliseconds since the editor started, which is the monotonic clock the core
/// uses for undo grouping and the auto-save delay.
fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// Whether the loop should keep going after a keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Carry on.
    Continue,
    /// The user asked to quit.
    Quit,
}

/// Everything a [`Driver`] needs that it cannot work out from the session.
///
/// The three that are read from the process by default — the home directory, the
/// extension directories and the terminal size — are fields rather than calls
/// buried in the loop, so a test can put a driver in a temporary home instead of
/// the one the test runner happens to have.
pub struct Options {
    /// The file deco was started with. Relative paths and the language server's
    /// workspace root are resolved against its directory.
    pub started_with: Option<PathBuf>,
    /// Present when every file this session reads and writes is on another
    /// machine — and, with it, where language servers are started.
    pub remote: Option<RemoteSession>,
    /// Every directory that may hold installed extensions.
    pub extension_roots: Vec<PathBuf>,
    /// Where extension permission decisions are remembered between sessions.
    ///
    /// `None` remembers nothing past this session, which is what a scenario wants
    /// and what deco does when it cannot work out where its configuration lives.
    pub permissions_file: Option<PathBuf>,
    /// What a leading `~` in a typed path expands to.
    pub home: Option<PathBuf>,
    /// What a relative typed path is taken against when the session has no file
    /// to name a workspace — `deco` on its own, and then a save-as.
    pub cwd: Option<PathBuf>,
    /// The terminal size, in cells.
    pub size: (u16, u16),
}

impl Default for Options {
    fn default() -> Self {
        Self {
            started_with: None,
            remote: None,
            extension_roots: extension_roots(),
            permissions_file: deco_config::paths::ConfigPaths::deco(
                &deco_config::paths::Env::from_process(),
                deco_config::paths::Layout::host(),
            )
            .map(|paths| paths.permissions),
            home: deco_config::paths::Env::from_process().home,
            cwd: std::env::current_dir().ok(),
            size: (80, 24),
        }
    }
}

/// The event loop, with the terminal taken out of it.
///
/// [`run_with`] is this plus a source of events and a painter: it reads keys from
/// crossterm and writes frames to stdout, and everything in between — what a chord
/// does, which of its outcomes need a filesystem, when an idle editor saves — is
/// here. The split is what lets the loop be driven by a test with no terminal
/// attached, against a real workspace on disk, rather than being reachable only by
/// a person holding a keyboard.
pub struct Driver {
    lsp: Lsp,
    hosts: crate::extensions::Hosts,
    remote: Option<deco_remote::Client>,
    started_with: Option<PathBuf>,
    extension_roots: Vec<PathBuf>,
    home: Option<PathBuf>,
    cwd: Option<PathBuf>,
    width: u16,
    height: u16,
    dirty: bool,
    /// When the document last changed, for `files.autoSave: "afterDelay"`. `None`
    /// while there is nothing to save.
    edited_at: Option<u64>,
    /// Where the file tree is rooted, if there is a workspace.
    ///
    /// Kept rather than recomputed per keystroke so the tree cannot end up
    /// reading one root while it was built against another.
    tree_root: Option<PathBuf>,
    /// What `git` says about the workspace, collected off the loop.
    scm: crate::scm::Scm,
}

impl Driver {
    /// Starts a language server, walks the extension directories and sizes the
    /// session — everything the loop does once, before its first keystroke.
    pub fn start(session: &mut Session, options: Options) -> Self {
        let Options {
            started_with,
            remote,
            extension_roots,
            permissions_file,
            home,
            cwd,
            size: (width, height),
        } = options;

        resize(session, width, height);

        let location = match &remote {
            Some(remote) => remote.location.clone(),
            None => crate::lsp::Location::Here,
        };
        let workspace_roots = match &location {
            crate::lsp::Location::Remote { workspace, .. } => Some(workspace.clone()),
            crate::lsp::Location::Here => workspace_root(started_with.as_deref()),
        };
        let remote = remote.map(|remote| remote.client);
        let remote_is_local = remote.is_none();
        let mut lsp =
            Lsp::with_location(session, workspace_root(started_with.as_deref()), location);
        // Started the same way in both cases. A remote session runs its servers on
        // the machine holding the files, which is the only place one could read
        // them.
        lsp.attach(session);
        session.frontend_commands = frontend_commands();

        // Where the file tree is rooted. The same answer the language server and
        // quick open get, so `ctrl+p` and the tree are looking at one workspace.
        // On a remote session that is the workspace the server was given, which
        // is a path on the far machine.
        let tree_root = workspace_roots.clone();
        if let Some(root) = tree_root.clone() {
            session.set_workspace_root(root);
        }

        // What is installed, listed in the palette whether or not it has started:
        // invoking one of these is what starts it. The walk happens here for the same
        // reason the theme walk does — the core has no filesystem.
        let catalogue = crate::extensions::discover(&extension_roots);
        session.problems.extend(catalogue.problems.iter().cloned());
        session
            .frontend_commands
            .extend(crate::extensions::rows(&catalogue));

        Self {
            lsp,
            hosts: {
                let hosts = crate::extensions::Hosts::rooted(
                    catalogue,
                    // The workspace as the machine holding it spells it, which in a
                    // remote session is a directory on the far end. An extension
                    // granted `readFile: workspace` gets exactly the directory the
                    // session is editing, and nothing when there is no workspace at
                    // all.
                    workspace_roots.into_iter().collect(),
                );
                match permissions_file {
                    Some(path) => hosts.remembering(path),
                    None => hosts,
                }
            },
            remote,
            started_with,
            extension_roots,
            home,
            cwd,
            width,
            height,
            dirty: true,
            edited_at: None,
            scm: crate::scm::Scm::new(
                &session.settings,
                // Only for a workspace on this machine. In a remote session
                // `tree_root` is a path on the far end, and running git here
                // against it would either fail or — worse, if a directory of
                // that name happens to exist locally — report a different
                // repository's branch as though it were the one being edited.
                // Git over a connection is its own piece of work.
                tree_root.clone().filter(|_| remote_is_local),
            ),
            tree_root,
        }
    }

    /// Whether anything has changed since the last frame was taken.
    pub fn needs_redraw(&self) -> bool {
        self.dirty
    }

    /// The frame to paint, and the acknowledgement that it has been.
    pub fn frame(&mut self, session: &mut Session) -> Frame {
        // The find bar costs a row, so the text area's height depends on
        // whether it is open — which the last keypress may have changed.
        resize(session, self.width, self.height);
        self.dirty = false;
        // Both overlays, not just the hover. The completion list was built,
        // filtered, navigable with the arrow keys and acceptable with `tab` —
        // and never drawn, because this call asked for a frame with a hover in
        // it and no way to mention the list. `render_with_overlays` has existed
        // the whole time, along with the rule for what happens when both are
        // present: the list wins, since it is the one being interacted with.
        render::render_with_overlays(
            session,
            self.width as usize,
            self.height as usize,
            self.lsp.hover(),
            self.lsp.suggest(),
        )
    }

    /// The terminal changed size.
    pub fn resize(&mut self, session: &mut Session, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        resize(session, width, height);
        self.dirty = true;
    }

    /// Collects whatever the language server and the extension hosts have said.
    pub fn poll(&mut self, session: &mut Session, now_ms: u64) {
        // Destructured so the language server and the hosts can both be
        // advanced while the connection is borrowed: their file requests are
        // served through the same connection the editor reads and writes with,
        // because in a remote session that is where the files are.
        let Self {
            lsp,
            hosts,
            remote,
            dirty,
            ..
        } = self;
        let mut files = match remote.as_mut() {
            Some(client) => crate::extensions::Files::Remote(client),
            None => crate::extensions::Files::Here,
        };
        *dirty |= lsp.poll(session, &mut files);
        hosts.poll(session, &mut files, now_ms);
        // After the others rather than beside them: a `git status` that landed
        // this turn should be on the same frame as whatever caused it.
        self.dirty |= self.scm.poll(session);
    }

    /// A moment with no keystroke in it: the same poll, plus the auto-save clock.
    ///
    /// Checked on the idle path only. A save while keys are still arriving would be
    /// a write per keystroke, which is the thing the delay exists to avoid.
    pub fn idle(&mut self, session: &mut Session, now_ms: u64) -> Result<()> {
        self.poll(session, now_ms);
        let Some(at) = self.edited_at else {
            return Ok(());
        };
        let idle = now_ms.saturating_sub(at);
        if auto_save_due(&session.document.settings, idle, session.document.dirty) {
            save(session, self.remote.as_mut())?;
            self.lsp.saved(session);
            self.edited_at = None;
            self.dirty = true;
        } else if !session.document.dirty {
            // Saved by hand in the meantime.
            self.edited_at = None;
        }
        Ok(())
    }

    /// Stops the language server. The loop is over.
    pub fn shutdown(&mut self) {
        self.lsp.detach();
    }

    /// One keystroke, all the way through: what the core makes of it, and then
    /// whichever of its outcomes needs something the core does not have.
    pub fn key(&mut self, session: &mut Session, chord: Chord, now_ms: u64) -> Result<Flow> {
        let Self {
            lsp,
            hosts,
            remote,
            started_with: path,
            extension_roots,
            home,
            cwd,
            dirty,
            edited_at,
            tree_root,
            ..
        } = self;
        *dirty = true;
        // Remembered before the chord runs: a printable key both inserts
        // itself and narrows an open list, and afterwards there is no way
        // to tell which key it was.
        let typed = printable(&chord);
        // Remembered so a tab switch can be seen afterwards: the chord
        // may put a different document on screen, and the language
        // server has to be told which file it is now looking at.
        let path_before = session.document.path.clone();
        let was_dirty = session.document.dirty;
        // And the language, which `ctrl+k m` can change without the
        // document changing — a different language is a different server.
        let language_before = session.document.language().map(str::to_owned);
        let was_backspace = chord.key
            == deco_keymap::keys::Key::Named(deco_keymap::keys::NamedKey::Backspace)
            && !chord.modifiers.ctrl
            && !chord.modifiers.alt
            && !chord.modifiers.meta;

        match session.handle_chord(chord, now_ms) {
            Outcome::Quit => return Ok(Flow::Quit),
            Outcome::Save => {
                save(session, remote.as_mut())?;
                lsp.saved(session);
            }
            // The picker named a theme; reading it is this side's job.
            Outcome::LoadTheme { label, path } => match load_theme(&label, path.as_deref()) {
                Ok(theme) => {
                    if let Outcome::Message(report) = session.set_theme(theme) {
                        session.status = Some(report);
                    }
                }
                Err(error) => {
                    session.status = Some(error.clone());
                    session.problems.push(error);
                }
            },
            // The prompt named a path; writing it is this side's job, and
            // so is working out what it meant.
            Outcome::SaveAs(target) => {
                // In a remote session the typed name belongs to the far end, so
                // it is left exactly as it was typed. `resolve_path` is about
                // *this* machine — `~` expansion, this process's working
                // directory — and a name resolved here would be one the server
                // has never heard of. The rest of the session's paths are the
                // far end's, relative to the workspace it serves, and a session
                // with half its paths in each namespace can no longer save
                // anything: the server refuses every path outside what it
                // serves, which is what a locally resolved absolute path is.
                //
                // Which also answers "can save-as copy this onto my laptop":
                // no. The workspace is one place, as `run_with` says, and half
                // of one would make every path ambiguous.
                let target = match remote {
                    Some(_) => target,
                    None => resolve_path(&target, path.as_deref(), home.as_deref(), cwd.as_deref()),
                };
                let written = match remote.as_mut() {
                    Some(client) => client
                        .write(&target.display().to_string(), &session.save_contents())
                        .map_err(|error| format!("{}: {error}", target.display())),
                    None => write_file(&target, &session.save_contents()),
                };
                match written {
                    Ok(()) => {
                        if let Outcome::Message(report) = session.rename_to(target) {
                            session.status = Some(report);
                        }
                        // A different path is a different document to a
                        // server, and possibly a different language.
                        lsp.attach(session);
                        lsp.saved(session);
                    }
                    Err(error) => {
                        session.status = Some(error.clone());
                        session.problems.push(error);
                    }
                }
            }
            // The core asked for the file as it is on disk; reading it is
            // this side's job.
            Outcome::Revert => {
                let target = session.document.path.clone();
                // Read wherever the file actually is. In a remote session the
                // document's path is the far end's, relative to the workspace it
                // serves, so reading it here resolves it against this process's
                // working directory instead — which either fails for a file that
                // exists perfectly well over there, or finds an unrelated local
                // file. Revert is the one command whose whole job is to discard
                // unsaved work, which makes reading the wrong file the worst
                // thing it could do.
                let read = target.as_deref().map(|target| match remote.as_mut() {
                    Some(client) => client
                        .read(&target.display().to_string())
                        .map_err(|error| std::io::Error::other(error.to_string())),
                    None => std::fs::read_to_string(target),
                });
                match read {
                    Some(Ok(text)) => {
                        if let Outcome::Message(report) = session.revert_to(&text) {
                            session.status = Some(report);
                        }
                        lsp.changed(session);
                    }
                    Some(Err(error)) => {
                        // The edits stay: throwing them away because the
                        // file could not be read would lose work to a
                        // failure that had nothing to do with it.
                        let path = target.unwrap_or_default();
                        session.status =
                            Some(format!("could not read {}: {error}", path.display()));
                    }
                    None => {}
                }
            }
            // The prompt asked what to look for; walking the workspace is
            // this side's job.
            // Searched on the far end, because that is where the files are.
            // This used to be refused: a local walk in a remote session
            // searches the wrong machine and reports matches in files the
            // editor is not showing.
            // Chosen out of the list below: the decision is taken back, and the
            // extension asks again the next time it wants that.
            Outcome::ForgetExtensionPermission(chosen) => {
                hosts.forget_permission(session, &chosen);
            }
            // The user answered an extension's permission request. The request
            // itself has been waiting in `hosts` since it was asked about; this is
            // what finally sends it a reply.
            Outcome::ExtensionConsent { allow } => {
                let mut files = match remote.as_mut() {
                    Some(client) => crate::extensions::Files::Remote(client),
                    None => crate::extensions::Files::Here,
                };
                hosts.answer_consent(session, allow, &mut files, now_ms);
            }
            // The name has been typed; asking what it would change is the
            // language server's business, and applying the answer needs a
            // filesystem — so both halves are here rather than in the core.
            Outcome::Rename { new_name } => lsp.request_rename(session, &new_name),
            // Which action was chosen; the list it indexes is the frontend's.
            Outcome::CodeAction(id) => {
                // Through the connection when there is one: the action's edit
                // names files on the machine the server is running on.
                let mut files = match remote.as_mut() {
                    Some(client) => crate::extensions::Files::Remote(client),
                    None => crate::extensions::Files::Here,
                };
                lsp.run_code_action(session, &id, &mut files);
            }
            Outcome::SearchInFiles { query, options } if remote.is_some() => {
                let client = remote.as_mut().expect("a remote session");
                match client.search(&query, options) {
                    Ok(found) => {
                        let matches = remote_matches(&found, &session.settings);
                        let (truncated, count) = (found.truncated, matches.len());
                        session.offer_search_results(&query, matches);
                        if truncated {
                            session.status = Some(format!(
                                "{count} matches for `{query}`, and there may be more"
                            ));
                        }
                    }
                    // A failed search leaves the session alone: nothing was
                    // opened and nothing changed, so there is nothing to undo.
                    Err(error) => {
                        session.status = Some(format!("could not search the remote: {error}"))
                    }
                }
            }
            Outcome::SearchInFiles { query, options } => {
                let root = workspace_root(path.as_deref()).unwrap_or_else(|| PathBuf::from("."));
                let found = crate::files::search(&root, &session.settings, &query, options);
                let (truncated, count) = (found.truncated, found.matches.len());
                session.offer_search_results(&query, found.matches);
                if truncated {
                    session.status = Some(format!(
                        "{count} matches for `{query}`, and there may be more"
                    ));
                }
            }
            // The search says *which files*; the session decides what the edit
            // is and makes it one undoable action. Both halves are needed here
            // because only this side knows where the files are.
            Outcome::ReplaceInFiles {
                query,
                replacement,
                options,
            } => {
                let root = workspace_root(path.as_deref()).unwrap_or_else(|| PathBuf::from("."));
                let searched = match remote.as_mut() {
                    Some(client) => client
                        .search(&query, options)
                        .map(|found| {
                            let paths = matched_paths(
                                found
                                    .matches
                                    .iter()
                                    .filter(|entry| {
                                        !crate::files::excluded_by_settings(
                                            &session.settings,
                                            &entry.path,
                                        )
                                    })
                                    .map(|entry| PathBuf::from(&entry.path)),
                            );
                            (paths, found.truncated)
                        })
                        .map_err(|error| format!("could not search the remote: {error}")),
                    None => {
                        let found = crate::files::search(&root, &session.settings, &query, options);
                        let paths = matched_paths(
                            found.matches.iter().map(|entry| PathBuf::from(&entry.id)),
                        );
                        Ok((paths, found.truncated))
                    }
                };

                match searched {
                    // A failed search leaves the session alone: nothing was
                    // opened and nothing changed, so there is nothing to undo.
                    Err(message) => session.status = Some(message),
                    Ok((paths, _)) if paths.is_empty() => {
                        session.status = Some(format!("no matches for `{query}`"));
                    }
                    Ok((paths, truncated)) => {
                        let mut files = match remote.as_mut() {
                            Some(client) => crate::extensions::Files::Remote(client),
                            None => crate::extensions::Files::Here,
                        };
                        let planned = session.plan_replacements(
                            &paths,
                            &query,
                            &replacement,
                            options,
                            |path| files.read(&path.display().to_string()),
                        );
                        match planned.and_then(|plan| session.apply_workspace_edit(plan, now_ms)) {
                            Ok(applied) => {
                                let mut report = applied.summary(&format!("Replaced `{query}`"));
                                // The search stopped early, so there may be
                                // occurrences it never saw. Said plainly: a
                                // replace-all that was not all is the one thing
                                // a user must not have to guess at.
                                if truncated {
                                    report.push_str(
                                        " — the search hit its limit, so there may be more",
                                    );
                                }
                                session.status = Some(report);
                            }
                            // Nothing was changed: the plan is built before
                            // anything is applied, which is what building one is
                            // for.
                            Err(error) => session.status = Some(error.to_string()),
                        }
                    }
                }
            }
            Outcome::SaveAll => {
                // The loop and the reporting are the core's; only the
                // write is this side's, because only this side has a
                // filesystem.
                let outcome = session.save_all(write_file);
                if let Outcome::Message(report) = outcome {
                    session.status = Some(report);
                }
                lsp.saved(session);
            }
            // The tree decided what should happen to a file; doing it needs a
            // filesystem, which is this side.
            Outcome::FileOperation(operation) => {
                let root = session
                    .explorer()
                    .map(|explorer| explorer.root().to_path_buf());
                match perform(&operation, remote.as_mut(), root.as_deref()) {
                    Ok(()) => {
                        // What the file looks like now, so undoing this rename
                        // can tell it apart from whatever might replace it.
                        // Before `file_operation_done`, which is what puts the
                        // undo entry in reach.
                        // A rename's destination, or what a create just made:
                        // both undo by acting on a path, and both need to know
                        // the thing at that path is still theirs.
                        let made = match &operation {
                            deco_editor::FileOperation::Rename { to, .. } => Some(to),
                            deco_editor::FileOperation::CreateFile(path)
                            | deco_editor::FileOperation::CreateFolder(path) => Some(path),
                            _ => None,
                        };
                        if let Some(stamp) = made.and_then(|path| stamp_of(path)) {
                            session.stamp_last_undo(stamp);
                        }
                        session.file_operation_done(&operation);
                        // A delete may have detached open documents; the server
                        // still has them open under URIs that name nothing.
                        lsp.close_deleted(session);
                        // A new file is opened, because creating one you then
                        // have to go and find is two steps where VS Code has
                        // one. Nothing is read: it is empty, and reading it back
                        // to prove that would be a round trip for no answer.
                        if let deco_editor::FileOperation::CreateFile(path) = &operation {
                            session.open(path.clone(), "");
                        }
                    }
                    Err(error) => {
                        session.file_operation_failed(&operation, &error.to_string());
                        // Only for a delete this machine actually attempted. A
                        // remote session refuses every mutation before touching
                        // either filesystem, and the paths are the far end's —
                        // testing them with a local `exists` says "gone" about
                        // every one of them and would let go of every tab in the
                        // workspace for a delete that never happened.
                        if remote.is_none() {
                            reconcile_failure(session, lsp, &operation);
                        }
                    }
                }
            }
            // Quick open and search named a file; reading it is this
            // side's job.
            Outcome::OpenFile { path: target, at } => {
                // Resolved because the path may have been typed: `ctrl+o`
                // accepts `~/notes.txt` and `src/main.rs`. Quick open and
                // search hand over absolute paths, for which this is
                // identity.
                // In remote mode the path is the server's, relative to
                // the workspace it serves, and resolving it against a
                // local directory would produce a path on the wrong
                // machine.
                let target = match remote {
                    Some(_) => target,
                    None => resolve_path(&target, path.as_deref(), home.as_deref(), cwd.as_deref()),
                };
                let read = match remote.as_mut() {
                    Some(client) => client
                        .read(&target.display().to_string())
                        .map_err(|error| std::io::Error::other(error.to_string())),
                    None => std::fs::read_to_string(&target),
                };
                match read {
                    Ok(text) => {
                        session.open(target, &text);
                        if let Some(at) = at {
                            // Clamped, because the file on disk may have
                            // moved on since it was searched.
                            let at = session.document.buffer.clamp_position(at);
                            session.view.selections = deco_core::SelectionSet::caret(at);
                            session.view.reveal_cursor(
                                &session.document.buffer,
                                &session.document.settings,
                            );
                        }
                    }
                    Err(error) => {
                        session.status =
                            Some(format!("could not open {}: {error}", target.display()));
                    }
                }
            }
            // Commands the core cannot implement because they need a
            // language server. Named rather than guessed at, so a
            // mistyped binding still reports as unknown.
            Outcome::Frontend(command) => match command.as_str() {
                "editor.action.showHover" => lsp.request_hover(session),
                "editor.action.revealDefinition" => lsp.request_definition(session),
                "editor.action.goToReferences" => lsp.request_references(session),
                "editor.action.rename" => lsp.offer_rename(session),
                "editor.action.quickFix" => lsp.request_code_actions(session),
                "workbench.action.gotoSymbol" => lsp.request_document_symbols(session),
                // The extension directories have to be walked from here,
                // for the same reason the file list is.
                "workbench.action.selectTheme" => {
                    let available = crate::themes::list(extension_roots);
                    session.offer_themes(crate::themes::rows(&available));
                }
                "closeHoverWidget" => lsp.dismiss_hover(),
                "editor.action.triggerSuggest" => {
                    lsp.request_completion(session, deco_lsp::requests::CompletionTrigger::Invoked)
                }
                // The workspace has to be walked from here: the core has
                // no filesystem.
                // The listing has to come from wherever the files are.
                "workbench.action.quickOpen" if remote.is_some() => {
                    let client = remote.as_mut().expect("just checked");
                    match client.list() {
                        Ok(files) => session.offer_files(
                            files
                                .into_iter()
                                .map(|file| deco_editor::commands::PaletteEntry::new(&file, &file))
                                .collect(),
                        ),
                        Err(error) => {
                            session.status = Some(format!("could not list the remote: {error}"));
                        }
                    }
                }
                "workbench.action.quickOpen" => {
                    let root =
                        workspace_root(path.as_deref()).unwrap_or_else(|| PathBuf::from("."));
                    let listing = crate::files::list(&root, &session.settings);
                    let truncated = listing.truncated;
                    session.offer_files(listing.files);
                    if truncated {
                        session.status = Some(format!(
                            "showing the first {} files",
                            crate::files::MAX_FILES
                        ));
                    }
                }
                "editor.action.formatDocument" => lsp.request_formatting(session, false),
                "editor.action.formatSelection" => lsp.request_formatting(session, true),
                "hideSuggestWidget" => lsp.dismiss_suggest(),
                "selectNextSuggestion" => {
                    lsp.select_next();
                }
                "selectPrevSuggestion" => {
                    lsp.select_previous();
                }
                "acceptSelectedSuggestion" => {
                    lsp.accept(session, now_ms);
                }
                // An extension's command, whose identifier is whatever is
                // installed rather than anything written down here. Asked
                // last so that no core command can be shadowed by one.
                // Offered before the extension commands are looked at, because it
                // is deco's own command rather than one an extension registered.
                "deco.extensions.forgetPermission" => {
                    hosts.offer_permissions(session);
                }
                other if hosts.run_command(session, other) => {}
                other => {
                    session.status = Some(format!("{other} is not implemented yet"));
                }
            },
            _ => {}
        }

        // While the find bar has the keyboard, a keystroke narrows the
        // query and the document never sees it. A completion list left
        // open underneath would be narrowed by text that was never
        // typed into the file — so it goes, along with any hover.
        if session.find.visible() || session.prompt.is_some() {
            lsp.dismiss_suggest();
            lsp.dismiss_hover();
        }
        // A printable key both typed itself and narrowed the list; a
        // backspace both deleted and widened it. Both after the command,
        // so the list and the document agree about what has been typed.
        else if let Some(c) = typed {
            if !lsp.typed(session, c) {
                // No list was open, so this may be a trigger character —
                // `.` or `::` — that should open one.
                if lsp
                    .completion_triggers()
                    .iter()
                    .any(|trigger| trigger.ends_with(c))
                {
                    lsp.request_completion(
                        session,
                        deco_lsp::requests::CompletionTrigger::Character(c.to_string()),
                    );
                }
            }
        } else if was_backspace {
            lsp.backspaced(session);
        }
        // The chord may have switched tabs — ctrl+tab, ctrl+w, a file
        // opened from a jump. `attach` is idempotent, so calling it for
        // the same document costs a comparison; for a new one it sends
        // didClose/didOpen or starts the right server, and the stored
        // diagnostics for the returning document are collected.
        if session.document.path != path_before
            || session.document.language().map(str::to_owned) != language_before
        {
            // A hover or completion list anchored in the old document
            // would describe text that is no longer on screen — or, after
            // a language change, would be the old server's answer about a
            // file it is no longer responsible for.
            lsp.dismiss_hover();
            lsp.dismiss_suggest();
            lsp.attach(session);
            lsp.refresh_diagnostics(session);
        }
        // After the command, not before: the server has to be told
        // about the text as it now is.
        lsp.changed(session);
        // A hover describing where the cursor was is worse than none.
        *dirty |= lsp.cursor_moved(session);

        // The auto-save clock restarts on every edit, so a delay measured
        // from the *first* keystroke of a paragraph cannot fire in the middle
        // of typing it. Cleared when the document is clean again, whether
        // this keystroke saved it or undid its way back.
        if session.document.dirty {
            *edited_at = Some(now_ms);
        } else if was_dirty {
            *edited_at = None;
        }

        // Whatever that keystroke was, it may have opened a directory in the
        // tree — `ctrl+b` showing it for the first time, `right` on a folder, a
        // reveal walking down to a file. Answering here rather than inside each
        // arm keeps the tree's reading in one place, and one place is where it
        // has to be to be turned into something asynchronous later.
        if let Some(root) = tree_root.clone() {
            fill_tree(session, &root, remote.as_mut())?;
        }
        Ok(Flow::Continue)
    }

    /// What the extension hosts have been up to, for a test or a status line.
    pub fn hosts(&self) -> &crate::extensions::Hosts {
        &self.hosts
    }

    /// The language-server client, for the same reason.
    pub fn lsp(&self) -> &Lsp {
        &self.lsp
    }
}

/// The commands this frontend implements, for the command palette.
///
/// Only the ones the core cannot run on its own — every one of these needs the
/// language-server client that lives here. The core cannot know which of the
/// commands it hands onward a frontend has wired up, so each frontend says: a
/// palette offering `Go to References`, which this one answers with "not
/// implemented yet", would be worse than one that leaves it out.
pub fn frontend_commands() -> Vec<deco_editor::commands::PaletteEntry> {
    [
        ("editor.action.showHover", "Show Hover"),
        ("editor.action.revealDefinition", "Go to Definition"),
        ("editor.action.goToReferences", "Go to References"),
        ("editor.action.rename", "Rename Symbol"),
        ("editor.action.quickFix", "Quick Fix"),
        ("workbench.action.gotoSymbol", "Go to Symbol in Editor"),
        ("workbench.action.selectTheme", "Color Theme"),
        ("editor.action.triggerSuggest", "Trigger Suggest"),
        ("editor.action.formatDocument", "Format Document"),
        ("editor.action.formatSelection", "Format Selection"),
        ("workbench.action.quickOpen", "Go to File"),
        ("workbench.action.findInFiles", "Find in Files"),
        ("workbench.action.replaceInFiles", "Replace in Files"),
        (
            "deco.extensions.forgetPermission",
            "Extensions: Forget a Permission Decision",
        ),
    ]
    .iter()
    .map(|(id, title)| deco_editor::commands::PaletteEntry::new(id, title))
    .collect()
}

/// Tells the session how much of the terminal is text.
///
/// The remainder is chrome — the status bar, and the find bar when it is open —
/// so this has to be redone whenever either the terminal or the find bar changes
/// size, or the last line of the file ends up underneath the bar.
fn resize(session: &mut Session, width: u16, height: u16) {
    let height = height as usize;
    let text_height = height.saturating_sub(render::chrome_height(session, height));
    session.resize(width as usize, text_height);
}

/// The character a chord types, if it types one.
///
/// Mirrors the rule in `Session::handle_chord`: an unmodified printable key
/// inserts itself, and anything with Ctrl, Alt or Meta was reaching for a
/// command. Duplicated deliberately rather than exposed from the core — the core
/// decides what a key *does*, and this only needs to know what was typed so the
/// completion list can be narrowed by the same character.
fn printable(chord: &deco_keymap::keys::Chord) -> Option<char> {
    use deco_keymap::keys::Key;
    match chord.key {
        Key::Char(c) if !chord.modifiers.ctrl && !chord.modifiers.alt && !chord.modifiers.meta => {
            if chord.modifiers.shift {
                c.to_uppercase().next()
            } else {
                Some(c)
            }
        }
        _ => None,
    }
}

/// How long to wait on the terminal before checking the language server.
///
/// A language server has nothing to do with the keyboard, so the loop cannot
/// simply block on input. 50ms is below the threshold at which a diagnostic
/// feels delayed, and 20 wakeups a second on an idle editor is not measurable.
const LSP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Carries out one of the tree's file operations.
///
/// The core has already decided it is allowed — the name is a name, the path is
/// inside the workspace, nothing in the listing is in the way. What is left is
/// the part that can still fail for reasons no amount of checking predicts: a
/// permission, a full disk, another program getting there first.
///
/// `create_new` rather than `File::create` for a new file, so that a file which
/// appeared between the check and here is not silently truncated. That race is
/// exactly why the frontend re-checks rather than trusting the tree's listing.
fn perform(
    operation: &deco_editor::FileOperation,
    remote: Option<&mut deco_remote::Client>,
    root: Option<&Path>,
) -> io::Result<()> {
    use deco_editor::FileOperation;

    if remote.is_some() {
        // The protocol reads, writes and lists; it has no create, rename or
        // delete. Refused by name rather than half-done locally, which would
        // change a file on this machine and report success about the other one.
        return Err(io::Error::other(
            "changing files over a remote connection is not implemented yet",
        ));
    }

    // The session checked that the path is inside the workspace, but it can only
    // compare the *spelling*: it has no filesystem. A directory replaced by a
    // symlink since the tree listed it resolves elsewhere, and every call below
    // follows it — so `New File` in a `src` that is now a link to `/outside`
    // would write there, having been told it was writing inside the workspace.
    if let Some(root) = root {
        inside_after_links(root, operation.parent().unwrap_or(Path::new("")))?;
    }

    match operation {
        FileOperation::CreateFile(path) => std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map(drop),
        FileOperation::CreateFolder(path) => std::fs::create_dir(path),
        FileOperation::Rename {
            from,
            to,
            expect,
            directory,
        } => {
            // The kind the tree was showing, not the kind on disk now. A
            // directory replaced by a file since it was read would otherwise be
            // moved as though it were still a directory, and every tab below the
            // old path retargeted onto a regular file — the same reinterpretation
            // a delete is already stopped from making.
            match std::fs::symlink_metadata(from) {
                Ok(meta) if meta.is_dir() == *directory => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "{} is not the {} the tree was showing any more",
                            from.display(),
                            if *directory { "folder" } else { "file" }
                        ),
                    ))
                }
                Err(error) => return Err(error),
            }
            // An undo carries what the file looked like when it was moved. If
            // that no longer matches, something replaced it, and moving *that*
            // back would be shifting somebody else's file on a keystroke meant
            // to undo your own — then pointing the buffer at it, so the next
            // save would write over its contents.
            if let Some(expected) = expect {
                match stamp_of(from) {
                    Some(now) if &now == expected => {}
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!(
                                "{} is not the file that was renamed any more",
                                from.display()
                            ),
                        ))
                    }
                }
            }
            // `rename` would overwrite an existing `to` on Unix. The tree
            // refused that already, but between its listing and this call is a
            // window, and silently replacing somebody's file is the one outcome
            // worth a second check.
            //
            // Not on a case-only rename, though. On the case-insensitive
            // filesystems macOS and Windows usually have, `Foo.rs` *is*
            // `foo.rs`, so this check sees the source itself and would refuse
            // every change of capitalisation. Canonicalising both tells the two
            // cases apart: same file, or a different one in the way.
            //
            // `symlink_metadata` rather than `exists`, which follows links: a
            // dangling symlink at the destination answers `false` to `exists`
            // and would be silently replaced. The tree cannot show one either —
            // `list_dir` reports only real files and directories — so it is
            // invisible from both sides, and needs no race to be hit.
            if std::fs::symlink_metadata(to).is_ok() && !same_file(from, to) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", to.display()),
                ));
            }
            std::fs::rename(from, to)
        }
        // Dispatched on what the tree was showing when this was confirmed, not
        // on what is on disk now. The tree has no watcher, so if a file has been
        // replaced by a directory since, asking the disk would find a directory
        // and delete it recursively — having asked the user about a file.
        // `remove_file` on a directory fails, which is the refusal wanted.
        FileOperation::Delete {
            path,
            directory: true,
        } => std::fs::remove_dir_all(path),
        FileOperation::Delete {
            path,
            directory: false,
        } => std::fs::remove_file(path),
        // Undoing a create. `remove_dir` rather than `remove_dir_all` — it
        // fails on a directory with anything in it, which is exactly the
        // refusal wanted, and gets it from the operating system rather than
        // from a check with a race under it.
        FileOperation::DeleteIfEmpty {
            path,
            directory,
            expect,
        } => {
            // Empty is not an identity. Another program can remove what was
            // created and leave a different empty file or directory in its
            // place, and undoing the create would delete that instead.
            if let Some(expected) = expect {
                match stamp_of(path) {
                    Some(now) if &now == expected => {}
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("{} is not what was created any more", path.display()),
                        ))
                    }
                }
            }
            if *directory {
                std::fs::remove_dir(path).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("{} is no longer empty", path.display()),
                    )
                })
            } else {
                // A file has to be checked, there being no "unlink if empty".
                // The window between this and the unlink is real and the cost of
                // losing it is small: something written in that instant is lost.
                // Refusing outright would be worse — it would mean a create
                // could never be undone on a filesystem anyone else is using.
                match std::fs::metadata(path) {
                    Ok(meta) if meta.len() > 0 => Err(io::Error::other(format!(
                        "{} has been written to since it was created — delete it \
                         yourself if that is what you meant",
                        path.display()
                    ))),
                    Ok(_) => std::fs::remove_file(path),
                    Err(error) => Err(error),
                }
            }
        }
    }
}

/// Puts the tree and the tabs back in step after a mutation reported failure.
///
/// Only for one this machine actually attempted: a remote session refuses every
/// mutation before touching either filesystem, and the paths are the far end's,
/// so a local `exists` would say "gone" about all of them.
fn reconcile_failure(session: &mut Session, lsp: &mut Lsp, operation: &deco_editor::FileOperation) {
    // Whatever failed, the listing that said it would work is now suspect — a
    // create refused because another program claimed the name means the tree is
    // missing that name, and without a re-read every retry fails against the
    // same stale picture. There is no watcher to notice it any other way.
    if let Some(parent) = operation.parent() {
        session.invalidate_directory(parent);
        // But re-reading it is no use if it is not a directory any more.
        // `list_dir` answers an unreadable or non-directory path with an empty
        // listing, so the tree would go on showing it as an empty folder for
        // good — and every create in it would go on failing — while the entry
        // that would correct it sits in the listing *above*. So that one is
        // re-read too, and what the tree remembers below is dropped.
        if !parent.is_dir() {
            if let Some(above) = parent.parent() {
                session.invalidate_directory(above);
            }
            session.forget_subtree(parent);
        }
    }

    // And a failed rename's *source*, subtree and all. The usual reason a
    // rename fails is that something changed underneath it — including the
    // source being removed — and the parent alone leaves that directory's own
    // cached listing describing children it may no longer have. Recreate it
    // under the old name later and they come back for good, because expanding a
    // directory whose listing is already `Known` asks for nothing.
    //
    // Invalidated rather than forgotten: the source usually still exists, the
    // rename having failed for some other reason, so re-reading it is right and
    // throwing away its expansion is not.
    if let deco_editor::FileOperation::Rename { from, .. } = operation {
        session.invalidate_subtree(from);
    }

    reconcile_failed_delete(session, lsp, operation);
}

/// Puts the tree and the tabs back in step after a delete reported failure.
///
/// `remove_dir_all` can take part of a tree and then stop, so "it failed" does
/// not mean "nothing happened". Everything here is driven by what is on disk
/// now rather than by what was asked for.
fn reconcile_failed_delete(
    session: &mut Session,
    lsp: &mut Lsp,
    operation: &deco_editor::FileOperation,
) {
    use deco_editor::FileOperation;
    let (path, recursive) = match operation {
        FileOperation::Delete { path, directory } => (path, *directory),
        FileOperation::DeleteIfEmpty { path, .. } => (path, false),
        _ => return,
    };

    // What the tree knew, taken *before* anything is invalidated: invalidating
    // turns those listings back into "not read yet", and a scan afterwards finds
    // nothing to check. Which is what made the whole tree half of the evidence
    // below dead — only an open tab could ever have raised the barrier.
    let known: Vec<PathBuf> = if recursive {
        session.known_paths_under(path)
    } else {
        Vec::new()
    };

    // And the subtree: the directory itself may survive, so re-reading its
    // parent rediscovers it and leaves its own listing — and every expanded one
    // below — describing files that have gone.
    session.invalidate_subtree(path);

    // Only a recursive delete can half happen: `remove_file` and `remove_dir`
    // either did it or did not. And even a recursive failure need not mean
    // anything went — a `PermissionDenied` on the directory itself stops it
    // before it opens anything. So the barrier goes up on *evidence* rather
    // than on the error kind: something the tree knew about, or something a tab
    // is holding, has actually gone.
    //
    // Bounded by what had been read, which is what is on screen — the tree only
    // knows the directories somebody expanded. A file removed out of a
    // collapsed directory is invisible here, and so is the undo it would have
    // invalidated; naming that is better than a check that walks a workspace to
    // answer a question about a keystroke.
    let mut anything_went = known
        .iter()
        .any(|known| matches!(known.try_exists(), Ok(false)));

    // Per file, because half a directory survives and only the disk knows which
    // half.
    for held in session.open_paths_under(path) {
        // `try_exists` rather than `exists`, which answers `false` for a file it
        // cannot stat. The same permission problem that stopped the delete can
        // hide a file that is still there, and letting go of a tab whose file
        // survived is the wrong way to be wrong. Only a definite `false`
        // detaches; an error leaves the tab attached.
        if matches!(held.try_exists(), Ok(false)) {
            anything_went = true;
            session.detach_tabs_under(&held);
        }
    }
    if recursive && anything_went {
        session.clear_file_undo();
    }
    lsp.close_deleted(session);
}

/// Refuses a directory that resolves outside `root` once symlinks are followed.
///
/// `canonicalize` is what turns the session's spelling-based check into one
/// about where the path actually leads. It closes the case that matters in
/// practice — a link sitting in the workspace, put there by a build or a
/// package manager — but not a racing one: something can still replace an
/// ancestor between this and the call below. Closing *that* needs `openat` with
/// `O_NOFOLLOW` and a directory handle per component, which the standard library
/// has on no platform. `docs/files.md` names the window, next to the rename one
/// it is a cousin of.
fn inside_after_links(root: &Path, dir: &Path) -> io::Result<()> {
    let real_root = std::fs::canonicalize(root)?;
    let real_dir = std::fs::canonicalize(dir)?;
    if real_dir.starts_with(&real_root) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} leads outside the workspace — something replaced a directory \
             with a link to {}",
            dir.display(),
            real_dir.display()
        ),
    ))
}

/// What the file at `path` looks like, or `None` if it cannot be seen.
fn stamp_of(path: &Path) -> Option<deco_editor::files::Stamp> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(deco_editor::files::Stamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// Whether two paths name the same file on disk.
///
/// By canonicalising rather than comparing strings, so that a case-only rename
/// on a case-insensitive filesystem is recognised as the file renaming itself.
/// A path that cannot be canonicalised is not the same file as anything — the
/// caller only asks when `to` exists, so failing here means something changed
/// underneath and refusing is the safe answer.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Answers every directory listing the file tree is waiting on.
///
/// A loop rather than one listing, because the tree asks for one at a time and
/// answering can reveal the next: opening the tree onto `src/deep/main.rs` needs
/// `src` before it can know it wants `src/deep`. Bounded by [`files::MAX_DEPTH`]
/// so a symlink that contains itself cannot spin here — the same guard, and the
/// same reason, as the walk's.
fn fill_tree(
    session: &mut Session,
    root: &Path,
    remote: Option<&mut deco_remote::Client>,
) -> Result<()> {
    let mut remote = remote;
    for _ in 0..crate::files::MAX_DEPTH {
        let Some(dir) = session.directory_wanted() else {
            return Ok(());
        };
        let entries = match remote.as_mut() {
            // The remote lists the whole workspace at once — that is the only
            // listing the protocol has — so a directory's contents are derived
            // from it. Wasteful next to a per-directory call, and no more so
            // than `ctrl+p`, which does the same list on every press. A
            // `list_dir` on the wire is the obvious improvement and is a
            // protocol change rather than a local one.
            Some(client) => match client.list() {
                Ok(files) => remote_children(&files, root, &dir),
                Err(error) => {
                    session.status = Some(format!("could not list the remote: {error}"));
                    // Filled empty rather than left pending, or the tree asks
                    // for the same unreachable directory on every keystroke.
                    Vec::new()
                }
            },
            None => crate::files::list_dir(root, &dir, &session.settings),
        };
        session.fill_directory(&dir, entries);
    }
    Ok(())
}

/// What `dir` directly contains, out of a flat list of every file under `root`.
///
/// Directories are inferred from the paths rather than reported: a flat listing
/// names files, and every path with something after `dir/` implies a directory
/// that the tree has to be able to show and expand.
fn remote_children(files: &[String], root: &Path, dir: &Path) -> Vec<deco_editor::explorer::Entry> {
    let prefix = match dir.strip_prefix(root) {
        Ok(rest) if rest.as_os_str().is_empty() => String::new(),
        Ok(rest) => format!("{}/", rest.to_string_lossy().replace('\\', "/")),
        Err(_) => return Vec::new(),
    };

    let mut names: Vec<deco_editor::explorer::Entry> = Vec::new();
    for file in files {
        let file = file.replace('\\', "/");
        let Some(rest) = file.strip_prefix(&prefix) else {
            continue;
        };
        let (name, is_dir) = match rest.split_once('/') {
            Some((head, _)) => (head, true),
            None if rest.is_empty() => continue,
            None => (rest, false),
        };
        if !names.iter().any(|entry| entry.name == name) {
            names.push(if is_dir {
                deco_editor::explorer::Entry::dir(name)
            } else {
                deco_editor::explorer::Entry::file(name)
            });
        }
    }
    names
}

/// The directory to hand a language server as its workspace root.
///
/// The file's own directory, which is the honest answer while deco has no
/// concept of an open folder: a server given a root it cannot make sense of
/// indexes the wrong tree, and one given none falls back to single-file mode,
/// which is worse than a directory that is merely narrow.
fn workspace_root(path: Option<&Path>) -> Option<PathBuf> {
    let path = path?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    absolute.parent().map(Path::to_path_buf)
}

/// Works out what a typed path meant.
///
/// `~` is expanded, and a relative path is taken against the workspace root —
/// which is the directory the editor was started in or the directory of the file
/// it was started with, the same root quick open walks. Resolving against the
/// process's working directory instead would mean a path that worked when deco was
/// launched from the project and not when it was launched from anywhere else.
///
/// An absolute path is returned unchanged, so callers that already have one — quick
/// open, search results, a go-to-definition jump — can go through here too.
///
/// `home` and `cwd` are passed in rather than read here, so that the rule can be
/// exercised against directories a test controls instead of whichever ones the
/// machine running the tests happens to have.
///
/// The working directory is the fallback for a session that was started with no
/// file — `deco` on its own, then `ctrl+s` and a name — and it is a fallback
/// rather than nothing for a sharp reason. A relative path returned from here
/// reaches [`Session::rename_to`] as the document's path and never compares equal
/// to the absolute one every other way of opening that same file produces, so
/// saving an untitled buffer as `notes.txt` and then choosing `notes.txt` from
/// quick open opened it twice, in two buffers with two undo histories. That is
/// the bug `deco::startup::absolute` exists to prevent for a path on the command
/// line; this is the other door into it.
fn resolve_path(
    typed: &Path,
    started_with: Option<&Path>,
    home: Option<&Path>,
    cwd: Option<&Path>,
) -> PathBuf {
    let text = typed.to_string_lossy();
    if let Some(rest) = text.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            if let Some(home) = home {
                // `trim_start_matches` rather than `join`, because joining an
                // absolute `/notes.txt` onto the home directory discards it.
                return home.join(rest.trim_start_matches(['/', '\\']));
            }
        }
    }
    if typed.is_absolute() {
        return typed.to_path_buf();
    }
    match workspace_root(started_with).or_else(|| cwd.map(Path::to_path_buf)) {
        Some(root) => root.join(typed),
        // Neither a workspace nor a readable working directory. The path is
        // handed onward as it was typed, which is what deco did before it
        // resolved anything.
        None => typed.to_path_buf(),
    }
}

/// Every directory that may hold installed extensions.
///
/// deco's own and VS Code's, so a theme installed for VS Code is offered here
/// without being copied — the same one-way borrowing that applies to settings.
fn extension_roots() -> Vec<PathBuf> {
    let env = deco_config::paths::Env::from_process();
    let layout = deco_config::paths::Layout::host();
    [
        deco_config::paths::ConfigPaths::deco(&env, layout),
        deco_config::paths::ConfigPaths::vscode(&env, layout),
    ]
    .into_iter()
    .flatten()
    .map(|paths| paths.extensions)
    .collect()
}

/// Loads the theme a picker chose, built-in or from a file.
///
/// The label is enough for a built-in; a contributed theme needs its file, whose
/// `include` chain `deco-theme` follows.
fn load_theme(
    label: &str,
    path: Option<&Path>,
) -> std::result::Result<deco_theme::ColorTheme, String> {
    match path {
        Some(path) => deco_theme::ColorTheme::load_from_file(path)
            .map_err(|error| format!("could not load `{label}`: {error}")),
        None => deco_theme::defaults::builtin(label)
            .ok_or_else(|| format!("`{label}` is not a theme deco ships with")),
    }
}

/// Writes `contents` to `path`, describing the failure in a form fit to show.
fn write_file(path: &Path, contents: &str) -> std::result::Result<(), String> {
    std::fs::write(path, contents).map_err(|error| format!("{}: {error}", path.display()))
}

/// Writes the open document to disk.
/// Writes the open document to disk.
///
/// **Only ever to its own path.** This used to fall back to the file deco was
/// started with when the document had none, which was true of a session holding
/// exactly one document and became a silent overwrite the moment tabs arrived: an
/// untitled tab and the started-with file are two different documents. The
/// parameter is gone rather than merely unused, so nothing can aim a write at the
/// wrong file again.
///
/// A document with no path never reaches here — [`Session`] turns `ctrl+s` into the
/// save-as prompt instead — but the arm stays as a guard rather than a `panic!`.
fn save(session: &mut Session, remote: Option<&mut deco_remote::Client>) -> Result<()> {
    let Some(path) = session.document.path.clone() else {
        session.status = Some("This document has no filename yet".to_owned());
        return Ok(());
    };

    // A failed remote write is reported and *not* fatal: the connection can drop
    // while the editor is perfectly able to keep the text and try again. A failed
    // local write is still fatal, as it was, because the alternative is an editor
    // that says "saved" about a disk that refused.
    if let Some(client) = remote {
        let asked = path.display().to_string();
        return match client.write(&asked, &session.save_contents()) {
            Ok(()) => {
                session.mark_saved();
                session.status = Some(format!("Saved {asked} on the remote"));
                Ok(())
            }
            Err(error) => {
                session.status = Some(format!("could not save {asked}: {error}"));
                Ok(())
            }
        };
    }

    std::fs::write(&path, session.save_contents())
        .with_context(|| format!("could not write {}", path.display()))?;
    session.mark_saved();
    session.status = Some(format!("Saved {}", path.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_theme::Rgba;

    /// A scratch directory of this test's own.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deco-perform-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[test]
    fn creating_a_file_that_appeared_since_the_check_does_not_truncate_it() {
        let dir = scratch("create");
        let path = dir.join("taken.rs");
        std::fs::write(&path, "someone else got here first\n").unwrap();

        // The tree's listing said this name was free; the disk disagrees.
        let error = perform(
            &deco_editor::FileOperation::CreateFile(path.clone()),
            None,
            Some(&dir),
        )
        .expect_err("creating over an existing file must fail");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "someone else got here first\n",
            "the file that was already there is untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renaming_onto_an_existing_file_is_refused_rather_than_overwriting() {
        let dir = scratch("rename");
        let from = dir.join("a.rs");
        let to = dir.join("b.rs");
        std::fs::write(&from, "moving\n").unwrap();
        std::fs::write(&to, "in the way\n").unwrap();

        let error = perform(
            &deco_editor::FileOperation::Rename {
                from: from.clone(),
                to: to.clone(),
                expect: None,
                directory: false,
            },
            None,
            Some(&dir),
        )
        .expect_err("a rename must not replace a file");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "in the way\n");
        assert!(from.exists(), "and the source is still there");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_rename_and_delete_do_what_they_say() {
        let dir = scratch("roundtrip");
        let made = dir.join("new.rs");
        perform(
            &deco_editor::FileOperation::CreateFile(made.clone()),
            None,
            Some(&dir),
        )
        .unwrap();
        assert!(made.is_file());
        assert_eq!(std::fs::read_to_string(&made).unwrap(), "");

        let moved = dir.join("moved.rs");
        perform(
            &deco_editor::FileOperation::Rename {
                from: made.clone(),
                to: moved.clone(),
                expect: None,
                directory: false,
            },
            None,
            Some(&dir),
        )
        .unwrap();
        assert!(!made.exists() && moved.is_file());

        perform(
            &deco_editor::FileOperation::Delete {
                path: moved.clone(),
                directory: false,
            },
            None,
            Some(&dir),
        )
        .unwrap();
        assert!(!moved.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undoing_a_create_refuses_once_the_file_has_content() {
        let dir = scratch("undo-create");
        let path = dir.join("new.rs");
        perform(
            &deco_editor::FileOperation::CreateFile(path.clone()),
            None,
            Some(&dir),
        )
        .unwrap();
        // Created, then typed in and saved — which is the whole point of having
        // created it.
        std::fs::write(&path, "fn main() {}\n").unwrap();

        perform(
            &deco_editor::FileOperation::DeleteIfEmpty {
                path: path.clone(),
                directory: false,
                expect: None,
            },
            None,
            Some(&dir),
        )
        .expect_err("undoing the create must not take the contents with it");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {}\n",
            "the work is still there"
        );

        // Still empty, and it goes.
        let untouched = dir.join("untouched.rs");
        perform(
            &deco_editor::FileOperation::CreateFile(untouched.clone()),
            None,
            Some(&dir),
        )
        .unwrap();
        perform(
            &deco_editor::FileOperation::DeleteIfEmpty {
                path: untouched.clone(),
                directory: false,
                expect: None,
            },
            None,
            Some(&dir),
        )
        .unwrap();
        assert!(!untouched.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undoing_a_new_folder_refuses_once_it_holds_anything() {
        let dir = scratch("undo-folder");
        let made = dir.join("pkg");
        perform(
            &deco_editor::FileOperation::CreateFolder(made.clone()),
            None,
            Some(&dir),
        )
        .unwrap();
        std::fs::write(made.join("mod.rs"), "pub fn x() {}\n").unwrap();

        perform(
            &deco_editor::FileOperation::DeleteIfEmpty {
                path: made.clone(),
                directory: true,
                expect: None,
            },
            None,
            Some(&dir),
        )
        .expect_err("undoing the create must not recursively delete a tree");
        assert!(
            made.join("mod.rs").is_file(),
            "the file added since is still there"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_will_not_replace_a_dangling_symlink() {
        let dir = scratch("dangling");
        let from = dir.join("real.rs");
        std::fs::write(&from, "moving\n").unwrap();
        let to = dir.join("link.rs");
        // A symlink to nothing: `exists` answers false for it, and the tree
        // cannot show it either.
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("nowhere.rs"), &to).unwrap();
        #[cfg(not(unix))]
        {
            let _ = &to;
            return;
        }

        #[cfg(unix)]
        {
            assert!(!to.exists(), "the premise: it looks absent");
            perform(
                &deco_editor::FileOperation::Rename {
                    from: from.clone(),
                    to: to.clone(),
                    expect: None,
                    directory: false,
                },
                None,
                Some(&dir),
            )
            .expect_err("a rename must not silently remove a symlink");
            assert!(
                std::fs::symlink_metadata(&to).is_ok(),
                "the link is still there"
            );
            assert!(from.exists(), "and so is the file that was to move");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_symlinked_directory_cannot_be_used_to_write_outside_the_workspace() {
        let dir = scratch("escape");
        let workspace = dir.join("workspace");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        // `src` was a directory when the tree listed it; it is a link now.
        std::os::unix::fs::symlink(&outside, workspace.join("src")).unwrap();

        // The session's own check passes: the spelling is inside the workspace.
        let wanted = workspace.join("src").join("planted.rs");
        assert!(wanted.starts_with(&workspace));

        perform(
            &deco_editor::FileOperation::CreateFile(wanted),
            None,
            Some(&workspace),
        )
        .expect_err("a link out of the workspace must not be written through");
        assert!(
            !outside.join("planted.rs").exists(),
            "and nothing was written where it led"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undoing_a_rename_refuses_once_something_else_holds_the_name() {
        let dir = scratch("undo-rename");
        let from = dir.join("a.rs");
        let to = dir.join("b.rs");
        std::fs::write(&from, "mine\n").unwrap();
        perform(
            &deco_editor::FileOperation::Rename {
                from: from.clone(),
                to: to.clone(),
                expect: None,
                directory: false,
            },
            None,
            Some(&dir),
        )
        .unwrap();
        let stamp = stamp_of(&to).expect("it is there");

        // Another program takes `b.rs` away and leaves something else in its
        // place. Undoing the rename must not move *that*.
        std::fs::remove_file(&to).unwrap();
        std::fs::write(&to, "somebody else's, and longer\n").unwrap();

        perform(
            &deco_editor::FileOperation::Rename {
                from: to.clone(),
                to: from.clone(),
                expect: Some(stamp),
                directory: false,
            },
            None,
            Some(&dir),
        )
        .expect_err("the file that was renamed is not there any more");
        assert_eq!(
            std::fs::read_to_string(&to).unwrap(),
            "somebody else's, and longer\n",
            "and it was left where it was"
        );
        assert!(!from.exists(), "nor was anything put back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undoing_a_rename_works_when_the_file_is_still_the_one_that_moved() {
        let dir = scratch("undo-rename-ok");
        let from = dir.join("a.rs");
        let to = dir.join("b.rs");
        std::fs::write(&from, "mine\n").unwrap();
        perform(
            &deco_editor::FileOperation::Rename {
                from: from.clone(),
                to: to.clone(),
                expect: None,
                directory: false,
            },
            None,
            Some(&dir),
        )
        .unwrap();

        perform(
            &deco_editor::FileOperation::Rename {
                from: to.clone(),
                to: from.clone(),
                expect: stamp_of(&to),
                directory: false,
            },
            None,
            Some(&dir),
        )
        .expect("nothing touched it, so the undo goes through");
        assert_eq!(std::fs::read_to_string(&from).unwrap(), "mine\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undoing_a_create_refuses_once_something_else_holds_the_name() {
        let dir = scratch("undo-create-swapped");
        let made = dir.join("new.rs");
        perform(
            &deco_editor::FileOperation::CreateFile(made.clone()),
            None,
            Some(&dir),
        )
        .unwrap();
        let stamp = stamp_of(&made).expect("it is there");

        // Removed and replaced by a *different* empty file. Emptiness alone
        // cannot tell them apart, which is the point.
        std::fs::remove_file(&made).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::File::create(&made).unwrap();

        let refused = perform(
            &deco_editor::FileOperation::DeleteIfEmpty {
                path: made.clone(),
                directory: false,
                expect: Some(stamp),
            },
            None,
            Some(&dir),
        );
        // On a filesystem whose timestamps are too coarse to tell the two
        // apart, this legitimately goes through — the stamp is evidence, not
        // proof, and the test says so rather than pretending otherwise.
        if refused.is_err() {
            assert!(made.exists(), "the replacement was left alone");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Answers every listing the tree is waiting on, from the real disk.
    fn read_tree(session: &mut Session, root: &Path) {
        for _ in 0..crate::files::MAX_DEPTH {
            let Some(wanted) = session.directory_wanted() else {
                return;
            };
            let entries = crate::files::list_dir(root, &wanted, &session.settings);
            session.fill_directory(&wanted, entries);
        }
    }

    #[test]
    fn a_rename_refuses_a_source_that_is_no_longer_what_was_shown() {
        let dir = scratch("rename-type");
        let was_a_dir = dir.join("d");
        // The tree read a directory; something has since put a file there.
        std::fs::write(&was_a_dir, "not a directory any more\n").unwrap();

        perform(
            &deco_editor::FileOperation::Rename {
                from: was_a_dir.clone(),
                to: dir.join("renamed"),
                expect: None,
                directory: true,
            },
            None,
            Some(&dir),
        )
        .expect_err("what the tree showed is not what is there");
        assert!(
            was_a_dir.is_file() && !dir.join("renamed").exists(),
            "and nothing was moved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_parent_that_stopped_being_a_directory_is_forgotten() {
        let dir = scratch("stale-parent");
        let inner = dir.join("d");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("a.rs"), "a\n").unwrap();

        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );
        session.resize(100, 30);
        session.set_workspace_root(&dir);
        read_tree(&mut session, &dir);
        session.run("workbench.files.action.focusFilesExplorer", None, 0);
        session.run("list.expand", None, 0);
        read_tree(&mut session, &dir);
        assert!(session
            .known_paths_under(&inner)
            .contains(&inner.join("a.rs")));

        // `d` becomes a file. Creating in it fails, and re-reading `d` alone
        // would answer "empty folder" for ever.
        std::fs::remove_dir_all(&inner).unwrap();
        std::fs::write(&inner, "now a file\n").unwrap();
        let mut lsp = Lsp::new(&mut session, None);
        reconcile_failure(
            &mut session,
            &mut lsp,
            &deco_editor::FileOperation::CreateFile(inner.join("new.rs")),
        );

        // Re-reading `d` alone proves nothing: invalidating it empties what the
        // tree knows either way, and `list_dir` would answer "empty folder"
        // for ever. The listing *above* is what has to be asked for again, so
        // that `d` stops being shown as a directory at all.
        read_tree(&mut session, &dir);
        let row = session
            .explorer()
            .and_then(|e| e.rows().into_iter().find(|r| r.path == inner))
            .expect("it is still listed, as something");
        assert!(
            !row.is_dir,
            "the tree must stop showing a path as a folder once it is a file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_rename_makes_the_tree_re_read_its_source() {
        // The wiring, not just the model: a failed rename has to reach
        // `invalidate_subtree`, or the source directory keeps showing children
        // it may no longer have.
        let dir = scratch("failed-rename");
        let inner = dir.join("d");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("old.rs"), "old\n").unwrap();

        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );
        session.resize(100, 30);
        session.set_workspace_root(&dir);
        read_tree(&mut session, &dir);
        session.run("workbench.files.action.focusFilesExplorer", None, 0);
        session.run("list.expand", None, 0);
        read_tree(&mut session, &dir);
        assert!(
            session
                .known_paths_under(&inner)
                .contains(&inner.join("old.rs")),
            "the tree has read `d`"
        );
        assert_eq!(session.directory_wanted(), None, "and wants nothing");

        // The rename fails, because `d` went away underneath it.
        std::fs::remove_dir_all(&inner).unwrap();
        let mut lsp = Lsp::new(&mut session, None);
        reconcile_failure(
            &mut session,
            &mut lsp,
            &deco_editor::FileOperation::Rename {
                from: inner.clone(),
                to: dir.join("renamed"),
                expect: None,
                directory: false,
            },
        );

        // Specifically the *source's* listing. Invalidating the parent alone
        // would satisfy a looser assertion and prove nothing about this fix.
        assert!(
            session.known_paths_under(&inner).is_empty(),
            "the source's own listing must be asked for again rather than \
             believed: {:?}",
            session.known_paths_under(&inner)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_delete_raises_the_barrier_from_what_the_tree_knew() {
        // The evidence the barrier rests on is what the tree had *read*, so a
        // partial delete has to be caught even when no tab was open on any of
        // it. Taking that snapshot after invalidating the subtree silently
        // disabled this whole half.
        let dir = scratch("partial-evidence");
        let inner = dir.join("d");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("a.rs"), "a\n").unwrap();
        std::fs::write(inner.join("b.rs"), "b\n").unwrap();

        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );
        session.resize(100, 30);
        session.set_workspace_root(&dir);
        read_tree(&mut session, &dir);

        // Something to lose, recorded before `d` is read — creating invalidates
        // the directory it lands in.
        session.run("workbench.files.action.focusFilesExplorer", None, 0);
        let deco_editor::Outcome::FileOperation(created) = session.create_in_tree("kept.rs", false)
        else {
            panic!("expected a create");
        };
        session.file_operation_done(&created);
        std::fs::write(dir.join("kept.rs"), "").unwrap();
        read_tree(&mut session, &dir);

        // Open `d` so the tree reads what is in it.
        session.run("workbench.files.action.focusFilesExplorer", None, 0);
        while session
            .explorer()
            .and_then(|e| e.selection())
            .map(|row| row.name != "d")
            .unwrap_or(false)
        {
            session.run("list.focusDown", None, 0);
        }
        session.run("list.expand", None, 0);
        read_tree(&mut session, &dir);
        assert!(
            session
                .known_paths_under(&inner)
                .contains(&inner.join("a.rs")),
            "the tree read `d`, so it knows what was in it"
        );
        assert!(
            session.can_undo_file_operation(),
            "and the create is undoable"
        );

        // A recursive delete that took one child and then stopped. No tab was
        // ever open on any of it.
        std::fs::remove_file(inner.join("a.rs")).unwrap();
        let mut lsp = Lsp::new(&mut session, None);
        reconcile_failed_delete(
            &mut session,
            &mut lsp,
            &deco_editor::FileOperation::Delete {
                path: inner.clone(),
                directory: true,
            },
        );

        assert!(
            !session.can_undo_file_operation(),
            "part of the tree went, so the earlier undos describe a state that \
             never existed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remote_directorys_contents_come_out_of_the_flat_listing() {
        // What `fs.list` answers: every file, relative to the workspace.
        let files = vec![
            "Cargo.toml".to_owned(),
            "src/main.rs".to_owned(),
            "src/deep/mod.rs".to_owned(),
            "src/deep/inner/x.rs".to_owned(),
        ];
        let root = Path::new("/remote/w");

        let mut top: Vec<String> = remote_children(&files, root, root)
            .into_iter()
            .map(|e| format!("{}{}", e.name, if e.is_dir { "/" } else { "" }))
            .collect();
        top.sort();
        assert_eq!(
            top,
            ["Cargo.toml", "src/"],
            "a directory is implied by its files"
        );

        let mut inner: Vec<String> = remote_children(&files, root, &root.join("src"))
            .into_iter()
            .map(|e| format!("{}{}", e.name, if e.is_dir { "/" } else { "" }))
            .collect();
        inner.sort();
        assert_eq!(
            inner,
            ["deep/", "main.rs"],
            "named once, not once per file inside it"
        );
    }

    #[test]
    fn a_directory_outside_the_remote_workspace_has_no_children() {
        let files = vec!["a.rs".to_owned()];
        assert!(
            remote_children(&files, Path::new("/remote/w"), Path::new("/elsewhere")).is_empty()
        );
    }

    /// An absolute path made of `parts`, on any platform.
    ///
    /// Not a literal `/w/src/main.rs`: that is absolute on Unix and **relative** on
    /// Windows, where a root needs a drive letter or a UNC prefix. A test that
    /// hard-coded one would be asserting about the host rather than about
    /// [`resolve_path`].
    fn absolute(parts: &[&str]) -> PathBuf {
        let mut path = std::env::current_dir().expect("a working directory");
        for part in parts {
            path.push(part);
        }
        path
    }

    /// A session whose `settings.json` contains `keys`.
    fn configured(json: &str) -> Session {
        let mut settings = deco_config::Settings::with_defaults();
        settings
            .load_layer(deco_config::Scope::User, json)
            .expect("valid settings");
        let mut session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);
        session.open(PathBuf::from("/w/a.rs"), "fn main() {}\n");
        session
    }

    // ---- files.autoSave ---------------------------------------------------

    /// Settings with `files.autoSave` set to `value`.
    fn auto_save(value: &str) -> deco_config::EditorSettings {
        let mut settings = deco_config::Settings::with_defaults();
        settings
            .load_layer(
                deco_config::Scope::User,
                &format!(r#"{{"files.autoSave": "{value}", "files.autoSaveDelay": 500}}"#),
            )
            .expect("valid settings");
        deco_config::EditorSettings::resolve(&settings, None)
    }

    #[test]
    fn off_never_saves_however_long_the_idle() {
        let settings = auto_save("off");
        assert!(!auto_save_due(&settings, 60_000, true));
    }

    #[test]
    fn after_delay_saves_once_the_delay_has_passed() {
        let settings = auto_save("afterDelay");
        assert!(!auto_save_due(&settings, 499, true), "not yet");
        assert!(auto_save_due(&settings, 500, true), "on the boundary");
        assert!(auto_save_due(&settings, 5_000, true));
    }

    #[test]
    fn a_clean_document_is_never_saved() {
        // Otherwise an idle editor would rewrite the same bytes every second, and a
        // file's modification time is something other tools watch.
        let settings = auto_save("afterDelay");
        assert!(!auto_save_due(&settings, 60_000, false));
    }

    #[test]
    fn the_delay_cannot_be_set_to_zero() {
        // Zero would mean a write per keystroke, which is what the delay exists to
        // avoid.
        let mut settings = deco_config::Settings::with_defaults();
        settings
            .load_layer(
                deco_config::Scope::User,
                r#"{"files.autoSave": "afterDelay", "files.autoSaveDelay": 0}"#,
            )
            .unwrap();
        let resolved = deco_config::EditorSettings::resolve(&settings, None);
        assert!(
            resolved.auto_save_delay >= 100,
            "{}",
            resolved.auto_save_delay
        );
    }

    #[test]
    fn the_focus_values_are_reported_rather_than_silently_ignored() {
        // A setting that does nothing and says nothing is worse than one that is
        // refused, so this goes in the session's problem list — where an unknown
        // colour theme already goes.
        for value in ["onFocusChange", "onWindowChange"] {
            let problem = auto_save(value)
                .unsupported()
                .unwrap_or_else(|| panic!("{value} should report"));
            assert!(problem.contains(value), "{problem}");
            assert!(problem.contains("not honoured"), "{problem}");
        }
        assert_eq!(auto_save("afterDelay").unsupported(), None);
        assert_eq!(auto_save("off").unsupported(), None);
    }

    #[test]
    fn a_cursor_style_nobody_set_leaves_the_terminals_own_alone() {
        // A terminal's caret is already configured, and replacing it with VS Code's
        // default on behalf of somebody who never mentioned it would be deco
        // overruling a preference it was not asked about.
        assert_eq!(wanted_cursor_style(&configured("{}")), None);
    }

    #[test]
    fn setting_the_style_is_asking_for_it_even_at_its_default_value() {
        // Writing the key down is the ask; which value it holds is a separate
        // question. `line` is the default *value*, not the absence of one.
        assert_eq!(
            wanted_cursor_style(&configured(r#"{"editor.cursorStyle": "line"}"#)),
            Some(deco_config::CursorStyle::Line)
        );
        assert_eq!(
            wanted_cursor_style(&configured(r#"{"editor.cursorStyle": "block"}"#)),
            Some(deco_config::CursorStyle::Block)
        );
    }

    #[test]
    fn a_language_override_of_the_style_is_honoured() {
        let session = configured(r#"{"[rust]": {"editor.cursorStyle": "underline"}}"#);
        assert_eq!(
            wanted_cursor_style(&session),
            Some(deco_config::CursorStyle::Underline)
        );
    }

    #[test]
    fn the_shapes_a_terminal_does_not_have_collapse_rather_than_being_refused() {
        // DECSCUSR has a bar, a block and an underline, and no thin or hollow
        // variant of any of them.
        use deco_config::CursorStyle;
        assert_eq!(
            to_decscusr(CursorStyle::LineThin),
            to_decscusr(CursorStyle::Line)
        );
        assert_eq!(
            to_decscusr(CursorStyle::BlockOutline),
            to_decscusr(CursorStyle::Block)
        );
        assert_eq!(
            to_decscusr(CursorStyle::UnderlineThin),
            to_decscusr(CursorStyle::Underline)
        );
        assert_ne!(
            to_decscusr(CursorStyle::Line),
            to_decscusr(CursorStyle::Block),
            "the three that do exist stay distinct"
        );
    }

    #[test]
    fn saving_an_untitled_document_writes_nothing() {
        // The regression guard for a silent overwrite: `save` used to fall back to
        // the file deco was started with, so `ctrl+n`, a keystroke and `ctrl+s`
        // replaced an unrelated file and reported `Saved a.rs`.
        let dir = std::env::temp_dir().join(format!("deco-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let started_with = dir.join("a.rs");
        std::fs::write(&started_with, "fn main() {}\n").unwrap();

        let mut session = Session::with_defaults();
        session.resize(80, 10);
        session.run("type", Some(&serde_json::json!({ "text": "scratch" })), 0);
        assert!(session.document.path.is_none(), "an untitled document");

        save(&mut session, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&started_with).unwrap(),
            "fn main() {}\n",
            "the file deco was started with must be untouched"
        );
        assert!(session.document.dirty, "and nothing was saved");
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        // Quick open, search results and go-to-definition all hand over absolute
        // paths, so this has to be identity for them.
        let target = absolute(&["w", "src", "main.rs"]);
        assert_eq!(
            resolve_path(&target, Some(&absolute(&["elsewhere", "a.rs"])), None, None),
            target
        );
    }

    #[test]
    fn a_relative_path_is_taken_against_the_workspace_root() {
        // Not the process's working directory: a path that worked when deco was
        // launched from the project and not from anywhere else would be worse than
        // one that always means the same thing.
        assert_eq!(
            resolve_path(
                Path::new("src/main.rs"),
                Some(&absolute(&["w", "notes.txt"])),
                None,
                None
            ),
            absolute(&["w", "src", "main.rs"])
        );
    }

    #[test]
    fn a_tilde_expands_to_the_home_directory() {
        // A directory this test names, rather than whichever one the machine
        // running it has: the rule is the same either way, and a fixture that
        // depends on the runner's `$HOME` is a test that passes for a reason it
        // is not asserting.
        let home = absolute(&["home", "u"]);
        assert_eq!(
            resolve_path(Path::new("~/notes.txt"), None, Some(&home), None),
            home.join("notes.txt")
        );
        assert_eq!(resolve_path(Path::new("~"), None, Some(&home), None), home);
    }

    #[test]
    fn a_tilde_with_no_home_to_expand_to_is_left_as_typed() {
        // Nothing sensible to expand to, and inventing a directory would be worse
        // than handing the path onward as it was written.
        assert_eq!(
            resolve_path(Path::new("~/notes.txt"), None, None, None),
            PathBuf::from("~/notes.txt")
        );
    }

    #[test]
    fn a_relative_path_with_no_workspace_falls_back_to_the_working_directory() {
        // The bug this pins: `deco` with no file, then `ctrl+s` and a name, used
        // to store the name unresolved. Every other way of opening that same file
        // produces an absolute path, which never compares equal to a relative one
        // — so the file opened a second time, in a second buffer, with a second
        // undo history, and whichever tab was saved last won.
        let cwd = absolute(&["home", "u", "project"]);
        assert_eq!(
            resolve_path(Path::new("notes.txt"), None, None, Some(&cwd)),
            cwd.join("notes.txt")
        );
    }

    #[test]
    fn the_workspace_root_still_wins_over_the_working_directory() {
        // The fallback is a fallback. A session started with a file resolves
        // against that file's directory, wherever deco was launched from.
        let root = absolute(&["w", "notes.txt"]);
        let cwd = absolute(&["somewhere", "else"]);
        assert_eq!(
            resolve_path(Path::new("a.txt"), Some(&root), None, Some(&cwd)),
            absolute(&["w", "a.txt"])
        );
    }

    #[test]
    fn a_tilde_inside_a_name_is_part_of_the_name() {
        // `~backup` is a file called `~backup`, and `a~b` is not a home directory.
        // Only a leading `~` on its own component means one.
        assert_eq!(
            resolve_path(
                Path::new("~backup"),
                Some(&absolute(&["w", "a.txt"])),
                None,
                None
            ),
            absolute(&["w", "~backup"])
        );
    }

    #[test]
    fn colours_are_converted_to_truecolor() {
        assert_eq!(
            to_crossterm(Rgba::rgb(1, 2, 3)),
            Color::Rgb { r: 1, g: 2, b: 3 }
        );
    }

    #[test]
    fn painting_writes_every_span_and_positions_the_cursor() {
        let frame = Frame {
            rows: vec![crate::render::Row {
                spans: vec![crate::render::Span {
                    text: "hi".to_owned(),
                    fg: Rgba::WHITE,
                    bg: Rgba::BLACK,
                }],
            }],
            cursor: Some((3, 0)),
        };
        let mut out: Vec<u8> = Vec::new();
        paint(&mut out, &frame, None).unwrap();
        let written = String::from_utf8_lossy(&out);
        assert!(written.contains("hi"));
        // The cursor is shown again once it has been positioned.
        assert!(written.contains("\u{1b}[?25h"), "cursor was never re-shown");
    }

    #[test]
    fn painting_never_emits_a_span_s_own_escape_sequence() {
        // The regression guard for the whole class, asserted at the write rather than
        // on the substitution: OSC 52 sets the clipboard on every terminal that
        // supports it, and a span's text can come from a file name or a search result
        // rather than from the renderer's own substitution.
        let frame = Frame {
            rows: vec![crate::render::Row {
                spans: vec![crate::render::Span {
                    text: "\u{1b}]52;c;aGVsbG8=\u{7}\u{1b}[31m".to_owned(),
                    fg: Rgba::WHITE,
                    bg: Rgba::BLACK,
                }],
            }],
            cursor: None,
        };
        let mut out: Vec<u8> = Vec::new();
        paint(&mut out, &frame, None).unwrap();

        assert!(
            !out.windows(4).any(|w| w == b"\x1b]52"),
            "an OSC 52 clipboard write reached the terminal"
        );
        assert!(
            !out.windows(5).any(|w| w == b"\x1b[31m"),
            "a colour sequence from the span reached the terminal"
        );
        assert!(!out.contains(&0x07), "a bell reached the terminal");
        // What it should have written instead.
        let written = String::from_utf8_lossy(&out);
        assert!(written.contains("␛]52;c;aGVsbG8=␇"), "{written:?}");
    }

    #[test]
    fn painting_a_frame_with_no_cursor_leaves_it_hidden() {
        let frame = Frame {
            rows: vec![crate::render::Row::default()],
            cursor: None,
        };
        let mut out: Vec<u8> = Vec::new();
        paint(&mut out, &frame, None).unwrap();
        assert!(!String::from_utf8_lossy(&out).contains("\u{1b}[?25h"));
    }

    #[test]
    fn saving_an_untitled_document_says_so_instead_of_failing_silently() {
        let mut session = Session::with_defaults();
        save(&mut session, None).unwrap();
        assert!(session.status.as_deref().unwrap().contains("no filename"));
    }
}
