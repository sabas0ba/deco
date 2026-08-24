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

use deco_core::position::Position;
use deco_editor::Session;
use deco_lsp::process::Consent;
use deco_lsp::requests::CompletionTrigger;
use deco_lsp::server::ServerConfig;
use deco_lsp::supervisor::{Supervisor, Update};
use deco_lsp::uri::PathMap;
use deco_lsp::{Hover, RequestId, ServerRegistry, Trust};

use crate::suggest::Suggest;

/// How long to wait for the handshake before giving up on a server.
///
/// Shorter than [`deco_lsp::supervisor::INITIALIZE_TIMEOUT`] because this
/// happens while the user is looking at an empty screen waiting for their file.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// The same, for a server on the other end of a transport.
const REMOTE_STARTUP_TIMEOUT: Duration = Duration::from_secs(45);

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
    paths: PathMap,
    /// Where a server should run, and how to reach it if that is elsewhere.
    location: Location,
    /// The hover currently on screen, and the position it describes.
    ///
    /// Kept with its position so it can be dismissed the moment the cursor
    /// leaves the range the server answered about — a hover box describing a
    /// different token than the one under the caret is actively misleading.
    hover: Option<ShownHover>,
    /// The hover request in flight, if any. At most one: a second would race
    /// the first and whichever answered last would win, which is not
    /// necessarily the one the user is waiting for.
    hover_request: Option<RequestId>,
    /// The go-to-definition request in flight.
    definition_request: Option<RequestId>,
    /// The outstanding `textDocument/references` request, if any.
    references_request: Option<RequestId>,
    /// The outstanding `semanticTokens/full` request, if any.
    semantic_request: Option<RequestId>,
    /// The outstanding `documentSymbol` request, and which document asked.
    ///
    /// The path is kept because the answer names positions and not a file, and the
    /// user may have switched tabs while the server was indexing — the list has to
    /// navigate the document that was asked about, not whichever is on screen when
    /// it arrives.
    symbols_request: Option<(RequestId, PathBuf)>,
    /// A fingerprint of the text last sent to the server.
    sent: Option<u64>,
    /// The completion list on screen, if one is open.
    suggest: Option<Suggest>,
    /// The completion request in flight.
    completion_request: Option<RequestId>,
    /// The formatting request in flight.
    format_request: Option<RequestId>,
    /// The rename request in flight.
    rename_request: Option<RequestId>,
    /// The code-action request in flight.
    code_action_request: Option<RequestId>,
    /// The `codeAction/resolve` in flight, for the action that was chosen.
    resolve_request: Option<RequestId>,
    /// What the server last offered, in the order it offered them.
    ///
    /// Held here rather than handed to the session, because most of an action is
    /// the server's own JSON — the edit, and the `data` a resolve is matched by.
    /// The prompt gets a title and an index; this is what that index is into.
    ///
    /// Cleared when a new list arrives and when the server goes away, so a stale
    /// index can never select an action from a question nobody asked.
    code_actions: Vec<deco_lsp::CodeAction>,
}

/// A hover being displayed.
#[derive(Debug, Clone, PartialEq)]
pub struct ShownHover {
    /// What the server said.
    pub hover: Hover,
    /// Where the cursor was when it was asked for.
    pub asked_at: Position,
}

impl ShownHover {
    /// Whether this hover still describes what is under the cursor.
    ///
    /// The server's own range when it gave one, since that is authoritative
    /// about which token it answered for. Failing that, the exact position it
    /// was asked about: guessing a wider area would keep a stale box on screen.
    pub fn applies_at(&self, position: Position) -> bool {
        match self.hover.range {
            Some(range) => {
                // Inclusive of the end, unlike a diagnostic: a hover range
                // covers an identifier, and the caret sitting just after the
                // last character is still on that identifier as far as the user
                // is concerned.
                position >= range.start && position <= range.end
            }
            None => position == self.asked_at,
        }
    }
}

/// Where the word under the cursor begins.
///
/// A cheap fingerprint of a document's text.
///
/// Only ever compared with another fingerprint of the same document, so a
/// collision would have to be between two states of one file — and the cost of
/// one is a redundant `didChange`, not a wrong result. `DefaultHasher` rather
/// than a real digest because this is a change detector, not a checksum.
fn fingerprint(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// `path` relative to `root` when it is inside it, and whole otherwise.
///
/// A references list is mostly locations in the workspace, and
/// `/home/you/work/project/src/main.rs` repeated down the column crowds out the
/// part that differs. A location outside the workspace keeps its full path,
/// because there the directory is the informative part.
fn shorten(path: &Path, root: Option<&Path>) -> String {
    root.and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The anchor a completion list filters from. A server is asked at the cursor
/// and answers about the whole word, so the editor has to agree with it about
/// where that word started — otherwise the list filters against the wrong text,
/// which looks like the server returning nonsense.
///
/// "Word" here is the identifier rule every language deco knows shares:
/// alphanumeric plus `_`. Deliberately not the server's idea of a word, because
/// the protocol gives no way to ask.
fn word_start(session: &Session) -> Position {
    let cursor = session.view.selections.primary().active;
    let Some(line) = session
        .document
        .buffer
        .line_content(cursor.line as usize)
        .map(|s| s.to_string())
    else {
        return cursor;
    };

    // Counted in UTF-16 units, since that is what a position is.
    let mut start = cursor.character;
    let units: Vec<u16> = line.encode_utf16().collect();
    while start > 0 {
        let index = (start - 1) as usize;
        let Some(&unit) = units.get(index) else { break };
        // Surrogates are part of a character outside the Basic Multilingual
        // Plane — an emoji, say — which is never an identifier character, so the
        // word ends here.
        let Some(c) = char::from_u32(unit as u32) else {
            break;
        };
        if !(c.is_alphanumeric() || c == '_') {
            break;
        }
        start -= 1;
    }
    Position::new(cursor.line, start)
}

/// The characters between two columns of a line, as typed.
///
/// Used to replay into a completion filter what the user typed while waiting for
/// the server's answer.
fn typed_between(line: &str, from: u32, to: u32) -> Vec<char> {
    let units: Vec<u16> = line.encode_utf16().collect();
    let from = from as usize;
    let to = (to as usize).min(units.len());
    if from >= to {
        return Vec::new();
    }
    String::from_utf16_lossy(&units[from..to]).chars().collect()
}

/// Where a language server runs.
///
/// The whole of remote language support is this enum and the [`PathMap`] it
/// hands out. A server is a program speaking a protocol over its stdin and
/// stdout, and every transport deco has already carries exactly that — so
/// running one on the far end is a question of which command to spawn and which
/// paths to put in the messages, and nothing about the protocol changes.
#[derive(Debug, Clone)]
pub enum Location {
    /// On this machine, reading the checkout the editor is looking at.
    Here,
    /// On the machine the session is connected to.
    Remote {
        /// How to reach it.
        authority: deco_remote::Authority,
        /// How to build the command that gets there.
        options: deco_remote::TransportOptions,
        /// The directory it serves, as *it* spells it.
        workspace: PathBuf,
    },
}

impl Location {
    /// How long to wait for a server to answer `initialize`.
    ///
    /// Longer over a transport, and not by a little: the wait covers an SSH
    /// handshake and a language server reading a project from a disk this
    /// machine never touches. Ten seconds is generous locally and would be a
    /// coin toss on a real remote.
    pub fn startup_timeout(&self) -> Duration {
        match self {
            Self::Here => STARTUP_TIMEOUT,
            Self::Remote { .. } => REMOTE_STARTUP_TIMEOUT,
        }
    }

    /// How the editor's paths relate to the ones a server there will see.
    pub fn paths(&self) -> PathMap {
        match self {
            Self::Here => PathMap::host(),
            Self::Remote { workspace, .. } => PathMap::remote(workspace.clone()),
        }
    }

    /// The definition, with its command changed into one that runs it there.
    ///
    /// Environment variables move into the argument vector as `env NAME=VALUE`
    /// rather than staying on the config, because [`deco_lsp`] sets them on the
    /// process it spawns — which over a transport is `ssh`, on this machine.
    /// They would have been set in the wrong place and silently not reached the
    /// server at all.
    pub fn resolve(&self, config: &ServerConfig) -> Result<ServerConfig, String> {
        let Self::Remote {
            authority, options, ..
        } = self
        else {
            return Ok(config.clone());
        };

        let mut argv = Vec::new();
        if !config.env.is_empty() {
            argv.push("env".to_owned());
            for (name, value) in &config.env {
                // A name with an `=` in it would split in the wrong place and
                // set a variable nobody asked for; a NUL or a newline cannot be
                // passed through an argument vector at all. Refused by name
                // rather than mangled, which is the same rule the server
                // definition itself is read under.
                if name.is_empty()
                    || name.contains('=')
                    || [name.as_str(), value.as_str()]
                        .iter()
                        .any(|text| text.contains('\0') || text.contains('\n'))
                {
                    return Err(format!(
                        "`{name}` cannot be sent to a remote server as an environment variable"
                    ));
                }
                argv.push(format!("{name}={value}"));
            }
        }
        argv.push(config.command.program.clone());
        argv.extend(config.command.args.iter().cloned());

        let command = deco_remote::command_for(authority, &argv, options)
            .map_err(|error| error.to_string())?;
        Ok(ServerConfig {
            command: deco_lsp::server::Command {
                program: command.program,
                args: command.args,
            },
            // Moved into the argument vector above, so leaving them here as well
            // would set them twice: once uselessly on this machine.
            env: Vec::new(),
            ..config.clone()
        })
    }
}

impl Lsp {
    /// Reads the configuration and prepares to attach servers here.
    pub fn new(session: &mut Session, root: Option<PathBuf>) -> Self {
        Self::with_location(session, root, Location::Here)
    }

    /// The same, saying where the servers should run.
    ///
    /// Nothing is started here: which server to run depends on the document,
    /// which may not be open yet.
    pub fn with_location(session: &mut Session, root: Option<PathBuf>, location: Location) -> Self {
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
            paths: location.paths(),
            root: match &location {
                // The root a server is told about has to be one it can see, and
                // in a remote session that is a directory on the other machine.
                Location::Remote { workspace, .. } => Some(workspace.clone()),
                Location::Here => root,
            },
            location,
            hover: None,
            hover_request: None,
            definition_request: None,
            references_request: None,
            semantic_request: None,
            symbols_request: None,
            sent: None,
            suggest: None,
            completion_request: None,
            format_request: None,
            rename_request: None,
            code_action_request: None,
            resolve_request: None,
            code_actions: Vec::new(),
        }
    }

    /// The completion list to draw, if one is open.
    pub fn suggest(&self) -> Option<&Suggest> {
        self.suggest.as_ref()
    }

    /// Asks for completions at the cursor.
    ///
    /// `trigger` records whether the user asked or a trigger character was
    /// typed; the server changes what it offers accordingly.
    pub fn request_completion(&mut self, session: &mut Session, trigger: CompletionTrigger) {
        self.dismiss_suggest();
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        let position = session.view.selections.primary().active;
        match supervisor.completion(&path, position, trigger) {
            Ok(Some(id)) => self.completion_request = Some(id),
            Ok(None) => {
                // Only worth saying when the user asked. A trigger character
                // that finds no provider should type itself and stay quiet.
                session.status = Some("this server does not offer completion".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
        self.sync_context(session);
    }

    /// The characters that should open a list without being asked.
    pub fn completion_triggers(&self) -> &[String] {
        self.supervisor
            .as_ref()
            .map(Supervisor::completion_triggers)
            .unwrap_or(&[])
    }

    /// Closes the list and cancels the request behind it.
    pub fn dismiss_suggest(&mut self) {
        self.suggest = None;
        if let (Some(id), Some(supervisor)) =
            (self.completion_request.take(), self.supervisor.as_mut())
        {
            let _ = supervisor.cancel(&id);
        }
    }

    /// Moves the selection in an open list. Returns whether anything changed.
    pub fn select_next(&mut self) -> bool {
        match self.suggest.as_mut() {
            Some(suggest) => {
                suggest.next();
                true
            }
            None => false,
        }
    }

    /// Moves the selection up in an open list.
    pub fn select_previous(&mut self) -> bool {
        match self.suggest.as_mut() {
            Some(suggest) => {
                suggest.previous();
                true
            }
            None => false,
        }
    }

    /// Narrows an open list as the user types, closing it when nothing matches.
    ///
    /// Called after the character has been inserted into the document, so the
    /// list and the text agree about what has been typed.
    pub fn typed(&mut self, session: &mut Session, c: char) -> bool {
        let Some(suggest) = self.suggest.as_mut() else {
            return false;
        };
        if !suggest.push(c) {
            self.dismiss_suggest();
            self.sync_context(session);
        }
        true
    }

    /// Widens an open list on backspace, closing it if the word is gone.
    pub fn backspaced(&mut self, session: &mut Session) -> bool {
        let Some(suggest) = self.suggest.as_mut() else {
            return false;
        };
        if !suggest.pop() || suggest.is_empty() {
            self.dismiss_suggest();
            self.sync_context(session);
        }
        true
    }

    /// Inserts the selected item, replacing what the list was matching.
    ///
    /// Returns whether anything was accepted, so the caller can fall through to
    /// the key's ordinary meaning — `enter` with no list open has to insert a
    /// newline.
    pub fn accept(&mut self, session: &mut Session, now_ms: u64) -> bool {
        let Some(suggest) = self.suggest.as_ref() else {
            return false;
        };
        let Some(item) = suggest.selected_item().cloned() else {
            self.dismiss_suggest();
            self.sync_context(session);
            return false;
        };

        // The server's own range when it gave one: it knows where the completion
        // begins, and guessing from the document is how `Hash` + `HashMap`
        // becomes `HashHashMap`. Failing that, the span from where the list
        // opened to the cursor, which is exactly what was matched against.
        let cursor = session.view.selections.primary().active;
        let range = item.replace.unwrap_or(deco_core::position::Range::ordered(
            suggest.anchor(),
            cursor,
        ));

        self.dismiss_suggest();
        if item.was_snippet {
            // Said plainly rather than silently inserting the reduced text: the
            // user asked for a snippet and got a best effort.
            session.status = Some(format!(
                "{}: inserted without placeholders (no snippet support yet)",
                item.label
            ));
        }
        session.replace_range(range, &item.insert, now_ms);
        self.sync_context(session);
        true
    }

    /// The hover to draw, if one applies to where the cursor is now.
    pub fn hover(&self) -> Option<&Hover> {
        self.hover.as_ref().map(|shown| &shown.hover)
    }

    /// Republishes every context key derived from the server's state.
    ///
    /// One function called from every path that changes that state, rather than
    /// each site remembering which keys it invalidated. A stale `when` key is a
    /// keybinding that silently does the wrong thing — F12 dead while a server
    /// offers definitions, or escape swallowed with no hover on screen — and
    /// that is the hardest kind of bug to notice.
    fn sync_context(&self, session: &mut Session) {
        let capabilities = self.supervisor.as_ref().map(Supervisor::capabilities);
        let has =
            |f: fn(&deco_lsp::ServerCapabilities) -> bool| capabilities.map(f).unwrap_or(false);

        // VS Code's own names, so a `when` clause copied out of somebody's
        // keybindings.json gates on the same thing here.
        session
            .context
            .set("editorHasHoverProvider", has(|c| c.hover));
        session
            .context
            .set("editorHasDefinitionProvider", has(|c| c.definition));
        session
            .context
            .set("editorHasReferenceProvider", has(|c| c.references));
        session.context.set(
            "editorHasDocumentSymbolProvider",
            has(|c| c.document_symbol),
        );
        session
            .context
            .set("editorHasRenameProvider", has(|c| c.rename.is_some()));
        session
            .context
            .set("editorHasDocumentFormattingProvider", has(|c| c.formatting));
        session.context.set(
            "editorHasCodeActionsProvider",
            has(|c| c.code_action.is_some()),
        );

        // Gates escape.
        session
            .context
            .set("editorHoverVisible", self.hover.is_some());
        // VS Code's key, and already referenced by the default keymap: `enter`
        // and `tab` are bound with `!suggestWidgetVisible` so they keep their
        // ordinary meaning while no list is open.
        session
            .context
            .set("suggestWidgetVisible", self.suggest.is_some());
    }

    /// Requests a hover for the cursor's position.
    ///
    /// Any hover already on screen is dismissed first: it described the previous
    /// position, and leaving it up while a new one is fetched shows the user an
    /// answer to a question they are no longer asking.
    pub fn request_hover(&mut self, session: &mut Session) {
        self.dismiss_hover();
        self.sync_context(session);
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        let position = session.view.selections.primary().active;
        match supervisor.hover(&path, position) {
            Ok(Some(id)) => self.hover_request = Some(id),
            Ok(None) => {
                session.status = Some("this server does not offer hover".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Asks the server to format the document, or the selection if there is one.
    ///
    /// The selection decides which method is used, rather than a separate
    /// keybinding choosing wrongly: `ctrl+shift+i` with text selected almost
    /// always means "format this", and reformatting the whole file instead is a
    /// diff nobody asked for.
    pub fn request_formatting(&mut self, session: &mut Session, selection_only: bool) {
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        let options = session.formatting_options();
        let selection = session.view.selections.primary();
        let range = deco_core::position::Range::ordered(selection.anchor, selection.active);

        let raised = if selection_only && !range.is_empty() {
            supervisor.range_formatting(&path, range, options)
        } else {
            supervisor.formatting(&path, options)
        };

        match raised {
            Ok(Some(id)) => {
                self.format_request = Some(id);
                // Said up front: formatting a large file can take a moment, and
                // silence looks like a key that does nothing.
                session.status = Some("Formatting…".to_owned());
            }
            Ok(None) => {
                session.status = Some("this server does not offer formatting".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Asks what the server offers to do about the selection.
    pub fn request_code_actions(&mut self, session: &mut Session) {
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            session.status = Some("no language server for this file".to_owned());
            return;
        };
        // The selection, or the caret when there is none — which is how VS Code
        // asks, and why a quick fix works without selecting the error first.
        let selection = session.view.selections.primary();
        let range = deco_core::position::Range::ordered(selection.anchor, selection.active);

        match supervisor.code_action(&path, range) {
            Ok(Some(id)) => {
                self.code_action_request = Some(id);
                session.status = Some("Looking for code actions…".to_owned());
            }
            Ok(None) => {
                session.status = Some("this server does not offer code actions".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Puts what the server offered into the picker.
    fn offer_code_actions(&mut self, session: &mut Session, actions: Vec<deco_lsp::CodeAction>) {
        let entries = actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let entry =
                    deco_editor::commands::PaletteEntry::new(&index.to_string(), &action.title);
                // The second column says either why it cannot be run or what
                // kind of thing it is — in that order, because a disabled
                // action's reason is the only thing about it worth reading.
                match (&action.disabled, action.short_kind()) {
                    (Some(reason), _) => entry.with_detail(&format!("unavailable — {reason}")),
                    (None, Some(kind)) => entry.with_detail(kind),
                    (None, None) => entry,
                }
            })
            .collect();
        self.code_actions = actions;
        session.offer_code_actions(entries);
    }

    /// Carries out the action the user chose from the picker.
    ///
    /// `id` is the index the picker was given. An action that already has its
    /// edit is applied here; one that does not goes back to the server first and
    /// is applied when the resolved answer arrives.
    pub fn run_code_action(
        &mut self,
        session: &mut Session,
        id: &str,
        files: &mut crate::extensions::Files<'_>,
    ) {
        let Some(action) = id
            .parse::<usize>()
            .ok()
            .and_then(|index| self.code_actions.get(index))
            .cloned()
        else {
            // The list was replaced or dropped between the prompt opening and
            // this arriving. Nothing to do, and nothing worth alarming anyone
            // about.
            return;
        };

        if let Some(reason) = &action.disabled {
            session.status = Some(format!("`{}` is unavailable: {reason}", action.title));
            return;
        }

        if action.needs_resolving() {
            match self
                .supervisor
                .as_mut()
                .map(|s| s.resolve_code_action(&action))
            {
                Some(Ok(Some(id))) => {
                    self.resolve_request = Some(id);
                    session.status = Some(format!("{}…", action.title));
                }
                // The server offered an action with no edit and no way to ask
                // for one. Naming it is the only honest answer: there is nothing
                // here to apply and never was.
                Some(Ok(None)) | None => {
                    session.status = Some(format!(
                        "`{}` came with no edit, and this server cannot resolve one",
                        action.title
                    ));
                }
                Some(Err(error)) => self.report(session, error.to_string()),
            }
            return;
        }

        self.apply_code_action(session, &action, files);
    }

    /// Applies a resolved action's edit, or says why there is nothing to apply.
    fn apply_code_action(
        &mut self,
        session: &mut Session,
        action: &deco_lsp::CodeAction,
        files: &mut crate::extensions::Files<'_>,
    ) {
        let Some(edit) = &action.edit else {
            // Either the resolve came back with nothing, or the action was only
            // ever a `Command`. Running a server command is `workspace/executeCommand`,
            // whose result comes back as a `workspace/applyEdit` *request* — a
            // different direction of authority, and not wired here.
            let detail = match &action.command {
                Some(command) => {
                    format!("it runs the server command `{command}`, which deco cannot")
                }
                None => "the server sent no edit for it".to_owned(),
            };
            session.status = Some(format!("`{}` was not applied: {detail}", action.title));
            return;
        };

        let edit = match deco_lsp::WorkspaceEdit::from_json(edit) {
            Ok(edit) => edit,
            // A file created, renamed or deleted. Refused for one action rather
            // than for the whole menu, which is why the edit is parsed here and
            // not while listing.
            Err(error) => {
                self.report(session, format!("`{}`: {error}", action.title));
                return;
            }
        };
        if edit.is_empty() {
            session.status = Some(format!("`{}` changes nothing", action.title));
            return;
        }

        let Some(supervisor) = self.supervisor.as_ref() else {
            return;
        };
        let paths = supervisor.paths();
        let planned = session
            .plan_workspace_edit(
                &edit,
                |uri| paths.from_uri(uri).ok(),
                |path| supervisor.version_of(path),
            )
            .and_then(|plan| {
                // Through `files` rather than `std::fs`: in a remote session the
                // language server runs where the files are, so the paths in its
                // answer are paths over there, and reading them here would find
                // either nothing or the wrong file.
                plan.with_contents(|path| files.read(&path.display().to_string()))
            });

        let plan = match planned {
            Ok(plan) => plan,
            Err(error) => {
                self.report(session, error.to_string());
                return;
            }
        };

        match session.apply_workspace_edit(plan, 0) {
            Ok(applied) => {
                session.status = Some(applied.summary(&action.title));
                // The answer arrived through the poll rather than through a
                // keypress, so the loop's own sync has already run this turn.
                self.changed(session);
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Opens the rename prompt, if this server can rename at all.
    ///
    /// Checked before asking rather than after: a prompt that appears, takes a
    /// name, and only then reports that renaming is not on offer has wasted the
    /// one thing it asked for.
    pub fn offer_rename(&mut self, session: &mut Session) {
        let Some(supervisor) = self.supervisor.as_ref() else {
            session.status = Some("no language server for this file".to_owned());
            return;
        };
        if supervisor.capabilities().rename.is_none() {
            session.status = Some("this server does not offer rename".to_owned());
            return;
        }
        session.offer_rename();
    }

    /// Asks the server what renaming the symbol under the cursor would change.
    pub fn request_rename(&mut self, session: &mut Session, new_name: &str) {
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        let position = session.view.selections.primary().active;
        match supervisor.rename(&path, position, new_name) {
            Ok(Some(id)) => {
                self.rename_request = Some(id);
                // A rename reads every file in the project on some servers, so
                // this is the one request where silence is long enough to look
                // like a key that did nothing.
                session.status = Some(format!("Renaming to `{new_name}`…"));
            }
            Ok(None) => {
                session.status = Some("this server does not offer rename".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Applies what the server said a rename would change.
    ///
    /// The reading of files is here because this is the layer with a filesystem;
    /// every decision about *whether* to apply is in `deco_editor::workspace`,
    /// which is where it can be tested without one.
    fn apply_rename(
        &mut self,
        session: &mut Session,
        edit: deco_lsp::WorkspaceEdit,
        files: &mut crate::extensions::Files<'_>,
    ) {
        if edit.is_empty() {
            session.status = Some("nothing to rename here".to_owned());
            return;
        }
        let Some(supervisor) = self.supervisor.as_ref() else {
            // The server went away between asking and answering.
            return;
        };
        let paths = supervisor.paths();

        let planned = session
            .plan_workspace_edit(
                &edit,
                |uri| paths.from_uri(uri).ok(),
                |path| supervisor.version_of(path),
            )
            .and_then(|plan| {
                // Through `files` rather than `std::fs`: in a remote session the
                // language server runs where the files are, so the paths in its
                // answer are paths over there, and reading them here would find
                // either nothing or the wrong file.
                plan.with_contents(|path| files.read(&path.display().to_string()))
            });

        let plan = match planned {
            Ok(plan) => plan,
            Err(error) => {
                self.report(session, error.to_string());
                return;
            }
        };

        match session.apply_workspace_edit(plan, 0) {
            Ok(applied) => {
                session.status = Some(applied.summary("Renamed"));
                // This answer arrived through the poll rather than through a
                // keypress, so the loop's own "tell the server what the text is
                // now" has already run for this turn. Without this the server
                // would keep answering about the old name until the next
                // keystroke — including about the rename that just happened.
                self.changed(session);
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Asks the server to classify the whole document.
    ///
    /// Skipped while an answer is outstanding: a request per keystroke would queue
    /// classifications of text that has already changed, and the newest answer is
    /// the only one worth having. The lexer's colouring stands in the meantime,
    /// which is why the delay is not visible as an absence of colour.
    pub fn request_semantic_tokens(&mut self, session: &mut Session) {
        if self.semantic_request.is_some() {
            return;
        }
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        // Silent: this is a refinement nobody asked for by pressing a key, so a
        // status line about it would be noise, and a server that does not offer it
        // is not a problem to report.
        if let Ok(Some(id)) = supervisor.semantic_tokens(&path) {
            self.semantic_request = Some(id);
        }
    }

    /// Requests everything that refers to whatever is under the cursor.
    pub fn request_references(&mut self, session: &mut Session) {
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        let position = session.view.selections.primary().active;
        match supervisor.references(&path, position) {
            Ok(Some(id)) => {
                self.references_request = Some(id);
                session.status = Some("Looking for references…".to_owned());
            }
            Ok(None) => {
                session.status = Some("this server does not offer references".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Requests the names this document declares, for the go-to-symbol prompt.
    pub fn request_document_symbols(&mut self, session: &mut Session) {
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        match supervisor.document_symbols(&path) {
            Ok(Some(id)) => {
                self.symbols_request = Some((id, path));
                session.status = Some("Looking for symbols…".to_owned());
            }
            Ok(None) => {
                session.status = Some("this server does not offer document symbols".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Requests the definition of whatever is under the cursor.
    pub fn request_definition(&mut self, session: &mut Session) {
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        let position = session.view.selections.primary().active;
        match supervisor.definition(&path, position) {
            Ok(Some(id)) => {
                self.definition_request = Some(id);
                // Said before the answer arrives, because a server that has to
                // index first can take seconds and silence looks like a
                // keybinding that does nothing.
                session.status = Some("Looking for the definition…".to_owned());
            }
            Ok(None) => {
                session.status = Some("this server does not offer go-to-definition".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Drops the hover on screen, and cancels the request behind it.
    pub fn dismiss_hover(&mut self) {
        self.hover = None;
        if let (Some(id), Some(supervisor)) = (self.hover_request.take(), self.supervisor.as_mut())
        {
            // Advisory, and the answer is dropped when it arrives — but a hover
            // the user has moved past is work the server can stop doing.
            let _ = supervisor.cancel(&id);
        }
    }

    /// Dismisses the hover if the cursor has moved off what it describes.
    ///
    /// Returns whether anything changed, so the caller can skip a repaint.
    pub fn cursor_moved(&mut self, session: &mut Session) -> bool {
        let position = session.view.selections.primary().active;
        if self
            .hover
            .as_ref()
            .is_some_and(|shown| !shown.applies_at(position))
        {
            self.hover = None;
            self.sync_context(session);
            return true;
        }
        false
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

        // Workspace-defined servers are set aside rather than tried. Setting them
        // aside rather than stopping at the first one is what keeps a workspace
        // from disabling a language: a repository that defines its own server
        // gets that definition declined, and the user's own server still starts.
        //
        // They are also named here, before anything is started, rather than as a
        // loop walks past them. A loop leaves the moment a server starts or fails
        // to, so reporting afterwards meant the disclosure was reached only when
        // every candidate had been refused — and the ordinary case, a user who
        // has a server of their own, was told nothing at all.
        let (refused, trusted): (Vec<_>, Vec<_>) = candidates
            .iter()
            .partition(|config| config.trust == Trust::Workspace);
        let refused: Vec<&str> = refused.iter().map(|config| config.id.as_str()).collect();
        if !refused.is_empty() {
            // The problem list rather than the status bar: `attach` runs on every
            // tab switch and language change, and a row that reappeared each time
            // would push aside whatever else the editor had to say. The list is
            // printed once before the frontend starts and shown by
            // `--print-config`, which is where a user goes to ask why a server is
            // not running.
            let problem = format!(
                "{} defined by this workspace and not started; move the definition into your own settings to run it",
                refused.join(", ")
            );
            if !session.problems.contains(&problem) {
                session.problems.push(problem);
            }
        }

        // The best candidate deco is willing to run, and only that one: the list
        // is in preference order, so a definition that fails to start is a
        // broken configuration to report rather than a reason to quietly run a
        // different server than the one asked for.
        if let Some(config) = trusted.first() {
            // Rewritten to run wherever this session's servers run, which for a
            // remote session means the same definition wrapped in the transport.
            let config = match self.location.resolve(config) {
                Ok(config) => config,
                Err(problem) => {
                    session.status = Some(format!("{}: {problem}", config.id));
                    session.problems.push(problem);
                    return;
                }
            };
            match Supervisor::start(
                &config,
                Consent::Granted,
                self.root.as_deref(),
                self.paths.clone(),
                self.location.startup_timeout(),
            ) {
                Ok(supervisor) => {
                    self.supervisor = Some(supervisor);
                    self.language = Some(language.clone());
                    self.sync_open(session, &path, &language);
                    self.sync_context(session);
                }
                Err(error) => {
                    // First line only in the status bar: a startup failure
                    // carries the whole stderr tail, which is invaluable in a
                    // log and unreadable in a single row.
                    let summary = error.to_string();
                    let first = summary.lines().next().unwrap_or("failed to start");
                    session.status = Some(format!("{}: {first}", config.id));
                    session.problems.push(summary);
                }
            }
            return;
        }

        // Nothing started, so the status bar is free to say why — and with no
        // server for this language there is nothing else competing for the row.
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
            Ok(()) => {
                self.open = Some(path.to_owned());
                self.sent = Some(fingerprint(&text));
                self.request_semantic_tokens(session);
            }
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

        // The event loop calls this after every keypress, most of which move the
        // cursor without touching the text. A fingerprint tells the two apart, so
        // an arrow key no longer sends the whole document — and, more visibly, no
        // longer throws away a classification that is still correct.
        let fingerprint = fingerprint(&text);
        if self.sent == Some(fingerprint) {
            return;
        }
        self.sent = Some(fingerprint);

        // The old classification described the text before this edit. Dropped
        // rather than kept until the answer arrives: a token list applied to
        // shifted text colours the wrong words, which is worse than the lexer's
        // colouring alone for the moment it takes to answer.
        session.semantic_tokens.clear();

        if let Err(error) = supervisor.did_change(&path, &[], &text) {
            self.report(session, error.to_string());
            return;
        }
        self.request_semantic_tokens(session);
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
    pub fn poll(
        &mut self,
        session: &mut Session,
        files: &mut crate::extensions::Files<'_>,
    ) -> bool {
        let Some(supervisor) = self.supervisor.as_mut() else {
            return false;
        };

        let updates = supervisor.poll();
        if updates.is_empty() {
            return false;
        }
        self.absorb(session, updates, files)
    }

    /// Applies updates already drained from a server.
    ///
    /// Split from [`Lsp::poll`] so that what an answer *does* can be asserted
    /// without a language server installed: the alternative is a test suite that
    /// passes or fails depending on what happens to be on the machine.
    fn absorb(
        &mut self,
        session: &mut Session,
        updates: Vec<Update>,
        files: &mut crate::extensions::Files<'_>,
    ) -> bool {
        let mut changed = false;
        // Collected first so `self` is free of the supervisor borrow.
        let open_uri = self.supervisor.as_ref().and_then(|supervisor| {
            session
                .document
                .path
                .as_deref()
                .and_then(|path| supervisor.uri_for(path))
        });

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
                    self.hover = None;
                    self.hover_request = None;
                    self.definition_request = None;
                    self.references_request = None;
                    self.semantic_request = None;
                    self.suggest = None;
                    self.completion_request = None;
                    self.format_request = None;
                    self.rename_request = None;
                    self.code_action_request = None;
                    self.resolve_request = None;
                    self.code_actions.clear();
                    // The features that were on offer went with the server.
                    self.sync_context(session);
                    return true;
                }
                Update::Hover { id, hover } => {
                    // Only the answer to the request still outstanding. An
                    // earlier one arriving late describes a position the user
                    // has left.
                    if self.hover_request.as_ref() != Some(&id) {
                        continue;
                    }
                    self.hover_request = None;
                    match hover {
                        Some(hover) => {
                            self.hover = Some(ShownHover {
                                hover,
                                asked_at: session.view.selections.primary().active,
                            });
                        }
                        // The server answered, and the answer is "nothing".
                        // Saying so beats a keypress that appears to do nothing.
                        None => session.status = Some("no information here".to_owned()),
                    }
                    self.sync_context(session);
                    changed = true;
                }
                Update::Locations {
                    id,
                    method,
                    locations,
                } => {
                    if self.references_request.as_ref() == Some(&id) {
                        self.references_request = None;
                        self.semantic_request = None;
                        self.offer_locations(session, &locations, files);
                        changed = true;
                        continue;
                    }
                    if self.definition_request.as_ref() != Some(&id) {
                        continue;
                    }
                    self.definition_request = None;
                    self.references_request = None;
                    self.semantic_request = None;
                    changed |= self.go_to(session, &method, &locations, files);
                }
                Update::Symbols { id, symbols } => {
                    // Only the outstanding request: an answer to a superseded one
                    // describes a file the prompt is no longer about.
                    let Some((asked, path)) = self.symbols_request.take() else {
                        continue;
                    };
                    if asked != id {
                        self.symbols_request = Some((asked, path));
                        continue;
                    }
                    self.offer_symbols(session, &path, &symbols);
                    changed = true;
                }
                Update::SemanticTokens { id, spans } => {
                    // Only the outstanding request: an earlier classification
                    // describes text the user has since edited.
                    if self.semantic_request.as_ref() != Some(&id) {
                        continue;
                    }
                    self.semantic_request = None;
                    session.semantic_tokens = spans;
                    changed = true;
                }
                Update::Completion {
                    id,
                    items,
                    incomplete,
                } => {
                    // Only the outstanding request: an earlier list arriving late
                    // describes a position the user has typed past.
                    if self.completion_request.as_ref() != Some(&id) {
                        continue;
                    }
                    self.completion_request = None;
                    if items.is_empty() {
                        session.status = Some("no completions here".to_owned());
                    } else {
                        let anchor = word_start(session);
                        let mut suggest = Suggest::new(items, anchor, incomplete);
                        // The characters between the word's start and the cursor
                        // were typed before the answer came back, so they are
                        // replayed into the filter. Without this the list shows
                        // everything the server offered at the word's start,
                        // ignoring what the user has since narrowed it to.
                        let cursor = session.view.selections.primary().active;
                        if anchor.line == cursor.line && cursor.character > anchor.character {
                            let line = session
                                .document
                                .buffer
                                .line_content(cursor.line as usize)
                                .map(|s| s.to_string())
                                .unwrap_or_default();
                            for c in typed_between(&line, anchor.character, cursor.character) {
                                if !suggest.push(c) {
                                    break;
                                }
                            }
                        }
                        if suggest.is_empty() {
                            session.status = Some("no completions here".to_owned());
                        } else {
                            self.suggest = Some(suggest);
                        }
                    }
                    self.sync_context(session);
                    changed = true;
                }
                Update::Edits { id, method, edits } => {
                    if self.format_request.as_ref() != Some(&id) {
                        continue;
                    }
                    self.format_request = None;
                    let short = method.rsplit('/').next().unwrap_or(&method).to_owned();
                    match session.apply_edits(&edits, 0) {
                        Ok(0) => {
                            session.status = Some("already formatted".to_owned());
                        }
                        Ok(count) => {
                            session.status = Some(format!(
                                "{short}: applied {count} edit{}",
                                if count == 1 { "" } else { "s" }
                            ));
                        }
                        // A broken server, and the file is untouched. Worth
                        // saying loudly: the user pressed a key and nothing
                        // happened, and the reason is not their fault.
                        Err(error) => {
                            session.status = Some(format!("{short}: {error}"));
                            session.problems.push(format!("{method}: {error}"));
                        }
                    }
                    changed = true;
                }
                Update::CodeActions { id, actions } => {
                    if self.code_action_request.as_ref() != Some(&id) {
                        continue;
                    }
                    self.code_action_request = None;
                    self.offer_code_actions(session, actions);
                    changed = true;
                }
                Update::CodeActionResolved { id, action } => {
                    if self.resolve_request.as_ref() != Some(&id) {
                        continue;
                    }
                    self.resolve_request = None;
                    self.apply_code_action(session, &action, files);
                    changed = true;
                }
                Update::Renamed { id, edit } => {
                    if self.rename_request.as_ref() != Some(&id) {
                        continue;
                    }
                    self.rename_request = None;
                    match edit {
                        Ok(edit) => self.apply_rename(session, edit, files),
                        // A server asking for something deco cannot do — a file
                        // created, renamed or deleted. Nothing was changed, and
                        // the message says which operation it was.
                        Err(error) => self.report(session, error.to_string()),
                    }
                    changed = true;
                }
                Update::RequestFailed { method, reason, .. } => {
                    // The short name: `textDocument/hover` in a status bar is
                    // mostly punctuation.
                    let short = method.rsplit('/').next().unwrap_or(&method);
                    session.status = Some(format!("{short}: {reason}"));
                    session.problems.push(format!("{method}: {reason}"));
                    changed = true;
                }
                Update::Ready { .. } | Update::Noted { .. } => {}
            }
        }
        changed
    }

    /// Moves the cursor to the first of `locations`, opening the file if needed.
    ///
    /// Returns whether the screen changed.
    fn go_to(
        &mut self,
        session: &mut Session,
        method: &str,
        locations: &[deco_lsp::Location],
        files: &mut crate::extensions::Files<'_>,
    ) -> bool {
        let Some(target) = locations.first() else {
            // A successful answer meaning the server found nothing. Reporting it
            // is the difference between "no definition" and "the editor is
            // broken".
            session.status = Some("no definition found".to_owned());
            return true;
        };

        // Several answers is a question, not a result: offer them rather than
        // picking one. The same list references uses, for the same reason.
        if locations.len() > 1 {
            self.offer_locations(session, locations, files);
            return true;
        }

        let Ok(path) = self.paths.from_uri(&target.uri) else {
            // `jdt:`, `untitled:` and friends. The editor cannot open one, and
            // pretending otherwise would create an empty buffer named after a
            // URI.
            session.status = Some(format!("cannot open {}", target.uri));
            return true;
        };

        let same_file = session.document.path.as_deref() == Some(path.as_path());
        if !same_file {
            // Into a new tab (or the tab already holding that file), so unsaved
            // work in the current document is not at risk and no longer needs to
            // block the jump.
            // Through `files`: a server running on the far end of a remote
            // session points at files over there, and reading the path here
            // would open either nothing or an unrelated local file.
            match files.read(&path.display().to_string()) {
                Ok(text) => {
                    session.open(path.clone(), &text);
                    // The new document needs its own server, and the old one
                    // needs telling that the previous file is closed.
                    self.attach(session);
                    self.refresh_diagnostics(session);
                }
                Err(error) => {
                    session.status = Some(format!("could not open {}: {error}", path.display()));
                    return true;
                }
            }
        }

        let clamped = session.document.buffer.clamp_position(target.range.start);
        session.view.selections = deco_core::SelectionSet::caret(clamped);
        session
            .view
            .reveal_cursor(&session.document.buffer, &session.document.settings);
        session.refresh_context();
        self.hover = None;
        self.sync_context(session);

        if locations.len() == 1 {
            let short = method.rsplit('/').next().unwrap_or(method);
            session.status = Some(format!("{short}: line {}", clamped.line + 1));
        }
        true
    }

    /// Offers `locations` as a list to pick from.
    ///
    /// The same prompt a project-wide search uses, because the two are the same
    /// question — which of these places do you want to be? — and a second list
    /// widget that behaved almost identically would be a second place for it to
    /// behave slightly differently.
    fn offer_locations(
        &mut self,
        session: &mut Session,
        locations: &[deco_lsp::Location],
        files: &mut crate::extensions::Files<'_>,
    ) {
        let paths = self.paths.clone();
        // The line's text is what makes a list of locations readable, and for a
        // location in a file that is not on screen it has to be read from disk.
        // One cache per response, because a server answering "find all references"
        // routinely returns twenty locations in the same file.
        let mut cache: std::collections::HashMap<PathBuf, Vec<String>> =
            std::collections::HashMap::new();
        let mut entries = Vec::new();

        for location in locations {
            let Ok(path) = paths.from_uri(&location.uri) else {
                // `jdt:` and friends: nothing here can open one, and an entry that
                // cannot be opened is worse than one that is missing.
                continue;
            };
            let lines = cache.entry(path.clone()).or_insert_with(|| {
                // The open document's own text rather than what is on disk, since
                // the two differ exactly when there are unsaved changes.
                if session.document.path.as_deref() == Some(path.as_path()) {
                    session
                        .document
                        .buffer
                        .text()
                        .lines()
                        .map(str::to_owned)
                        .collect()
                } else {
                    // Same machine as the file itself; see `go_to`.
                    files
                        .read(&path.display().to_string())
                        .map(|text| text.lines().map(str::to_owned).collect())
                        .unwrap_or_default()
                }
            });
            let line = location.range.start.line as usize;
            let text = lines.get(line).map(|line| line.trim()).unwrap_or("");
            let shown = shorten(&path, self.root.as_deref());
            entries.push(deco_editor::commands::PaletteEntry::at(
                &path.to_string_lossy(),
                &format!("{shown}:{}: {text}", line + 1),
                location.range.start,
            ));
        }

        if entries.is_empty() {
            session.status = Some("no locations found".to_owned());
            return;
        }
        let count = entries.len();
        session.offer_search_results("locations", entries);
        session.status = Some(format!(
            "{count} {}",
            if count == 1 { "location" } else { "locations" }
        ));
    }

    /// Opens the go-to-symbol prompt over what the server found.
    ///
    /// Every entry carries `path`, so accepting one goes through the same
    /// open-a-file-at-a-position path a search result does. For the document
    /// already on screen that is a tab switch onto itself, which keeps unsaved
    /// changes; for one the user has since navigated away from it comes back.
    fn offer_symbols(
        &mut self,
        session: &mut Session,
        path: &Path,
        symbols: &[deco_lsp::requests::DocumentSymbol],
    ) {
        let id = path.to_string_lossy().into_owned();
        let entries: Vec<deco_editor::commands::PaletteEntry> = symbols
            .iter()
            .map(|symbol| {
                let entry = deco_editor::commands::PaletteEntry::at(
                    &id,
                    &symbol.qualified(),
                    symbol.position,
                );
                // The kind in the second column, because it is what tells a field
                // from a method of the same name.
                match symbol.kind {
                    Some(kind) => entry.with_detail(kind),
                    None => entry,
                }
            })
            .collect();

        let count = entries.len();
        session.offer_symbols(entries);
        if count > 0 {
            session.status = Some(format!(
                "{count} {}",
                if count == 1 { "symbol" } else { "symbols" }
            ));
        }
    }

    /// Reloads the active document's diagnostics from the store.
    ///
    /// For a tab switch: the server has been publishing for every file it knows
    /// about all along, but only the on-screen document's publications reach the
    /// session — so a document coming back from the background has to collect
    /// what it missed.
    pub fn refresh_diagnostics(&self, session: &mut Session) {
        let Some(supervisor) = self.supervisor.as_ref() else {
            return;
        };
        let Some(uri) = session
            .document
            .path
            .as_deref()
            .and_then(|path| supervisor.uri_for(path))
        else {
            return;
        };
        session.set_diagnostics(supervisor.diagnostics(&uri).to_vec());
    }

    /// Stops the server, if one is running.
    pub fn detach(&mut self) {
        if let Some(mut supervisor) = self.supervisor.take() {
            supervisor.stop();
        }
        self.language = None;
        self.open = None;
        self.hover = None;
        self.hover_request = None;
        self.definition_request = None;
        self.references_request = None;
        self.semantic_request = None;
        self.symbols_request = None;
        self.suggest = None;
        self.completion_request = None;
        self.format_request = None;
        self.rename_request = None;
        self.code_action_request = None;
        self.resolve_request = None;
        self.code_actions.clear();
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
    use serde_json::json;

    /// A server definition to be rewritten.
    fn definition(env: Vec<(String, String)>) -> ServerConfig {
        ServerConfig {
            id: "toml-lsp".to_owned(),
            language_ids: vec!["toml".to_owned()],
            command: deco_lsp::server::Command {
                program: "taplo".to_owned(),
                args: vec!["lsp".to_owned(), "stdio".to_owned()],
            },
            env,
            initialization_options: None,
            trust: Trust::User,
        }
    }

    fn remote() -> Location {
        Location::Remote {
            authority: deco_remote::Authority::parse("ssh-remote+myhost").expect("an authority"),
            options: deco_remote::TransportOptions::default(),
            workspace: PathBuf::from("/home/u/project"),
        }
    }

    #[test]
    fn a_local_session_runs_the_definition_exactly_as_written() {
        let config = definition(vec![("RUST_LOG".to_owned(), "debug".to_owned())]);
        let resolved = Location::Here.resolve(&config).expect("no rewriting");
        assert_eq!(resolved.command, config.command);
        assert_eq!(resolved.env, config.env);
    }

    #[test]
    fn a_remote_session_runs_the_same_definition_over_the_transport() {
        let resolved = remote()
            .resolve(&definition(Vec::new()))
            .expect("a command");
        assert_eq!(resolved.command.program, "ssh");
        // The server's own command survives as the tail, as separate arguments:
        // the transport never assembles a shell string.
        let tail = &resolved.command.args[resolved.command.args.len() - 3..];
        assert_eq!(tail, ["taplo", "lsp", "stdio"]);
        assert!(resolved.command.args.contains(&"myhost".to_owned()));
        // Everything else about the definition is untouched — it is the same
        // server, started somewhere else.
        assert_eq!(resolved.id, "toml-lsp");
        assert_eq!(resolved.language_ids, ["toml"]);
    }

    #[test]
    fn environment_variables_travel_to_the_far_end_rather_than_being_set_here() {
        // `deco-lsp` sets `env` on the process it spawns, which over a transport
        // is `ssh` on this machine. Left there they would be set in the wrong
        // place and never reach the server, which is the kind of failure that
        // looks like the setting being ignored.
        let config = definition(vec![
            ("RUST_LOG".to_owned(), "debug".to_owned()),
            ("PATH_EXTRA".to_owned(), "/opt/bin".to_owned()),
        ]);
        let resolved = remote().resolve(&config).expect("a command");
        assert!(resolved.env.is_empty(), "{:?}", resolved.env);

        let args = resolved.command.args.join(" ");
        assert!(
            args.contains("env RUST_LOG=debug PATH_EXTRA=/opt/bin taplo lsp stdio"),
            "{args}"
        );
    }

    #[test]
    fn an_environment_variable_that_cannot_be_sent_is_refused_by_name() {
        // `NAME=VALUE` splits at the first `=`, so a name containing one would
        // set a variable nobody asked for.
        let config = definition(vec![("A=B".to_owned(), "c".to_owned())]);
        let error = remote().resolve(&config).expect_err("a refusal");
        assert!(error.contains("A=B"), "{error}");

        // And a newline cannot be carried in an argument vector at all.
        let config = definition(vec![("A".to_owned(), "one\ntwo".to_owned())]);
        assert!(remote().resolve(&config).is_err());
    }

    #[test]
    fn a_remote_session_maps_paths_through_the_far_ends_workspace() {
        let paths = remote().paths();
        assert_eq!(
            paths
                .to_uri(Path::new("src/main.rs"))
                .expect("a uri")
                .as_str(),
            "file:///home/u/project/src/main.rs"
        );
        assert!(Location::Here
            .paths()
            .to_uri(Path::new("src/main.rs"))
            .is_err());
    }

    #[test]
    fn a_remote_server_is_given_longer_to_start() {
        // The wait covers an SSH handshake and a server reading a project from a
        // disk this machine never touches.
        assert!(remote().startup_timeout() > Location::Here.startup_timeout());
    }

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
        // Nothing else claims the language, so the status bar is free to carry it.
        let status = s.status.expect("the refusal must be visible");
        assert!(status.contains("theirs"), "{status}");
        assert!(status.contains("workspace"), "{status}");
        // And the problem list has it either way, which is where it stays once
        // the status bar has moved on.
        assert!(
            s.problems.iter().any(|problem| problem.contains("theirs")),
            "{:?}",
            s.problems
        );
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
        assert!(!lsp.poll(&mut s, &mut here()));
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
        // And `theirs` is still named, even though the loop left before reaching
        // the end. The refusal is the user's to act on: they cannot decide to
        // move the definition into their own settings without being told it was
        // declined.
        assert!(
            s.problems.iter().any(|problem| problem.contains("theirs")),
            "{:?}",
            s.problems
        );
    }

    #[test]
    fn a_refusal_is_recorded_once_however_often_attach_runs() {
        // `attach` runs on every tab switch and language change, so a disclosure
        // that appended each time would fill the problem list with copies.
        let mut s = session(settings_with(
            Scope::Workspace,
            r#"{"deco.lsp.servers": {"theirs": {"languages": ["toml"], "command": "./taplo"}}}"#,
        ));
        let mut lsp = Lsp::new(&mut s, None);
        for _ in 0..3 {
            lsp.attach(&mut s);
        }
        assert_eq!(
            s.problems
                .iter()
                .filter(|problem| problem.contains("theirs"))
                .count(),
            1,
            "{:?}",
            s.problems
        );
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

    fn hover_of(contents: &str, range: Option<deco_core::position::Range>) -> Hover {
        Hover {
            contents: contents.to_owned(),
            range,
        }
    }

    #[test]
    fn a_hover_survives_the_cursor_staying_inside_the_range_it_describes() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.hover = Some(ShownHover {
            hover: hover_of(
                "fn main()",
                Some(deco_core::position::Range::new(
                    Position::new(0, 2),
                    Position::new(0, 6),
                )),
            ),
            asked_at: Position::new(0, 3),
        });

        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 5));
        assert!(!lsp.cursor_moved(&mut s), "still inside the range");
        assert!(lsp.hover().is_some());
    }

    #[test]
    fn a_hover_is_dismissed_when_the_cursor_leaves_its_range() {
        // A box describing a different token than the one under the caret is
        // actively misleading, which is worse than no box.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.hover = Some(ShownHover {
            hover: hover_of(
                "fn main()",
                Some(deco_core::position::Range::new(
                    Position::new(0, 2),
                    Position::new(0, 6),
                )),
            ),
            asked_at: Position::new(0, 3),
        });

        s.view.selections = deco_core::SelectionSet::caret(Position::new(0, 9));
        assert!(lsp.cursor_moved(&mut s), "outside the range");
        assert!(lsp.hover().is_none());
        assert_eq!(s.context.get("editorHoverVisible"), Some(&json!(false)));
    }

    // ---- The references list ---------------------------------------------

    fn location(path: &str, line: u32, character: u32) -> deco_lsp::Location {
        deco_lsp::Location {
            uri: deco_lsp::Uri::from_path(Path::new(path), deco_lsp::uri::PathStyle::Unix).unwrap(),
            range: deco_core::position::Range::new(
                Position::new(line, character),
                Position::new(line, character + 3),
            ),
        }
    }

    #[test]
    fn references_are_offered_as_a_list_with_their_line_text() {
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/a.toml"), "one = 1\ntwo = total\n");
        let mut lsp = Lsp::new(&mut s, Some(PathBuf::from("/w")));

        lsp.offer_locations(&mut s, &[location("/w/a.toml", 1, 6)], &mut here());
        let prompt = s.prompt.as_ref().expect("a list should be open");
        assert_eq!(prompt.matches(), 1);
        // The path is shortened against the workspace root, and the line's text
        // is what makes the row readable.
        assert_eq!(prompt.selected().unwrap().title, "a.toml:2: two = total");
        assert_eq!(s.status.as_deref(), Some("1 location"));
    }

    #[test]
    fn the_open_documents_own_text_is_used_rather_than_what_is_on_disk() {
        // They differ exactly when there are unsaved changes, and the list has to
        // describe what the user is looking at.
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/a.toml"), "edited in memory\n");
        let mut lsp = Lsp::new(&mut s, Some(PathBuf::from("/w")));
        lsp.offer_locations(&mut s, &[location("/w/a.toml", 0, 0)], &mut here());
        assert!(
            s.prompt
                .as_ref()
                .unwrap()
                .selected()
                .unwrap()
                .title
                .contains("edited in memory"),
            "a file that does not exist on disk still shows its line"
        );
    }

    #[test]
    fn an_empty_answer_says_so_rather_than_opening_an_empty_list() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.offer_locations(&mut s, &[], &mut here());
        assert!(s.prompt.is_none());
        assert_eq!(s.status.as_deref(), Some("no locations found"));
    }

    #[test]
    fn several_locations_are_counted_in_the_plural() {
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/a.toml"), "a\nb\nc\n");
        let mut lsp = Lsp::new(&mut s, Some(PathBuf::from("/w")));
        lsp.offer_locations(
            &mut s,
            &[location("/w/a.toml", 0, 0), location("/w/a.toml", 2, 0)],
            &mut here(),
        );
        assert_eq!(s.status.as_deref(), Some("2 locations"));
        assert_eq!(s.prompt.as_ref().unwrap().matches(), 2);
    }

    #[test]
    fn a_location_outside_the_workspace_keeps_its_whole_path() {
        assert_eq!(
            shorten(Path::new("/w/src/main.rs"), Some(Path::new("/w"))),
            "src/main.rs"
        );
        assert_eq!(
            shorten(Path::new("/elsewhere/dep.rs"), Some(Path::new("/w"))),
            "/elsewhere/dep.rs"
        );
        assert_eq!(shorten(Path::new("/w/a.rs"), None), "/w/a.rs");
    }

    #[test]
    fn a_stale_references_answer_is_ignored() {
        // The same rule the other requests follow: an answer to a question the
        // user has moved on from describes a position that no longer exists.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.references_request = Some(deco_lsp::RequestId::Number(7));
        assert_ne!(
            lsp.references_request,
            Some(deco_lsp::RequestId::Number(8)),
            "the ids differ, so the answer is not this request's"
        );
    }

    #[test]
    fn the_end_of_a_hover_range_still_counts_as_inside() {
        // Unlike a diagnostic: the caret sitting just after an identifier's last
        // character is still on that identifier as far as the user is concerned.
        let shown = ShownHover {
            hover: hover_of(
                "x",
                Some(deco_core::position::Range::new(
                    Position::new(1, 4),
                    Position::new(1, 8),
                )),
            ),
            asked_at: Position::new(1, 5),
        };
        assert!(shown.applies_at(Position::new(1, 8)));
        assert!(!shown.applies_at(Position::new(1, 9)));
    }

    #[test]
    fn a_hover_without_a_range_applies_only_where_it_was_asked() {
        // Guessing a wider area would keep a stale box on screen.
        let shown = ShownHover {
            hover: hover_of("x", None),
            asked_at: Position::new(2, 4),
        };
        assert!(shown.applies_at(Position::new(2, 4)));
        assert!(!shown.applies_at(Position::new(2, 5)));
    }

    #[test]
    fn context_keys_are_false_with_no_server() {
        // F12 must look dead when nothing offers definitions — that is correct,
        // not a bug, and the keys are what make it so.
        let mut s = session(Settings::with_defaults());
        let lsp = Lsp::new(&mut s, None);
        lsp.sync_context(&mut s);

        for key in [
            "editorHasHoverProvider",
            "editorHasDefinitionProvider",
            "editorHasReferenceProvider",
            "editorHasRenameProvider",
            "editorHasCodeActionsProvider",
            "editorHoverVisible",
        ] {
            assert_eq!(s.context.get(key), Some(&json!(false)), "{key}");
        }
    }

    #[test]
    fn requesting_code_actions_without_a_server_says_so() {
        // `ctrl+.` is gated on the context key, so this is reached from the
        // palette, where the command is offered unconditionally.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.request_code_actions(&mut s);
        assert_eq!(
            s.status.as_deref(),
            Some("no language server for this file")
        );
        assert!(s.prompt.is_none());
    }

    #[test]
    fn detaching_forgets_the_actions_that_were_on_offer() {
        // An index into a list from a server that is gone would select whatever
        // happened to land at that position next.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.code_actions = code_actions(&json!([{"title": "Fix", "edit": {"changes": {}}}]));
        lsp.code_action_request = Some(deco_lsp::RequestId::Number(1));
        lsp.resolve_request = Some(deco_lsp::RequestId::Number(2));

        lsp.detach();
        assert!(lsp.code_actions.is_empty());
        assert!(lsp.code_action_request.is_none());
        assert!(lsp.resolve_request.is_none());
    }

    #[test]
    fn choosing_an_action_that_is_no_longer_there_does_nothing() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.run_code_action(&mut s, "3", &mut here());
        lsp.run_code_action(&mut s, "not a number", &mut here());
        assert_eq!(s.status, None, "nothing to say and nothing to alarm about");
    }

    #[test]
    fn a_disabled_action_says_why_rather_than_doing_nothing() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.code_actions = code_actions(&json!([{
            "title": "Extract into function",
            "disabled": {"reason": "not inside a function"},
        }]));

        lsp.run_code_action(&mut s, "0", &mut here());
        assert_eq!(
            s.status.as_deref(),
            Some("`Extract into function` is unavailable: not inside a function")
        );
    }

    #[test]
    fn an_action_that_only_runs_a_command_is_refused_by_name() {
        // Running one is `workspace/executeCommand`, whose result comes back as
        // a request in the other direction. Saying so beats a key that appears
        // to work and changes nothing.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.code_actions = code_actions(&json!([
            {"title": "Organize imports", "command": "rust-analyzer.organizeImports"},
        ]));

        // Its `edit` is absent, but it is not an action waiting to be filled in:
        // `codeAction/resolve` takes a `CodeAction`, so this goes straight to the
        // refusal rather than to a round trip the server cannot answer.
        lsp.run_code_action(&mut s, "0", &mut here());
        let status = s.status.clone().expect("a reason");
        assert!(
            status.contains("Organize imports") && status.contains("rust-analyzer.organizeImports"),
            "the message should name the action and the command it would run: {status}"
        );
    }

    #[test]
    fn an_action_whose_edit_changes_nothing_says_so() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.code_actions = code_actions(&json!([
            {"title": "Tidy", "kind": "quickfix", "edit": {"changes": {}}},
        ]));

        lsp.run_code_action(&mut s, "0", &mut here());
        assert_eq!(s.status.as_deref(), Some("`Tidy` changes nothing"));
    }

    #[test]
    fn an_action_that_wants_a_file_operation_is_refused_by_name() {
        // Refused for this action alone. The rest of the menu is untouched,
        // which is why the edit is parsed on selection rather than on listing.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.code_actions = code_actions(&json!([{
            "title": "Move to its own file",
            "kind": "refactor.move",
            "edit": {"documentChanges": [
                {"kind": "create", "uri": "file:///w/new.rs"},
            ]},
        }]));

        lsp.run_code_action(&mut s, "0", &mut here());
        let status = s.status.clone().expect("a reason");
        assert!(
            status.contains("Move to its own file") && status.contains("create a file"),
            "the message should name the action and the operation: {status}"
        );
    }

    #[test]
    fn requesting_a_hover_without_a_server_does_nothing_visible() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.request_hover(&mut s);
        lsp.request_definition(&mut s);
        assert!(lsp.hover().is_none());
        assert_eq!(s.status, None);
    }

    #[test]
    fn dismissing_a_hover_clears_the_context_key() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.hover = Some(ShownHover {
            hover: hover_of("x", None),
            asked_at: Position::ZERO,
        });
        lsp.sync_context(&mut s);
        assert_eq!(s.context.get("editorHoverVisible"), Some(&json!(true)));

        lsp.dismiss_hover();
        lsp.sync_context(&mut s);
        assert_eq!(s.context.get("editorHoverVisible"), Some(&json!(false)));
    }

    #[test]
    fn detaching_forgets_the_hover_and_the_requests_in_flight() {
        // Their answers can never arrive now, and a box left on screen would
        // describe a document the server no longer has.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.hover = Some(ShownHover {
            hover: hover_of("x", None),
            asked_at: Position::ZERO,
        });
        lsp.hover_request = Some(deco_lsp::RequestId::Number(1));
        lsp.definition_request = Some(deco_lsp::RequestId::Number(2));

        lsp.detach();
        assert!(lsp.hover().is_none());
        assert!(lsp.hover_request.is_none());
        assert!(lsp.definition_request.is_none());
    }

    #[test]
    fn renaming_without_a_server_says_why_no_prompt_opened() {
        // Unlike a key that was never bound, this one *was* asked for: the
        // command came from the palette, where it is offered unconditionally.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.offer_rename(&mut s);
        assert_eq!(
            s.status.as_deref(),
            Some("no language server for this file")
        );
        assert!(s.prompt.is_none(), "and nothing to type into");
    }

    #[test]
    fn requesting_a_rename_without_a_server_does_nothing_visible() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.request_rename(&mut s, "whatever");
        assert_eq!(s.status, None);
    }

    #[test]
    fn detaching_forgets_a_rename_request() {
        // Its answer can never arrive now, and applying a stale one would edit
        // several files against positions the server no longer stands behind.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.rename_request = Some(deco_lsp::RequestId::Number(1));
        lsp.detach();
        assert!(lsp.rename_request.is_none());
    }

    #[test]
    fn formatting_without_a_server_does_nothing_visible() {
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.request_formatting(&mut s, false);
        lsp.request_formatting(&mut s, true);
        assert_eq!(s.status, None);
    }

    #[test]
    fn detaching_forgets_a_formatting_request() {
        // Its answer can never arrive now, and applying a stale one would
        // reformat against a document the server no longer has.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.format_request = Some(deco_lsp::RequestId::Number(1));
        lsp.detach();
        assert!(lsp.format_request.is_none());
    }

    #[test]
    fn the_formatting_context_key_is_false_with_no_server() {
        // ctrl+shift+i is gated on it, so the key is dead until a server offers
        // formatting — which is correct, not a bug.
        let mut s = session(Settings::with_defaults());
        let lsp = Lsp::new(&mut s, None);
        lsp.sync_context(&mut s);
        assert_eq!(
            s.context.get("editorHasDocumentFormattingProvider"),
            Some(&json!(false))
        );
    }

    /// A local filesystem, for the paths a test's edits never actually reach.
    fn here() -> crate::extensions::Files<'static> {
        crate::extensions::Files::Here
    }

    /// The actions a server would have sent, parsed the way one arriving is.
    fn code_actions(value: &serde_json::Value) -> Vec<deco_lsp::CodeAction> {
        deco_lsp::CodeAction::list_from_json(value)
    }

    // ---- The symbol list --------------------------------------------------

    fn symbol(
        name: &str,
        kind: Option<&'static str>,
        line: u32,
    ) -> deco_lsp::requests::DocumentSymbol {
        deco_lsp::requests::DocumentSymbol {
            name: name.to_owned(),
            kind,
            container: None,
            position: Position::new(line, 4),
        }
    }

    #[test]
    fn symbols_are_offered_as_a_list_with_their_kind() {
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/a.toml"), "one = 1\ntwo = 2\n");
        let mut lsp = Lsp::new(&mut s, Some(PathBuf::from("/w")));

        lsp.offer_symbols(
            &mut s,
            Path::new("/w/a.toml"),
            &[symbol("one", Some("key"), 0), symbol("two", Some("key"), 1)],
        );
        let prompt = s.prompt.as_ref().expect("a list should be open");
        assert_eq!(prompt.matches(), 2);
        let first = prompt.selected().expect("a selection");
        assert_eq!(first.title, "one");
        // The kind, so a field and a method of the same name are told apart.
        assert_eq!(first.detail.as_deref(), Some("key"));
        // And the document, so accepting it navigates the file that was asked
        // about rather than whichever is on screen.
        assert_eq!(first.id, "/w/a.toml");
        assert_eq!(s.status.as_deref(), Some("2 symbols"));
    }

    #[test]
    fn a_symbol_with_no_recognised_kind_gets_no_second_column() {
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/a.toml"), "one = 1\n");
        let mut lsp = Lsp::new(&mut s, None);
        lsp.offer_symbols(&mut s, Path::new("/w/a.toml"), &[symbol("one", None, 0)]);
        let prompt = s.prompt.as_ref().expect("a list should be open");
        assert_eq!(prompt.selected().unwrap().detail, None);
    }

    #[test]
    fn an_empty_symbol_list_reports_rather_than_opening_a_prompt() {
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/a.toml"), "one = 1\n");
        let mut lsp = Lsp::new(&mut s, None);
        lsp.offer_symbols(&mut s, Path::new("/w/a.toml"), &[]);
        assert!(s.prompt.is_none());
        assert_eq!(
            s.status.as_deref(),
            Some("this server found no symbols in this file")
        );
    }

    #[test]
    fn a_superseded_symbol_answer_is_ignored() {
        // Two `ctrl+shift+o` presses in a row: the first answer describes a list
        // the prompt is no longer about.
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/a.toml"), "one = 1\n");
        let mut lsp = Lsp::new(&mut s, None);
        lsp.symbols_request = Some((deco_lsp::RequestId::Number(9), PathBuf::from("/w/a.toml")));

        // An answer to an older request leaves the outstanding one in place.
        let stale = Update::Symbols {
            id: deco_lsp::RequestId::Number(8),
            symbols: vec![symbol("stale", Some("key"), 0)],
        };
        lsp.absorb(&mut s, vec![stale], &mut here());
        assert!(s.prompt.is_none());
        assert!(lsp.symbols_request.is_some(), "still waiting for 9");

        lsp.absorb(
            &mut s,
            vec![Update::Symbols {
                id: deco_lsp::RequestId::Number(9),
                symbols: vec![symbol("fresh", Some("key"), 0)],
            }],
            &mut here(),
        );
        assert_eq!(
            s.prompt
                .as_ref()
                .and_then(|p| p.selected())
                .map(|e| e.title.clone())
                .as_deref(),
            Some("fresh")
        );
        assert!(lsp.symbols_request.is_none());
    }

    #[test]
    fn the_symbol_provider_gates_its_key() {
        // ctrl+shift+o is gated on it, so the key is dead until a server offers
        // document symbols.
        let mut s = session(Settings::with_defaults());
        let lsp = Lsp::new(&mut s, None);
        lsp.sync_context(&mut s);
        assert_eq!(
            s.context.get("editorHasDocumentSymbolProvider"),
            Some(&json!(false))
        );
    }
}
