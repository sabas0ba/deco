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
/// A panic inside the event loop must not leave the terminal in raw mode on the
/// alternate screen with no echo, so teardown runs on drop rather than at the
/// end of `run`.
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
        // The cursor shape is restored along with the screen. `editor.cursorStyle`
        // applies only while deco is running and must not persist in the shell.
        let _ = execute!(
            io::stdout(),
            cursor::SetCursorStyle::DefaultUserShape,
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// The caret shape set by `editor.cursorStyle`, or `None` to keep the
/// terminal's own shape.
///
/// Returns `None` when the user has not set the key. The terminal's caret is
/// already configured, usually as a block, and deco does not replace it with VS
/// Code's default. Setting the key, even to its default value, applies it.
///
/// Two shapes have no terminal equivalent. DECSCUSR has a bar, a block and an
/// underline, with no thin or hollow variants. `line-thin` is drawn as `line`
/// and `block-outline` as `block`, as the closest available shapes.
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
    // Always blinking, which is VS Code's `editor.cursorBlinking` default. deco
    // does not resolve that setting.
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
            // All text is made printable before it reaches the terminal. The renderer
            // already substitutes document text. This covers everything else, such as
            // a file name containing an escape byte or a search result containing a
            // line from another file. A terminal interprets what is written to it, and
            // `\x1b]52;c;…` writes the clipboard.
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
/// A pure function of its three inputs, so the rule is testable without a terminal
/// and without waiting for the delay.
///
/// The frontend owns the clock. `deco-editor` receives `now_ms` per keystroke and
/// has no timer, which keeps every command deterministic under test. The idle timer
/// therefore lives in the event loop. The same poll that receives language server
/// diagnostics also detects that the delay has passed.
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
/// The server applies its own skip list (`.git`, `node_modules`, `target`) and
/// does not know the user's settings, so the remaining filtering happens here.
/// A `files.exclude` pattern can therefore make a search report fewer matches
/// than the server counted. The count shown is the count after filtering, which
/// matches what is on screen.
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
                // The id is the path requested from the server when the entry
                // is opened. In a remote session it is relative to the workspace.
                &entry.path,
                &format!("{}:{}: {}", entry.path, entry.line + 1, entry.text),
                deco_core::position::Position::new(entry.line, entry.character),
            )
        })
        .collect()
}

/// The distinct files a set of matches named, in the order they first appeared.
///
/// A search reports one entry per *match*, but a file with many matches is
/// opened once and gets one transaction. Deduplication is done in place rather
/// than through a set to keep the search order. That order determines the order
/// in which files are opened, and therefore the tab order.
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
/// Holds what the editor needs in addition to the connection. Language servers
/// are also started on the remote machine, which requires the transport and the
/// directory the remote side is serving.
pub struct RemoteSession {
    /// The connection files are read and written through.
    pub client: deco_remote::Client,
    /// A separate connection owned by the source-control worker.
    ///
    /// Optional for compatibility with an older remote server that does not
    /// advertise the SCM methods yet.
    pub scm: Option<deco_remote::Client>,
    /// Where language servers run, which is the same machine.
    pub location: crate::lsp::Location,
}

/// The editor, optionally against a remote workspace.
///
/// When `remote` is present, every file the session reads and writes is on the
/// remote machine. This is a session mode rather than a per-document property.
/// deco does not open local and remote files in one window, because mixing them
/// would make every path ambiguous.
///
/// Language servers and project search both run on the machine that holds the
/// files, because only that machine can read them.
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

        // Wait with a timeout rather than blocking on `event::read`, so language
        // server diagnostics arrive while the user is idle instead of on the next
        // keystroke. The interval is short enough for results to appear promptly
        // and long enough that an idle editor does not busy-loop.
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

    // Shut down before the terminal guard restores the screen, so the editor
    // stays visible while a slow server stops.
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
/// The values read from the process by default (the home directory, the
/// extension directories and the terminal size) are fields rather than calls
/// inside the loop. A test can then use a temporary home directory instead of
/// the test runner's.
pub struct Options {
    /// The file deco was started with. Relative paths and the language server's
    /// workspace root are resolved against its directory.
    pub started_with: Option<PathBuf>,
    /// Present when every file this session reads and writes is on another
    /// machine. It also determines where language servers are started.
    pub remote: Option<RemoteSession>,
    /// Every directory that may hold installed extensions.
    pub extension_roots: Vec<PathBuf>,
    /// Where extension permission decisions are remembered between sessions.
    ///
    /// `None` keeps decisions only for this session. Scenarios use this, and deco
    /// uses it when it cannot determine its configuration directory.
    pub permissions_file: Option<PathBuf>,
    /// What a leading `~` in a typed path expands to.
    pub home: Option<PathBuf>,
    /// The base for a relative typed path when the session has no file that
    /// defines a workspace, for example a save-as after starting `deco` with no
    /// arguments.
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
/// [`run_with`] adds an event source and a painter to this type. It reads keys
/// from crossterm and writes frames to stdout. Everything else is here: what a
/// chord does, which of its outcomes need a filesystem, and when an idle editor
/// saves. This split lets a test drive the loop without a terminal, against a
/// real workspace on disk.
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
    /// Stored rather than recomputed per keystroke, so the tree always reads
    /// from the root it was built against.
    tree_root: Option<PathBuf>,
    /// The workspace's `git` status, collected outside the loop.
    scm: crate::scm::Scm,
}

impl Driver {
    /// Starts a language server, scans the extension directories and sizes the
    /// session. These are the one-time steps before the first keystroke.
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

        let (location, remote, remote_scm, is_remote) = match remote {
            Some(RemoteSession {
                client,
                scm,
                location,
            }) => (location, Some(client), scm, true),
            None => (crate::lsp::Location::Here, None, None, false),
        };
        let workspace_roots = match &location {
            crate::lsp::Location::Remote { workspace, .. } => Some(workspace.clone()),
            crate::lsp::Location::Here => workspace_root(started_with.as_deref()),
        };
        let mut lsp =
            Lsp::with_location(session, workspace_root(started_with.as_deref()), location);
        // Started the same way in both cases. A remote session runs its servers on
        // the machine that holds the files, because only that machine can read
        // them.
        lsp.attach(session);
        session.frontend_commands = frontend_commands();

        // The file tree root. The language server and quick open use the same
        // root, so `ctrl+p` and the tree show the same workspace. In a remote
        // session it is the workspace given to the server, a path on the remote
        // machine.
        let tree_root = workspace_roots.clone();
        if let Some(root) = tree_root.clone() {
            session.set_workspace_root(root);
        }

        // Installed extensions are listed in the palette whether or not they have
        // started. Invoking one starts it. The scan happens here, like the theme
        // scan, because the core has no filesystem access.
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
                    // The workspace path as the machine holding it names it. In a
                    // remote session this is a directory on the remote machine. An
                    // extension granted `readFile: workspace` gets exactly the
                    // directory the session is editing, and nothing when there is
                    // no workspace.
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
            scm: match remote_scm {
                Some(client) => crate::scm::Scm::remote(
                    client,
                    tree_root.clone().expect("a remote session has a workspace"),
                ),
                None => crate::scm::Scm::new(
                    &session.settings,
                    // An older remote server does not advertise the SCM methods.
                    // Its path must still never be passed to the local git.
                    tree_root.clone().filter(|_| !is_remote),
                ),
            },
            tree_root,
        }
    }

    /// Whether anything has changed since the last frame was taken.
    pub fn needs_redraw(&self) -> bool {
        self.dirty
    }

    /// Returns the frame to paint and marks the driver as drawn.
    pub fn frame(&mut self, session: &mut Session) -> Frame {
        // The find bar uses a row, so the text area's height depends on whether
        // it is open, which the last keypress may have changed.
        resize(session, self.width, self.height);
        // The marks are derived from the buffer, so they are updated here rather
        // than in the renderer. `render` holds the session by shared reference,
        // and the alternative would be a diff per frame. When nothing has been
        // typed, this is one version comparison per document.
        session.refresh_diffs();
        self.dirty = false;
        // Pass both overlays, not just the hover, so the completion list is
        // drawn. When both are present, the list is shown because it is the one
        // being interacted with.
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

    /// Collects pending messages from the language server and the extension hosts.
    pub fn poll(&mut self, session: &mut Session, now_ms: u64) {
        // Destructured so the language server and the hosts can both be
        // advanced while the connection is borrowed. Their file requests use the
        // same connection the editor reads and writes with, because in a remote
        // session the files are on the remote machine.
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
        // Polled after the others, so a `git status` result that arrives in this
        // iteration appears on the same frame as the change that caused it.
        *dirty |= self.scm.poll(session);
        if session.take_checkout_completed() {
            reload_after_checkout(session, lsp, remote.as_mut(), self.tree_root.as_deref());
            *dirty = true;
        }
    }

    /// Handles an interval with no keystroke: the same poll, plus the auto-save
    /// check.
    ///
    /// Checked on the idle path only. Saving while keys are still arriving would
    /// write once per keystroke, which the delay exists to prevent.
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

    /// Stops the language server and the source-control worker after the loop ends.
    pub fn shutdown(&mut self) {
        self.lsp.detach();
        self.scm.shutdown();
    }

    /// Processes one keystroke. The core handles the chord, then this method
    /// handles any outcome that needs resources the core does not have.
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
            scm,
            ..
        } = self;
        *dirty = true;
        // Recorded before the chord runs. A printable key both inserts itself
        // and narrows an open list, and the key cannot be recovered afterwards.
        let typed = printable(&chord);
        // Recorded to detect a tab switch. The chord may show a different
        // document, and the language server must be told which file is active.
        let path_before = session.document.path.clone();
        let was_dirty = session.document.dirty;
        // The language is recorded too. `ctrl+k m` can change it without
        // changing the document, and a different language uses a different
        // server.
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
            // The picker selected a theme. The frontend reads it.
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
            // The prompt returned a path. The frontend resolves it and writes
            // the file.
            Outcome::SaveAs(target) => {
                // In a remote session the typed name refers to the remote side,
                // so it is used exactly as typed. `resolve_path` applies to the
                // local machine (`~` expansion and this process's working
                // directory), and a locally resolved name would be unknown to
                // the server. The session's other paths are remote paths relative
                // to the served workspace. The server rejects every path outside
                // that workspace, including a locally resolved absolute path, so
                // mixing the two namespaces would make every later save fail.
                //
                // Save-as therefore cannot copy a remote file to the local
                // machine. As described in `run_with`, a session has a single
                // workspace.
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
                        // A different path is a different document to the
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
            // The core requested the file's contents on disk. The frontend
            // reads it.
            Outcome::Revert => {
                let target = session.document.path.clone();
                // Read the file from the machine that holds it. In a remote
                // session the path is relative to the remote workspace. Reading
                // it locally would resolve it against this process's working
                // directory, which either fails or finds an unrelated local file.
                // Revert discards unsaved work, so reading the wrong file must
                // be avoided.
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
                        // Keep the unsaved edits. A read failure must not
                        // discard them.
                        let path = target.unwrap_or_default();
                        session.status =
                            Some(format!("could not read {}: {error}", path.display()));
                    }
                    None => {}
                }
            }
            // The user chose a remembered permission to forget. The decision is
            // withdrawn, and the extension asks again the next time it needs
            // that permission.
            Outcome::ForgetExtensionPermission(chosen) => {
                hosts.forget_permission(session, &chosen);
            }
            // The user answered an extension's permission request. The request
            // has been waiting in `hosts`; this sends the reply.
            Outcome::ExtensionConsent { allow } => {
                let mut files = match remote.as_mut() {
                    Some(client) => crate::extensions::Files::Remote(client),
                    None => crate::extensions::Files::Here,
                };
                hosts.answer_consent(session, allow, &mut files, now_ms);
            }
            // The new name has been entered. The language server computes the
            // changes and applying them needs a filesystem, so both steps are
            // handled here rather than in the core.
            Outcome::Rename { new_name } => lsp.request_rename(session, &new_name),
            // The chosen action. The list it indexes belongs to the frontend.
            Outcome::CodeAction(id) => {
                // Use the connection when there is one, because the action's edit
                // names files on the machine where the server runs.
                let mut files = match remote.as_mut() {
                    Some(client) => crate::extensions::Files::Remote(client),
                    None => crate::extensions::Files::Here,
                };
                lsp.run_code_action(session, &id, &mut files);
            }
            // The prompt returned a search query. The frontend searches the
            // workspace. In a remote session the search runs on the remote
            // machine, because the files are there. A local search would
            // report matches in files the editor is not showing.
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
                    // A failed search leaves the session unchanged. Nothing was
                    // opened or modified, so there is nothing to undo.
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
            // The search finds *which files* match. The session builds the edit
            // as one undoable action. Both steps run here because only the
            // frontend knows where the files are.
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
                    // A failed search leaves the session unchanged. Nothing was
                    // opened or modified, so there is nothing to undo.
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
                                // occurrences it did not find. The report says
                                // so explicitly, because the user must know
                                // whether every occurrence was replaced.
                                if truncated {
                                    report.push_str(
                                        " — the search hit its limit, so there may be more",
                                    );
                                }
                                session.status = Some(report);
                            }
                            // Nothing was changed, because the whole plan is
                            // built before anything is applied.
                            Err(error) => session.status = Some(error.to_string()),
                        }
                    }
                }
            }
            Outcome::SaveAll => {
                // The core runs the loop and builds the report. The frontend
                // only performs the write, because only the frontend has
                // filesystem access.
                let outcome = session.save_all(write_file);
                if let Outcome::Message(report) = outcome {
                    session.status = Some(report);
                }
                lsp.saved(session);
            }
            // The source-control view chose an operation. Performing it needs
            // the filesystem, which the frontend has.
            Outcome::GitOperation(operation) => {
                scm.apply(session, &operation);
                *dirty = true;
            }
            Outcome::GitBranches => {
                scm.branches(session);
                *dirty = true;
            }
            Outcome::GitCheckoutPreview(target) => {
                scm.checkout_plan(session, target);
                *dirty = true;
            }
            Outcome::GitComparison(request) => {
                scm.compare(session, request);
                *dirty = true;
            }
            Outcome::FileOperation(operation) => {
                let root = session
                    .explorer()
                    .map(|explorer| explorer.root().to_path_buf());
                match perform(&operation, remote.as_mut(), root.as_deref()) {
                    Ok(()) => {
                        // Record the current state of a rename's destination or
                        // a newly created path. Both are undone by acting on that
                        // path, and the stamp lets undo detect whether something
                        // else has replaced it. This runs before
                        // `file_operation_done`, which makes the undo entry
                        // available.
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
                        // A delete may have detached open documents. The server
                        // still has them open under URIs that no longer exist.
                        lsp.close_deleted(session);
                        // A new file is opened immediately, as in VS Code. It is
                        // not read back because it is known to be empty.
                        if let deco_editor::FileOperation::CreateFile(path) = &operation {
                            session.open(path.clone(), "");
                        }
                    }
                    Err(error) => {
                        session.file_operation_failed(&operation, &error.to_string());
                        // Only for a delete attempted on this machine. A remote
                        // session rejects every mutation before touching either
                        // filesystem, and its paths are remote paths. A local
                        // `exists` check would report every one of them as
                        // missing and release every tab in the workspace for a
                        // delete that never happened.
                        if remote.is_none() {
                            reconcile_failure(session, lsp, &operation);
                        }
                    }
                }
            }
            // Quick open or search selected a file. The frontend reads it.
            Outcome::OpenFile { path: target, at } => {
                // Resolved because the path may have been typed: `ctrl+o`
                // accepts `~/notes.txt` and `src/main.rs`. Quick open and
                // search pass absolute paths, which resolve to themselves.
                // In remote mode the path is the server's, relative to the
                // workspace it serves. Resolving it against a local directory
                // would produce a path on the wrong machine.
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
                            // changed since it was searched.
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
            // Commands the core cannot implement, for example because they
            // need a language server. Each is matched by name, so a mistyped
            // binding is still reported as not implemented.
            Outcome::Frontend(command) => match command.as_str() {
                "editor.action.showHover" => lsp.request_hover(session),
                "editor.action.revealDefinition" => lsp.request_definition(session),
                "editor.action.goToReferences" => lsp.request_references(session),
                "editor.action.rename" => lsp.offer_rename(session),
                "editor.action.quickFix" => lsp.request_code_actions(session),
                "workbench.action.gotoSymbol" => lsp.request_document_symbols(session),
                // The extension directories are scanned here, for the same
                // reason as the file list: the core has no filesystem access.
                "workbench.action.selectTheme" => {
                    let available = crate::themes::list(extension_roots);
                    session.offer_themes(crate::themes::rows(&available));
                }
                "closeHoverWidget" => lsp.dismiss_hover(),
                "editor.action.triggerSuggest" => {
                    lsp.request_completion(session, deco_lsp::requests::CompletionTrigger::Invoked)
                }
                // The workspace is listed here because the core has no
                // filesystem access. The listing comes from the machine that
                // holds the files.
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
                // Matched before the extension commands, because it is deco's
                // own command rather than one an extension registered.
                "deco.extensions.forgetPermission" => {
                    hosts.offer_permissions(session);
                }
                // An extension command. Its identifier depends on what is
                // installed. Checked last so an extension cannot shadow a
                // core command.
                other if hosts.run_command(session, other) => {}
                other => {
                    session.status = Some(format!("{other} is not implemented yet"));
                }
            },
            _ => {}
        }

        // Local checkout completes in the command arm above. Remote checkout
        // completes from `poll`, which runs the same reconciliation there.
        if session.take_checkout_completed() {
            reload_after_checkout(session, lsp, remote.as_mut(), tree_root.as_deref());
        }

        // While the find bar or a prompt has keyboard focus, keystrokes edit
        // the query, not the document. A completion list left open would be
        // narrowed by text that was not typed into the file, so it is closed
        // along with any hover.
        if session.find.visible() || session.prompt.is_some() {
            lsp.dismiss_suggest();
            lsp.dismiss_hover();
        }
        // A printable key both typed itself and narrowed the list. A
        // backspace both deleted and widened it. Both are handled after the
        // command, so the list and the document match.
        else if let Some(c) = typed {
            if !lsp.typed(session, c) {
                // No list was open, so this may be a trigger character, such
                // as `.` or `::`, that should open one.
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
        // The chord may have switched tabs, for example with ctrl+tab, ctrl+w
        // or a jump that opened a file. `attach` is idempotent, so for the same
        // document it is only a comparison. For a new document it sends
        // didClose/didOpen or starts the right server, and the stored
        // diagnostics for the document are collected.
        if session.document.path != path_before
            || session.document.language().map(str::to_owned) != language_before
        {
            // A hover or completion list from the old document would describe
            // text that is no longer on screen. After a language change it
            // would come from a server no longer responsible for the file.
            lsp.dismiss_hover();
            lsp.dismiss_suggest();
            lsp.attach(session);
            lsp.refresh_diagnostics(session);
        }
        // After the command, so the server receives the current text.
        lsp.changed(session);
        // Close a hover that describes the previous cursor position.
        *dirty |= lsp.cursor_moved(session);

        // The auto-save timer restarts on every edit, so it does not fire
        // while the user is still typing. It is cleared when the document is
        // clean again, whether this keystroke saved it or undid back to the
        // saved state.
        if session.document.dirty {
            *edited_at = Some(now_ms);
        } else if was_dirty {
            *edited_at = None;
        }

        // Any keystroke may have opened a directory in the tree, for example
        // `ctrl+b` showing it for the first time, `right` on a folder, or a
        // reveal expanding down to a file. Filling the tree here rather than in
        // each arm keeps directory reading in one place, which makes it easier
        // to make asynchronous later.
        if let Some(root) = tree_root.clone() {
            fill_tree(session, &root, remote.as_mut())?;
        }
        Ok(Flow::Continue)
    }

    /// The extension hosts' state, for a test or a status line.
    pub fn hosts(&self) -> &crate::extensions::Hosts {
        &self.hosts
    }

    /// The language-server client, for the same reason.
    pub fn lsp(&self) -> &Lsp {
        &self.lsp
    }
}

/// Re-reads every open file after Git replaced the working tree.
///
/// A file absent on the target branch is detached rather than closed. Its old
/// text remains visible and marked dirty, so it is not lost. All buffers had to
/// be clean before checkout, so this copy is unambiguous.
fn reload_after_checkout(
    session: &mut Session,
    lsp: &mut Lsp,
    mut remote: Option<&mut deco_remote::Client>,
    tree_root: Option<&Path>,
) {
    let paths = session.open_paths();
    let mut detached = 0usize;
    for path in paths {
        let read = match remote.as_deref_mut() {
            Some(client) => client
                .read(&path.display().to_string())
                .map_err(|error| error.to_string()),
            None => std::fs::read_to_string(&path).map_err(|error| error.to_string()),
        };
        match read {
            Ok(text) => {
                session.reload_open(&path, &text);
            }
            Err(_) => detached += session.detach_tabs_under(&path),
        }
    }
    if let Some(root) = tree_root {
        session.invalidate_subtree(root);
    }
    lsp.close_deleted(session);
    lsp.attach(session);
    lsp.changed(session);
    if detached > 0 {
        session.status = Some(format!(
            "switched branches; kept {detached} missing tab{} as unsaved text",
            if detached == 1 { "" } else { "s" }
        ));
    }
}

/// The commands this frontend implements, for the command palette.
///
/// Only the commands the core cannot run on its own, because they need the
/// language-server client or other resources in this frontend. The core cannot
/// know which forwarded commands a frontend implements, so each frontend lists
/// its own. This keeps the palette from offering a command such as `Go to
/// References` that would only report "not implemented yet".
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
/// The remainder is chrome: the status bar, and the find bar when it is open.
/// This must be recomputed whenever the terminal or the find bar changes size,
/// or the last line of the file is hidden under the bar.
fn resize(session: &mut Session, width: u16, height: u16) {
    let height = height as usize;
    let text_height = height.saturating_sub(render::chrome_height(session, height));
    session.resize(width as usize, text_height);
}

/// The character a chord types, if it types one.
///
/// Mirrors the rule in `Session::handle_chord`: an unmodified printable key
/// inserts itself, and a key with Ctrl, Alt or Meta is a command. The rule is
/// duplicated rather than exposed from the core. The core decides what a key
/// *does*; this only needs the typed character to narrow the completion list.
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
/// Language server messages do not arrive through the keyboard, so the loop
/// cannot block on input. 50ms is below the threshold at which a diagnostic
/// appears delayed, and 20 wakeups a second on an idle editor has no measurable
/// cost.
const LSP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Carries out one of the tree's file operations.
///
/// The core has already validated the operation: the name is valid, the path is
/// inside the workspace, and nothing in the listing conflicts. The remaining
/// failures cannot be predicted by checks, such as permissions, a full disk, or
/// another program changing the file first.
///
/// A new file uses `create_new` rather than `File::create`, so a file that
/// appeared after the check is not silently truncated. This race is why the
/// frontend checks again rather than trusting the tree's listing.
fn perform(
    operation: &deco_editor::FileOperation,
    remote: Option<&mut deco_remote::Client>,
    root: Option<&Path>,
) -> io::Result<()> {
    use deco_editor::FileOperation;

    if remote.is_some() {
        // The protocol supports read, write and list, but not create, rename or
        // delete. Reject the operation explicitly. Performing it locally would
        // change a file on this machine and report success for the remote one.
        return Err(io::Error::other(
            "changing files over a remote connection is not implemented yet",
        ));
    }

    // The session checked that the path is inside the workspace, but without a
    // filesystem it can only compare the path text. A directory replaced by a
    // symlink after the tree listed it resolves elsewhere, and every call below
    // follows the link. `New File` in a `src` that is now a link to `/outside`
    // would therefore write outside the workspace.
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
            // Check against the kind the tree was showing, not the kind on disk
            // now. Otherwise a directory replaced by a file since it was read
            // would be moved as a directory, and every tab below the old path
            // would be retargeted onto a regular file. Delete has the same
            // protection.
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
            // An undo carries the file's stamp from when it was moved. If the
            // stamp no longer matches, something replaced the file. Moving it
            // back would move a different file and point the buffer at it, so
            // the next save would overwrite its contents.
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
            // `rename` would overwrite an existing `to` on Unix. The tree already
            // rejected that, but the destination can appear between its listing
            // and this call, so check again to avoid silently replacing a file.
            //
            // A case-only rename is exempt. On the case-insensitive filesystems
            // that macOS and Windows usually use, `Foo.rs` and `foo.rs` are the
            // same file, so this check would find the source itself and reject
            // every change of capitalisation. Canonicalising both paths
            // distinguishes the same file from a different file in the way.
            //
            // `symlink_metadata` is used rather than `exists`, which follows
            // links. A dangling symlink at the destination returns `false` from
            // `exists` and would be silently replaced. The tree does not show
            // such links either, because `list_dir` reports only real files and
            // directories, so this case occurs even without a race.
            if std::fs::symlink_metadata(to).is_ok() && !same_file(from, to) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", to.display()),
                ));
            }
            std::fs::rename(from, to)
        }
        // Dispatched on what the tree was showing when the user confirmed, not
        // on what is on disk now. The tree has no watcher. If a file has since
        // been replaced by a directory, checking the disk would find a directory
        // and delete it recursively after the user confirmed deleting a file.
        // `remove_file` on a directory fails, which is the intended result.
        FileOperation::Delete {
            path,
            directory: true,
        } => std::fs::remove_dir_all(path),
        FileOperation::Delete {
            path,
            directory: false,
        } => std::fs::remove_file(path),
        // Undoing a create. `remove_dir` is used rather than `remove_dir_all`
        // because it fails on a non-empty directory. The operating system
        // enforces this, so there is no race with a separate check.
        FileOperation::DeleteIfEmpty {
            path,
            directory,
            expect,
        } => {
            // Being empty does not identify the entry. Another program can
            // remove what was created and leave a different empty file or
            // directory in its place, and undoing the create would delete it.
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
                // A file must be checked explicitly because there is no
                // "unlink if empty". Data written between this check and the
                // unlink is lost. This window is accepted, because refusing
                // instead would mean a create could never be undone on a
                // filesystem shared with other programs.
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

/// Resynchronises the tree and the tabs after a mutation reported failure.
///
/// Only for a mutation attempted on this machine. A remote session rejects every
/// mutation before touching either filesystem, and its paths are remote paths,
/// so a local `exists` would report all of them as missing.
fn reconcile_failure(session: &mut Session, lsp: &mut Lsp, operation: &deco_editor::FileOperation) {
    // After any failure, the listing that allowed the operation may be stale.
    // For example, a create rejected because another program took the name
    // means the tree is missing that name, and without a re-read every retry
    // fails the same way. There is no watcher to detect this otherwise.
    if let Some(parent) = operation.parent() {
        session.invalidate_directory(parent);
        // Re-reading the parent does not help if it is no longer a directory.
        // `list_dir` returns an empty listing for an unreadable or
        // non-directory path, so the tree would keep showing an empty folder
        // and every create in it would keep failing. The entry that would
        // correct this is in the listing above, so that listing is re-read too
        // and the cached subtree below is dropped.
        if !parent.is_dir() {
            if let Some(above) = parent.parent() {
                session.invalidate_directory(above);
            }
            session.forget_subtree(parent);
        }
    }

    // Also invalidate a failed rename's *source* and its subtree. A rename
    // usually fails because something changed, possibly including removal of
    // the source. Invalidating only the parent would leave the source's cached
    // listing describing children it may no longer have. If the directory is
    // later recreated under the old name, those stale entries would reappear
    // permanently, because expanding a directory whose listing is already
    // `Known` does not re-read it.
    //
    // Invalidated rather than forgotten, because the source usually still
    // exists. Re-reading it is correct, and discarding its expansion state is
    // not.
    if let deco_editor::FileOperation::Rename { from, .. } = operation {
        session.invalidate_subtree(from);
    }

    reconcile_failed_delete(session, lsp, operation);
}

/// Resynchronises the tree and the tabs after a delete reported failure.
///
/// `remove_dir_all` can delete part of a tree and then stop, so a failure does
/// not mean nothing was deleted. Everything here is based on what is on disk now
/// rather than on what was requested.
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

    // Collect the paths the tree knew *before* anything is invalidated.
    // Invalidating resets those listings to "not read yet", so a scan afterwards
    // would find nothing to check, and only open tabs could trigger the
    // barrier below.
    let known: Vec<PathBuf> = if recursive {
        session.known_paths_under(path)
    } else {
        Vec::new()
    };

    // Invalidate the subtree as well. The directory itself may survive, and
    // re-reading only its parent would leave its own listing, and every
    // expanded listing below it, describing files that no longer exist.
    session.invalidate_subtree(path);

    // Only a recursive delete can partially succeed. `remove_file` and
    // `remove_dir` either succeed or do not. Even a recursive failure may have
    // deleted nothing, for example a `PermissionDenied` on the directory itself.
    // The barrier is therefore raised on *evidence* rather than on the error
    // kind: a path the tree knew about, or a file an open tab holds, no longer
    // exists.
    //
    // The check is limited to what has been read, because the tree only knows
    // directories that were expanded. A file removed from a collapsed directory
    // is not detected here, and neither is the undo it would invalidate. This
    // limitation is accepted to avoid walking the whole workspace on a
    // keystroke.
    let mut anything_went = known
        .iter()
        .any(|known| matches!(known.try_exists(), Ok(false)));

    // Checked per file, because part of a directory may survive and only the
    // disk shows which part.
    for held in session.open_paths_under(path) {
        // `try_exists` rather than `exists`, which returns `false` for a file it
        // cannot stat. The permission problem that stopped the delete can also
        // hide a file that still exists, and its tab must not be detached. Only
        // a definite `false` detaches; an error leaves the tab attached.
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

/// Rejects a directory that resolves outside `root` once symlinks are followed.
///
/// `canonicalize` turns the session's text-based check into a check of where
/// the path actually leads. This covers the common case of a link in the
/// workspace created by a build or a package manager. It does not cover a race:
/// an ancestor can still be replaced between this check and the operation.
/// Preventing that requires `openat` with `O_NOFOLLOW` and a directory handle
/// per component, which the standard library does not provide on any platform.
/// `docs/files.md` documents this window next to the related rename window.
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

/// The size and modification time of the file at `path`, or `None` if it cannot
/// be read.
fn stamp_of(path: &Path) -> Option<deco_editor::files::Stamp> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(deco_editor::files::Stamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// Whether two paths name the same file on disk.
///
/// Compares canonical paths rather than strings, so a case-only rename on a
/// case-insensitive filesystem is recognised as the same file. A path that
/// cannot be canonicalised is treated as a different file. The caller only asks
/// when `to` exists, so a failure here means something changed and rejecting
/// the rename is the safe result.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Answers every directory listing the file tree is waiting on.
///
/// A loop rather than one listing, because the tree requests one at a time and
/// each answer can produce the next request. Revealing `src/deep/main.rs` needs
/// `src` before the tree can request `src/deep`. Bounded by [`files::MAX_DEPTH`]
/// so a self-referencing symlink cannot cause an endless loop, the same guard
/// the walk uses.
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
            // The protocol only lists the whole workspace, so a directory's
            // contents are derived from that listing. This is less efficient
            // than a per-directory call, but no worse than `ctrl+p`, which
            // requests the same list on every press. A `list_dir` method would
            // improve this and requires a protocol change.
            Some(client) => match client.list() {
                Ok(files) => remote_children(&files, root, &dir),
                Err(error) => {
                    session.status = Some(format!("could not list the remote: {error}"));
                    // Filled empty rather than left pending. Otherwise the tree
                    // requests the same unreachable directory on every keystroke.
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
/// Directories are inferred from the paths because a flat listing contains only
/// files. Every path with another component after `dir/` implies a directory
/// that the tree must be able to show and expand.
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
/// The file's own directory, because deco has no concept of an open folder. A
/// server given an unsuitable root indexes the wrong tree. A server given no
/// root falls back to single-file mode, which is worse than a narrow directory.
fn workspace_root(path: Option<&Path>) -> Option<PathBuf> {
    let path = path?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    absolute.parent().map(Path::to_path_buf)
}

/// Resolves a typed path.
///
/// `~` is expanded, and a relative path is resolved against the workspace root:
/// the directory the editor was started in or the directory of the file it was
/// started with, the same root quick open walks. Resolving against the process's
/// working directory instead would make a path work only when deco was launched
/// from the project directory.
///
/// An absolute path is returned unchanged, so callers that already have one
/// (quick open, search results, a go-to-definition jump) can also use this.
///
/// `home` and `cwd` are parameters rather than read here, so tests can use
/// directories they control instead of those on the test machine.
///
/// The working directory is the fallback for a session started with no file,
/// for example `deco` with no arguments, then `ctrl+s` and a name. Without the
/// fallback, a relative path from here would reach [`Session::rename_to`] as the
/// document's path and never compare equal to the absolute path produced by
/// every other way of opening the same file. Saving an untitled buffer as
/// `notes.txt` and then choosing `notes.txt` from quick open would open it
/// twice, in two buffers with separate undo histories. `deco::startup::absolute`
/// prevents the same bug for a path on the command line.
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
        // returned as typed.
        None => typed.to_path_buf(),
    }
}

/// Every directory that may hold installed extensions.
///
/// deco's own and VS Code's, so a theme installed for VS Code is offered without
/// being copied. Settings are also read from VS Code in the same one-way manner.
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
///
/// **Only to its own path.** This previously fell back to the file deco was
/// started with when the document had no path. That was correct with a single
/// document but caused a silent overwrite once tabs were added, because an
/// untitled tab and the started-with file are different documents. The
/// parameter was removed rather than left unused, so no write can target the
/// wrong file.
///
/// A document with no path does not reach here, because [`Session`] turns
/// `ctrl+s` into the save-as prompt. The arm remains as a guard rather than a
/// `panic!`.
fn save(session: &mut Session, remote: Option<&mut deco_remote::Client>) -> Result<()> {
    let Some(path) = session.document.path.clone() else {
        session.status = Some("This document has no filename yet".to_owned());
        return Ok(());
    };

    // A failed remote write is reported and is *not* fatal. The connection can
    // drop while the editor keeps the text and can retry. A failed local write
    // remains fatal, so the editor never reports "saved" when the write failed.
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

        // The tree's listing showed this name as free, but the file exists.
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
        // Created, then edited and saved.
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

        // A file that is still empty is deleted.
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
        // A dangling symlink. `exists` returns false for it, and the tree does
        // not show it.
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

        // The session's text-based check passes: the path is inside the workspace.
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

        // Another program removes `b.rs` and puts a different file in its
        // place. Undoing the rename must not move the new file.
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

        // Removed and replaced by a *different* empty file, which emptiness
        // alone cannot distinguish.
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
        // On a filesystem whose timestamps are too coarse to distinguish the
        // two, the delete succeeds. The stamp is a heuristic, not proof, so the
        // test only asserts when the delete was rejected.
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
        // The tree read a directory, which has since been replaced by a file.
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
        // would always return an empty folder.
        std::fs::remove_dir_all(&inner).unwrap();
        std::fs::write(&inner, "now a file\n").unwrap();
        let mut lsp = Lsp::new(&mut session, None);
        reconcile_failure(
            &mut session,
            &mut lsp,
            &deco_editor::FileOperation::CreateFile(inner.join("new.rs")),
        );

        // Checking `d` alone is not sufficient: invalidating it empties what the
        // tree knows either way, and `list_dir` would keep returning an empty
        // folder. The listing *above* must be re-read so that `d` is no longer
        // shown as a directory.
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
        // Tests the frontend path, not just the model. A failed rename must call
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

        // The rename fails because `d` was removed.
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

        // Check the *source's* listing specifically. Invalidating only the
        // parent would pass a looser assertion without testing this fix.
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
        // The barrier uses what the tree had *read* as evidence, so a partial
        // delete must be detected even when no tab was open on any of it.
        // Taking the snapshot after invalidating the subtree previously
        // disabled this check.
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

        // An undoable operation, recorded before `d` is read, because creating
        // invalidates the directory the new entry is in.
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

        // A recursive delete that removed one child and then stopped. No tab
        // was open on any of it.
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
        // The `fs.list` response: every file, relative to the workspace.
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
    /// Not a literal `/w/src/main.rs`, which is absolute on Unix and **relative**
    /// on Windows, where a root needs a drive letter or a UNC prefix. A hard-coded
    /// path would make the test depend on the host rather than on [`resolve_path`].
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
        // Otherwise an idle editor would rewrite the same bytes repeatedly and
        // change the modification time, which other tools watch.
        let settings = auto_save("afterDelay");
        assert!(!auto_save_due(&settings, 60_000, false));
    }

    #[test]
    fn the_delay_cannot_be_set_to_zero() {
        // Zero would mean a write per keystroke, which the delay exists to
        // prevent.
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
        // An unsupported value must be reported rather than silently ignored.
        // It goes in the session's problem list, like an unknown colour theme.
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
        // The terminal's caret is already configured. deco does not replace it
        // with VS Code's default unless the user sets the key.
        assert_eq!(wanted_cursor_style(&configured("{}")), None);
    }

    #[test]
    fn setting_the_style_is_asking_for_it_even_at_its_default_value() {
        // Setting the key applies it regardless of its value. `line` is the
        // default *value*, not an unset key.
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
        // Regression test for a silent overwrite. `save` previously fell back to
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
        // Quick open, search results and go-to-definition all pass absolute
        // paths, which must be returned unchanged.
        let target = absolute(&["w", "src", "main.rs"]);
        assert_eq!(
            resolve_path(&target, Some(&absolute(&["elsewhere", "a.rs"])), None, None),
            target
        );
    }

    #[test]
    fn a_relative_path_is_taken_against_the_workspace_root() {
        // Not the process's working directory, so a path means the same thing
        // regardless of where deco was launched.
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
        // Use a directory chosen by the test rather than the runner's `$HOME`,
        // so the result does not depend on the machine.
        let home = absolute(&["home", "u"]);
        assert_eq!(
            resolve_path(Path::new("~/notes.txt"), None, Some(&home), None),
            home.join("notes.txt")
        );
        assert_eq!(resolve_path(Path::new("~"), None, Some(&home), None), home);
    }

    #[test]
    fn a_tilde_with_no_home_to_expand_to_is_left_as_typed() {
        // With no home directory, the path is returned as typed rather than
        // expanded to an invented directory.
        assert_eq!(
            resolve_path(Path::new("~/notes.txt"), None, None, None),
            PathBuf::from("~/notes.txt")
        );
    }

    #[test]
    fn a_relative_path_with_no_workspace_falls_back_to_the_working_directory() {
        // Regression test: `deco` with no file, then `ctrl+s` and a name,
        // previously stored the name unresolved. Every other way of opening the
        // same file produces an absolute path, which never compares equal to a
        // relative one. The file was then opened a second time, in a second
        // buffer with a separate undo history, and the last tab saved overwrote
        // the other.
        let cwd = absolute(&["home", "u", "project"]);
        assert_eq!(
            resolve_path(Path::new("notes.txt"), None, None, Some(&cwd)),
            cwd.join("notes.txt")
        );
    }

    #[test]
    fn the_workspace_root_still_wins_over_the_working_directory() {
        // The working directory is only a fallback. A session started with a
        // file resolves against that file's directory, wherever deco was
        // launched from.
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
        // Only a leading `~` as a whole component refers to the home directory.
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
        // Regression test for this class of bug, asserted on the output rather than
        // on the substitution. OSC 52 sets the clipboard on every terminal that
        // supports it, and a span's text can come from a file name or a search
        // result that did not pass through the renderer's substitution.
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
