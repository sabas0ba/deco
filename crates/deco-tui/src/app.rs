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
                    // Quick open named a file; reading it is this side's job.
                    Outcome::OpenFile(target) => match std::fs::read_to_string(&target) {
                        Ok(text) => session.open(target, &text),
                        Err(error) => {
                            session.status =
                                Some(format!("could not open {}: {error}", target.display()));
                        }
                    },
                    // Commands the core cannot implement because they need a
                    // language server. Named rather than guessed at, so a
                    // mistyped binding still reports as unknown.
                    Outcome::Frontend(command) => match command.as_str() {
                        "editor.action.showHover" => lsp.request_hover(session),
                        "editor.action.revealDefinition" => lsp.request_definition(session),
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
                if session.document.path != path_before {
                    // A hover or completion list anchored in the old document
                    // would describe text that is no longer on screen.
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
        ("editor.action.triggerSuggest", "Trigger Suggest"),
        ("editor.action.formatDocument", "Format Document"),
        ("editor.action.formatSelection", "Format Selection"),
        ("workbench.action.quickOpen", "Go to File"),
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
