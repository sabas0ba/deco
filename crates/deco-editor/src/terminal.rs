//! The integrated terminal as the session sees it: a screen in the panel, and
//! the bytes waiting to be sent to the program running in it.
//!
//! The session has no processes. It asks the frontend to start a shell with
//! [`Outcome::StartTerminal`](crate::Outcome::StartTerminal), receives the
//! shell's output through [`Session::terminal_output`](crate::Session::terminal_output),
//! and queues keys and status replies for the frontend to write. This keeps
//! the screen and the key routing testable without a pseudo-terminal.

use deco_term::Terminal;

/// The lines of history kept when `terminal.integrated.scrollback` is unset,
/// as in VS Code.
pub const DEFAULT_SCROLLBACK: usize = 1000;

/// The most history lines kept, whatever the setting says.
const MAX_SCROLLBACK: usize = 100_000;

/// The commands a key still runs while the terminal has the keyboard.
///
/// Every other key goes to the program. The list follows the part of VS
/// Code's `terminal.integrated.commandsToSkipShell` default that deco
/// implements: moving between regions, the palette and quick open, the panel
/// and side-bar toggles, quitting, and the terminal's own commands.
pub const SKIP_SHELL: &[&str] = &[
    "workbench.action.terminal.toggleTerminal",
    "workbench.action.terminal.new",
    "workbench.action.terminal.focusNext",
    "workbench.action.terminal.focusPrevious",
    "workbench.action.terminal.kill",
    "workbench.action.terminal.paste",
    "workbench.action.terminal.scrollUpPage",
    "workbench.action.terminal.scrollDownPage",
    "workbench.action.terminal.scrollToTop",
    "workbench.action.terminal.scrollToBottom",
    "workbench.action.togglePanel",
    "workbench.action.closePanel",
    "workbench.action.toggleSidebarVisibility",
    "workbench.action.showCommands",
    "workbench.action.quickOpen",
    "workbench.action.quit",
    "workbench.action.focusActiveEditorGroup",
    "workbench.action.focusFirstEditorGroup",
    "workbench.action.focusSecondEditorGroup",
    "workbench.action.focusSideBar",
    "workbench.files.action.focusFilesExplorer",
    "workbench.view.explorer",
    "workbench.view.scm",
    "workbench.actions.view.problems",
];

/// One of the terminals in the panel.
#[derive(Debug, Clone)]
pub struct TerminalPanel {
    /// The number the frontend knows its process by, unique in the session.
    id: u32,
    terminal: Terminal,
    /// How many lines the view is scrolled back into the history.
    back: usize,
    /// The program's exit code, once it has exited.
    exited: Option<Option<i32>>,
    /// Keys and status replies not yet written to the program.
    input: Vec<u8>,
}

impl TerminalPanel {
    /// An empty terminal of `cols` by `rows`, known to the frontend as `id`.
    pub fn new(id: u32, cols: usize, rows: usize, scrollback: usize) -> Self {
        Self {
            id,
            terminal: Terminal::new(cols, rows, scrollback.min(MAX_SCROLLBACK)),
            back: 0,
            exited: None,
            input: Vec::new(),
        }
    }

    /// The number the frontend knows this terminal's process by.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// What the terminal shows.
    pub fn screen(&self) -> &deco_term::Screen {
        self.terminal.screen()
    }

    /// How many lines the view is scrolled back.
    pub fn back(&self) -> usize {
        self.back
    }

    /// Whether the program has exited, and with which code if it is known.
    pub fn exited(&self) -> Option<Option<i32>> {
        self.exited
    }

    /// The program's output.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        let before = self.terminal.screen().scrolled();
        self.terminal.feed(bytes);
        let screen = self.terminal.screen_mut();
        // A view scrolled into the history keeps showing the same lines while
        // new output arrives below. Counted by lines scrolled rather than by
        // the history's length, which stays the same once it is full.
        if self.back > 0 {
            let added = usize::try_from(screen.scrolled() - before).unwrap_or(usize::MAX);
            self.back = self.back.saturating_add(added).min(screen.scrollback_len());
        }
        let replies = screen.take_responses();
        self.input.extend_from_slice(&replies);
    }

    /// Queues `bytes` as typed input, and returns the view to the bottom.
    pub(crate) fn send(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
        self.back = 0;
    }

    pub(crate) fn take_input(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.input)
    }

    pub(crate) fn exit(&mut self, code: Option<i32>) {
        self.exited = Some(code);
        self.input.clear();
    }

    /// Resizes the screen. Returns whether the size changed.
    pub(crate) fn resize(&mut self, cols: usize, rows: usize) -> bool {
        let screen = self.terminal.screen();
        if (screen.cols(), screen.rows()) == (cols, rows) {
            return false;
        }
        self.terminal.screen_mut().resize(cols, rows);
        self.back = self.back.min(self.terminal.screen().scrollback_len());
        true
    }

    /// Scrolls the view by `lines`, positive into the history.
    pub(crate) fn scroll(&mut self, lines: isize) {
        let limit = self.terminal.screen().scrollback_len();
        self.back = self.back.saturating_add_signed(lines).min(limit);
    }

    /// Scrolls to the oldest line or back to the bottom.
    pub(crate) fn scroll_to(&mut self, top: bool) {
        self.back = if top {
            self.terminal.screen().scrollback_len()
        } else {
            0
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top_line(panel: &TerminalPanel) -> String {
        panel
            .screen()
            .line(0, panel.back())
            .map(deco_term::screen::Line::text)
            .unwrap_or_default()
    }

    #[test]
    fn a_view_in_the_history_stays_put_while_output_arrives_even_when_the_history_is_full() {
        let mut panel = TerminalPanel::new(1, 10, 2, 5);
        let lines: String = (0..20).map(|n| format!("{n}\r\n")).collect();
        panel.feed(lines.as_bytes());
        assert_eq!(panel.screen().scrollback_len(), 5, "full");
        panel.scroll(1);
        let shown = top_line(&panel);
        panel.feed(b"20\r\n");
        assert_eq!(top_line(&panel), shown);
        assert_eq!(panel.back(), 2);
    }
}
