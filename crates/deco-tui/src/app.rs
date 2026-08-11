//! The terminal event loop.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use crossterm::event::{self, Event};
use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::{cursor, execute, queue, terminal};
use deco_editor::{Outcome, Session};
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
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
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
fn paint(out: &mut impl Write, frame: &Frame) -> Result<()> {
    queue!(out, cursor::Hide, cursor::MoveTo(0, 0))?;
    for (row_index, row) in frame.rows.iter().enumerate() {
        queue!(out, cursor::MoveTo(0, row_index as u16))?;
        for span in &row.spans {
            queue!(
                out,
                SetForegroundColor(to_crossterm(span.fg)),
                SetBackgroundColor(to_crossterm(span.bg))
            )?;
            out.write_all(span.text.as_bytes())?;
        }
        queue!(out, ResetColor)?;
    }
    if let Some((x, y)) = frame.cursor {
        queue!(out, cursor::MoveTo(x, y), cursor::Show)?;
    }
    out.flush()?;
    Ok(())
}

/// Runs the editor until the user quits.
pub fn run(session: &mut Session, path: Option<PathBuf>) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let started = Instant::now();
    let mut out = io::stdout();

    let (mut width, mut height) = terminal::size().unwrap_or((80, 24));
    resize(session, width, height);

    let mut lsp = Lsp::new(session, workspace_root(path.as_deref()));
    lsp.attach(session);
    session.frontend_commands = frontend_commands();

    let mut dirty = true;
    loop {
        if dirty {
            // The find bar costs a row, so the text area's height depends on
            // whether it is open — which the last keypress may have changed.
            resize(session, width, height);
            let frame =
                render::render_with_hover(session, width as usize, height as usize, lsp.hover());
            paint(&mut out, &frame)?;
            dirty = false;
        }

        // Waiting with a timeout rather than blocking on `event::read`, so a
        // language server's diagnostics arrive while the user is idle instead of
        // on their next keystroke. The interval is a compromise: short enough
        // that results feel immediate, long enough that an idle editor is not
        // spinning.
        if !event::poll(LSP_POLL_INTERVAL)? {
            dirty |= lsp.poll(session);
            continue;
        }
        dirty |= lsp.poll(session);

        match event::read()? {
            Event::Key(key) => {
                let Some(chord) = chord_from_event(key) else {
                    continue;
                };
                dirty = true;
                // Elapsed milliseconds is the monotonic clock the editor uses
                // for undo grouping.
                let now_ms = started.elapsed().as_millis() as u64;
                // Remembered before the chord runs: a printable key both inserts
                // itself and narrows an open list, and afterwards there is no way
                // to tell which key it was.
                let typed = printable(&chord);
                // Remembered so a tab switch can be seen afterwards: the chord
                // may put a different document on screen, and the language
                // server has to be told which file it is now looking at.
                let path_before = session.document.path.clone();
                // And the language, which `ctrl+k m` can change without the
                // document changing — a different language is a different server.
                let language_before = session.document.language().map(str::to_owned);
                let was_backspace = chord.key
                    == deco_keymap::keys::Key::Named(deco_keymap::keys::NamedKey::Backspace)
                    && !chord.modifiers.ctrl
                    && !chord.modifiers.alt
                    && !chord.modifiers.meta;

                match session.handle_chord(chord, now_ms) {
                    Outcome::Quit => break,
                    Outcome::Save => {
                        save(session, path.as_ref())?;
                        lsp.saved(session);
                    }
                    // The picker named a theme; reading it is this side's job.
                    Outcome::LoadTheme { label, path } => {
                        match load_theme(&label, path.as_deref()) {
                            Ok(theme) => {
                                if let Outcome::Message(report) = session.set_theme(theme) {
                                    session.status = Some(report);
                                }
                            }
                            Err(error) => {
                                session.status = Some(error.clone());
                                session.problems.push(error);
                            }
                        }
                    }
                    // The prompt named a path; writing it is this side's job, and
                    // so is working out what it meant.
                    Outcome::SaveAs(target) => {
                        let target = resolve_path(&target, path.as_deref());
                        match write_file(&target, &session.save_contents()) {
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
                    // Quick open and search named a file; reading it is this
                    // side's job.
                    Outcome::OpenFile { path: target, at } => {
                        // Resolved because the path may have been typed: `ctrl+o`
                        // accepts `~/notes.txt` and `src/main.rs`. Quick open and
                        // search hand over absolute paths, for which this is
                        // identity.
                        let target = resolve_path(&target, path.as_deref());
                        match std::fs::read_to_string(&target) {
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
                        "workbench.action.gotoSymbol" => lsp.request_document_symbols(session),
                        // The extension directories have to be walked from here,
                        // for the same reason the file list is.
                        "workbench.action.selectTheme" => {
                            let available = crate::themes::list(&extension_roots());
                            session.offer_themes(crate::themes::rows(&available));
                        }
                        "closeHoverWidget" => lsp.dismiss_hover(),
                        "editor.action.triggerSuggest" => lsp.request_completion(
                            session,
                            deco_lsp::requests::CompletionTrigger::Invoked,
                        ),
                        // The workspace has to be walked from here: the core has
                        // no filesystem.
                        "workbench.action.quickOpen" => {
                            let root = workspace_root(path.as_deref())
                                .unwrap_or_else(|| PathBuf::from("."));
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
                        // Every file under the workspace root, searched here for
                        // the same reason the file list is walked here.
                        "workbench.action.findInFiles" => match session.search_seed() {
                            Some(needle) => {
                                let root = workspace_root(path.as_deref())
                                    .unwrap_or_else(|| PathBuf::from("."));
                                let found = crate::files::search(
                                    &root,
                                    &session.settings,
                                    &needle,
                                    session.find.options(),
                                );
                                let (truncated, count) = (found.truncated, found.matches.len());
                                session.offer_search_results(&needle, found.matches);
                                if truncated {
                                    session.status = Some(format!(
                                        "{count} matches for `{needle}`, and there may be more"
                                    ));
                                }
                            }
                            None => {
                                session.status = Some("nothing to search for".to_owned());
                            }
                        },
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
                dirty |= lsp.cursor_moved(session);
            }
            Event::Resize(new_width, new_height) => {
                width = new_width;
                height = new_height;
                resize(session, width, height);
                dirty = true;
            }
            _ => {}
        }
    }

    // Before the terminal guard restores the screen, so a server that takes a
    // moment to stop does so while the editor still looks alive.
    lsp.detach();
    Ok(())
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
        ("workbench.action.gotoSymbol", "Go to Symbol in Editor"),
        ("workbench.action.selectTheme", "Color Theme"),
        ("editor.action.triggerSuggest", "Trigger Suggest"),
        ("editor.action.formatDocument", "Format Document"),
        ("editor.action.formatSelection", "Format Selection"),
        ("workbench.action.quickOpen", "Go to File"),
        ("workbench.action.findInFiles", "Find in Files"),
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
    let text_height = (height as usize).saturating_sub(render::chrome_height(session));
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
fn resolve_path(typed: &Path, started_with: Option<&Path>) -> PathBuf {
    let text = typed.to_string_lossy();
    if let Some(rest) = text.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            if let Some(home) = deco_config::paths::Env::from_process().home {
                // `trim_start_matches` rather than `join`, because joining an
                // absolute `/notes.txt` onto the home directory discards it.
                return home.join(rest.trim_start_matches(['/', '\\']));
            }
        }
    }
    if typed.is_absolute() {
        return typed.to_path_buf();
    }
    match workspace_root(started_with) {
        Some(root) => root.join(typed),
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
fn save(session: &mut Session, path: Option<&PathBuf>) -> Result<()> {
    let target = session.document.path.clone().or_else(|| path.cloned());
    match target {
        Some(path) => {
            std::fs::write(&path, session.save_contents())
                .with_context(|| format!("could not write {}", path.display()))?;
            session.mark_saved();
            session.status = Some(format!("Saved {}", path.display()));
        }
        None => {
            // Losing the user's work silently would be far worse than saying so.
            session.status = Some("This document has no filename yet".to_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_theme::Rgba;

    #[test]
    fn an_absolute_path_is_left_alone() {
        // Quick open, search results and go-to-definition all hand over absolute
        // paths, so this has to be identity for them.
        assert_eq!(
            resolve_path(
                Path::new("/w/src/main.rs"),
                Some(Path::new("/elsewhere/a.rs"))
            ),
            PathBuf::from("/w/src/main.rs")
        );
    }

    #[test]
    fn a_relative_path_is_taken_against_the_workspace_root() {
        // Not the process's working directory: a path that worked when deco was
        // launched from the project and not from anywhere else would be worse than
        // one that always means the same thing.
        assert_eq!(
            resolve_path(Path::new("src/main.rs"), Some(Path::new("/w/notes.txt"))),
            PathBuf::from("/w/src/main.rs")
        );
    }

    #[test]
    fn a_tilde_expands_to_the_home_directory() {
        let Some(home) = deco_config::paths::Env::from_process().home else {
            // No HOME in this environment; there is nothing to expand to and the
            // path is left as typed.
            return;
        };
        assert_eq!(
            resolve_path(Path::new("~/notes.txt"), None),
            home.join("notes.txt")
        );
        assert_eq!(resolve_path(Path::new("~"), None), home);
    }

    #[test]
    fn a_tilde_inside_a_name_is_part_of_the_name() {
        // `~backup` is a file called `~backup`, and `a~b` is not a home directory.
        // Only a leading `~` on its own component means one.
        let resolved = resolve_path(Path::new("~backup"), Some(Path::new("/w/a.txt")));
        assert_eq!(resolved, PathBuf::from("/w/~backup"));
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
        paint(&mut out, &frame).unwrap();
        let written = String::from_utf8_lossy(&out);
        assert!(written.contains("hi"));
        // The cursor is shown again once it has been positioned.
        assert!(written.contains("\u{1b}[?25h"), "cursor was never re-shown");
    }

    #[test]
    fn painting_a_frame_with_no_cursor_leaves_it_hidden() {
        let frame = Frame {
            rows: vec![crate::render::Row::default()],
            cursor: None,
        };
        let mut out: Vec<u8> = Vec::new();
        paint(&mut out, &frame).unwrap();
        assert!(!String::from_utf8_lossy(&out).contains("\u{1b}[?25h"));
    }

    #[test]
    fn saving_an_untitled_document_says_so_instead_of_failing_silently() {
        let mut session = Session::with_defaults();
        save(&mut session, None).unwrap();
        assert!(session.status.as_deref().unwrap().contains("no filename"));
    }
}
