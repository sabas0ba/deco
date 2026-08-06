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
    session.resize(width as usize, height.saturating_sub(1) as usize);

    let mut lsp = Lsp::new(session, workspace_root(path.as_deref()));
    lsp.attach(session);

    let mut dirty = true;
    loop {
        if dirty {
            let frame = render::render(session, width as usize, height as usize);
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
                match session.handle_chord(chord, now_ms) {
                    Outcome::Quit => break,
                    Outcome::Save => {
                        save(session, path.as_ref())?;
                        lsp.saved(session);
                    }
                    _ => {}
                }
                // After the command, not before: the server has to be told
                // about the text as it now is.
                lsp.changed(session);
            }
            Event::Resize(new_width, new_height) => {
                width = new_width;
                height = new_height;
                session.resize(width as usize, height.saturating_sub(1) as usize);
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
