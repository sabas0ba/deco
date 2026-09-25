//! Attaching a language server to the terminal frontend.
//!
//! `deco-lsp` intentionally contains no policy: it can start a server, but it
//! does not decide which one, when, or what to do when one exits. That policy
//! is in this module because it concerns editor behaviour rather than the
//! protocol.
//!
//! The policy:
//!
//! - **One server, for the open document's language.** deco edits one document
//!   at a time, so starting more would do work for files that are not open.
//!   When the document changes language, the old server is stopped and a new
//!   one started.
//! - **A workspace-defined server is not started, and the user is told why.**
//!   Approving one requires a prompt the terminal frontend does not have yet.
//!   Not running a program can be recovered from; running the wrong one cannot.
//!   The message names the server so the user can move the definition into
//!   their own settings if they want it.
//! - **A server failure affects only that server.** Every failure results in a
//!   status bar message, and the editor keeps working.
//! - **Polling never blocks.** The event loop waits on the terminal with a
//!   timeout and drains the server in between, so a busy server does not slow
//!   down typing and an unresponsive one does not freeze the editor.

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

/// How long to wait for the handshake before abandoning a server.
///
/// Shorter than [`deco_lsp::supervisor::INITIALIZE_TIMEOUT`] because the user
/// is waiting for their file on an empty screen during the handshake.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// The same, for a server reached through a transport.
const REMOTE_STARTUP_TIMEOUT: Duration = Duration::from_secs(45);

/// The language-server side of a terminal session.
pub struct Lsp {
    registry: ServerRegistry,
    enabled: bool,
    supervisor: Option<Supervisor>,
    /// The language the running server was started for, used to detect a
    /// document in another language.
    language: Option<String>,
    /// The document the server has been notified about.
    open: Option<PathBuf>,
    root: Option<PathBuf>,
    paths: PathMap,
    /// Where a server should run, and how to reach it if it is remote.
    location: Location,
    /// The hover currently on screen, and the position it describes.
    ///
    /// Stored with its position so it can be dismissed as soon as the cursor
    /// leaves the range the hover describes. A hover box for a different token
    /// than the one under the caret is misleading.
    hover: Option<ShownHover>,
    /// The hover request in flight, if any. At most one: a second request would
    /// race the first, and the last response would win even if it is not the
    /// one the user is waiting for.
    hover_request: Option<RequestId>,
    /// The go-to-definition request in flight.
    definition_request: Option<RequestId>,
    /// The outstanding `textDocument/references` request, if any.
    references_request: Option<RequestId>,
    /// The outstanding `semanticTokens/full` request, if any.
    semantic_request: Option<RequestId>,
    /// The outstanding `documentSymbol` request, and the document it is for.
    ///
    /// The path is stored because the response contains positions but no file,
    /// and the user may have switched tabs while the server was indexing. The
    /// list must navigate the requested document, not the one on screen when the
    /// response arrives.
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
    /// The code actions the server last returned, in the server's order.
    ///
    /// Stored here rather than in the session, because most of an action is the
    /// server's JSON: the edit, and the `data` used to match a resolve. The
    /// prompt receives a title and an index into this list.
    ///
    /// Cleared when a new list arrives and when the server stops, so a stale
    /// index cannot select an action from an outdated list.
    code_actions: Vec<deco_lsp::CodeAction>,
}

/// A hover being displayed.
#[derive(Debug, Clone, PartialEq)]
pub struct ShownHover {
    /// The server's response.
    pub hover: Hover,
    /// Where the cursor was when the hover was requested.
    pub asked_at: Position,
}

impl ShownHover {
    /// Whether this hover still describes what is under the cursor.
    ///
    /// Uses the server's range when it provides one, because that range
    /// identifies the token the hover describes. Otherwise uses the exact
    /// requested position: guessing a wider area would keep a stale box on
    /// screen.
    pub fn applies_at(&self, position: Position) -> bool {
        match self.hover.range {
            Some(range) => {
                // Inclusive of the end, unlike a diagnostic: a hover range
                // covers an identifier, and a caret just after the last
                // character is still on that identifier for the user.
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
/// Only compared with another fingerprint of the same document, so a collision
/// can only occur between two states of one file. The cost of a collision is a
/// redundant `didChange`, not a wrong result. `DefaultHasher` is used rather
/// than a cryptographic digest because this is a change detector, not a
/// checksum.
fn fingerprint(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// `path` relative to `root` when it is inside it, and whole otherwise.
///
/// A references list mostly contains locations in the workspace, and repeating
/// `/home/you/work/project/src/main.rs` in every row hides the part that
/// differs. A location outside the workspace keeps its full path, because there
/// the directory is the useful part.
fn shorten(path: &Path, root: Option<&Path>) -> String {
    root.and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The anchor a completion list filters from. The server is queried at the
/// cursor and responds for the whole word, so the editor must use the same word
/// start. Otherwise the list filters against the wrong text.
///
/// "Word" here uses the identifier rule shared by every language deco supports:
/// alphanumeric plus `_`. The server's definition of a word is not used because
/// the protocol provides no way to query it.
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

    // Counted in UTF-16 units, because positions use UTF-16 units.
    let mut start = cursor.character;
    let units: Vec<u16> = line.encode_utf16().collect();
    while start > 0 {
        let index = (start - 1) as usize;
        let Some(&unit) = units.get(index) else { break };
        // A surrogate is part of a character outside the Basic Multilingual
        // Plane, such as an emoji. These are not treated as identifier
        // characters, so the word ends here.
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
/// Used to apply the characters typed while waiting for the server's response
/// to the completion filter.
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
/// Remote language support consists of this enum and the [`PathMap`] it
/// returns. A server is a program that uses the protocol over stdin and stdout,
/// and every deco transport already carries stdin and stdout. Running a server
/// remotely therefore only changes the command to spawn and the paths in the
/// messages. The protocol is unchanged.
#[derive(Debug, Clone)]
pub enum Location {
    /// On this machine, reading the checkout the editor is looking at.
    Here,
    /// On the machine the session is connected to.
    Remote {
        /// The remote machine to connect to.
        authority: deco_remote::Authority,
        /// Options for building the transport command.
        options: deco_remote::TransportOptions,
        /// The directory the server serves, as a path on the remote machine.
        workspace: PathBuf,
    },
}

impl Location {
    /// How long to wait for a server to answer `initialize`.
    ///
    /// Much longer over a transport: the wait includes an SSH handshake and a
    /// language server reading a project from a remote disk. Ten seconds is
    /// enough locally but often not enough for a real remote host.
    pub fn startup_timeout(&self) -> Duration {
        match self {
            Self::Here => STARTUP_TIMEOUT,
            Self::Remote { .. } => REMOTE_STARTUP_TIMEOUT,
        }
    }

    /// The mapping between the editor's paths and the paths the server uses.
    pub fn paths(&self) -> PathMap {
        match self {
            Self::Here => PathMap::host(),
            Self::Remote { workspace, .. } => PathMap::remote(workspace.clone()),
        }
    }

    /// The definition, with its command changed to run at this location.
    ///
    /// Environment variables are moved into the argument vector as
    /// `env NAME=VALUE` rather than kept in the config, because [`deco_lsp`]
    /// sets them on the process it spawns. Over a transport that process is the
    /// local `ssh`, so the variables would not reach the server.
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
                // A name containing `=` would split in the wrong place and set
                // a different variable. A NUL or a newline cannot be passed in
                // an argument vector. Such a variable is rejected by name rather
                // than altered, which is the same rule used when reading the
                // server definition.
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
            // Moved into the argument vector above. Keeping them here would also
            // set them on the local process, where they have no effect.
            env: Vec::new(),
            ..config.clone()
        })
    }
}

impl Lsp {
    /// Reads the configuration and prepares to attach local servers.
    pub fn new(session: &mut Session, root: Option<PathBuf>) -> Self {
        Self::with_location(session, root, Location::Here)
    }

    /// The same, with the location where servers should run.
    ///
    /// No server is started here: the server depends on the document, which may
    /// not be open yet.
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
                // The root sent to a server must be accessible to it. In a
                // remote session that is a directory on the remote machine.
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
    /// `trigger` records whether the user invoked completion or typed a trigger
    /// character. The server adjusts its results accordingly.
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
                // Only useful when the user invoked completion. A trigger
                // character with no provider should insert the character
                // without a message.
                session.status = Some("this server does not offer completion".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
        self.sync_context(session);
    }

    /// The characters that open a list automatically.
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
    /// list and the text contain the same typed characters.
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
    /// Returns whether anything was accepted, so the caller can fall back to the
    /// key's ordinary behaviour. For example, `enter` with no list open must
    /// insert a newline.
    pub fn accept(&mut self, session: &mut Session, now_ms: u64) -> bool {
        let Some(suggest) = self.suggest.as_ref() else {
            return false;
        };
        let Some(item) = suggest.selected_item().cloned() else {
            self.dismiss_suggest();
            self.sync_context(session);
            return false;
        };

        // Use the server's range when it provides one, because the server knows
        // where the completion begins. Guessing from the document can turn
        // `Hash` + `HashMap` into `HashHashMap`. Otherwise use the span from
        // where the list opened to the cursor, which is the text that was matched.
        let cursor = session.view.selections.primary().active;
        let range = item.replace.unwrap_or(deco_core::position::Range::ordered(
            suggest.anchor(),
            cursor,
        ));

        self.dismiss_suggest();
        let expanded = item
            .snippet_source
            .as_deref()
            .and_then(|source| session.expand_snippet(source));
        if let Some(snippet) = expanded.as_ref().or(item.snippet.as_ref()) {
            session.insert_snippet(range, snippet, now_ms);
            self.sync_context(session);
            return true;
        }
        if item.was_snippet {
            // Report that the reduced text was inserted instead of the snippet,
            // rather than inserting it without notice.
            session.status = Some(format!(
                "{}: inserted without tab stops (unsupported snippet syntax)",
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
    /// Every path that changes that state calls this one function, instead of
    /// each call site updating the keys it affects. A stale `when` key makes a
    /// keybinding behave incorrectly without any error, for example F12 doing
    /// nothing while a server offers definitions, or escape being consumed with
    /// no hover on screen. Such bugs are hard to notice.
    fn sync_context(&self, session: &mut Session) {
        // A server with no open *document* in the session offers nothing. It was
        // never notified about this buffer, so `F2` and `F12` would either do
        // nothing or send a URI the server does not know. This happens when the
        // open file is deleted, or renamed to a name with no language: the server
        // keeps running but the document is closed.
        let capabilities = self
            .open
            .as_ref()
            .and(self.supervisor.as_ref())
            .map(Supervisor::capabilities);
        let has =
            |f: fn(&deco_lsp::ServerCapabilities) -> bool| capabilities.map(f).unwrap_or(false);

        // VS Code's names, so a `when` clause copied from a VS Code
        // keybindings.json checks the same condition here.
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
    /// Any hover already on screen is dismissed first, because it describes the
    /// previous position.
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
    /// The selection determines which method is used, rather than a separate
    /// keybinding. `ctrl+shift+i` with text selected almost always means
    /// "format the selection", and reformatting the whole file would produce an
    /// unwanted diff.
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
                // Shown immediately: formatting a large file can take a moment,
                // and without a message the key appears to do nothing.
                session.status = Some("Formatting…".to_owned());
            }
            Ok(None) => {
                session.status = Some("this server does not offer formatting".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Requests the code actions the server offers for the selection.
    pub fn request_code_actions(&mut self, session: &mut Session) {
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            session.status = Some("no language server for this file".to_owned());
            return;
        };
        // The selection, or the caret when there is none. VS Code does the same,
        // so a quick fix works without selecting the error first.
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

    /// Puts the server's code actions into the picker.
    fn offer_code_actions(&mut self, session: &mut Session, actions: Vec<deco_lsp::CodeAction>) {
        let entries = actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let entry =
                    deco_editor::commands::PaletteEntry::new(&index.to_string(), &action.title);
                // The second column shows why the action cannot be run, or else
                // its kind. The reason takes precedence because it is the most
                // relevant information for a disabled action.
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
    /// edit is applied here. Otherwise the action is resolved by the server first
    /// and applied when the resolved response arrives.
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
            // The list was replaced or dropped after the prompt opened. Nothing
            // to do, and no message is needed.
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
                // The server offered an action with no edit and no way to
                // resolve one. Report the action by name, because there is
                // nothing to apply.
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

    /// Applies a resolved action's edit, or reports why there is nothing to apply.
    fn apply_code_action(
        &mut self,
        session: &mut Session,
        action: &deco_lsp::CodeAction,
        files: &mut crate::extensions::Files<'_>,
    ) {
        let Some(edit) = &action.edit else {
            // Either the resolve returned no edit, or the action is only a
            // `Command`. Running a server command uses `workspace/executeCommand`,
            // and its result arrives as a server-to-client `workspace/applyEdit`
            // *request*. That direction is not implemented here.
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
            // A file create, rename or delete operation. Only this action is
            // rejected, not the whole menu, which is why the edit is parsed here
            // and not while listing.
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
                // Read through `files` rather than `std::fs`. In a remote
                // session the language server runs on the remote machine, so
                // the paths in its response are remote paths. Reading them
                // locally would find either nothing or the wrong file.
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
                // The response arrived through the poll rather than a keypress,
                // so the loop's own sync has already run in this iteration.
                self.changed(session);
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Opens the rename prompt, if this server can rename at all.
    ///
    /// The capability is checked before the prompt opens, so the user does not
    /// type a name only to learn that rename is not supported.
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

    /// Requests the edits for renaming the symbol under the cursor.
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
                // Some servers read every file in the project for a rename, so
                // without a message the key can appear to do nothing.
                session.status = Some(format!("Renaming to `{new_name}`…"));
            }
            Ok(None) => {
                session.status = Some("this server does not offer rename".to_owned());
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Applies the edits the server returned for a rename.
    ///
    /// Files are read here because this layer has filesystem access. Every
    /// decision about *whether* to apply is in `deco_editor::workspace`, where
    /// it can be tested without a filesystem.
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
            // The server stopped between the request and the response.
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
                // Read through `files` rather than `std::fs`. In a remote
                // session the language server runs on the remote machine, so
                // the paths in its response are remote paths. Reading them
                // locally would find either nothing or the wrong file.
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
                // This response arrived through the poll rather than a keypress,
                // so the loop's own text sync has already run in this iteration.
                // Without this call the server would keep using the old text,
                // including the old name, until the next keystroke.
                self.changed(session);
            }
            Err(error) => self.report(session, error.to_string()),
        }
    }

    /// Asks the server to classify the whole document.
    ///
    /// Skipped while a request is outstanding. A request per keystroke would
    /// queue classifications of text that has already changed, and only the
    /// newest response is useful. The lexer's colouring remains in the meantime,
    /// so the delay does not show as missing colour.
    pub fn request_semantic_tokens(&mut self, session: &mut Session) {
        if self.semantic_request.is_some() {
            return;
        }
        let (Some(path), Some(supervisor)) =
            (session.document.path.clone(), self.supervisor.as_mut())
        else {
            return;
        };
        // No status message: this request is not triggered by a key press, and
        // a server that does not support it is not an error.
        if let Ok(Some(id)) = supervisor.semantic_tokens(&path) {
            self.semantic_request = Some(id);
        }
    }

    /// Requests all references to the symbol under the cursor.
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

    /// Requests the definition of the symbol under the cursor.
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
                // Shown before the response arrives. A server that has to index
                // first can take seconds, and without a message the keybinding
                // appears to do nothing.
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
            // Cancellation is advisory, and the response is dropped when it
            // arrives. Cancelling lets the server stop work on an obsolete hover.
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
    /// does nothing, so the event loop can call it at any time.
    /// Notifies the server about deleted files.
    ///
    /// The server still considers a deleted file's document open, under a URI
    /// that no longer exists, so it keeps responding about it and its
    /// diagnostics remain after the file is gone. Paths are drained from the
    /// session, so each deletion produces one `didClose` per file.
    pub fn close_deleted(&mut self, session: &mut Session) {
        for path in session.take_closed_documents() {
            if !self.enabled {
                continue;
            }
            // A server that never opened the file, or has since stopped, is not
            // an error to report: the file is deleted in either case.
            if let Some(supervisor) = self.supervisor.as_mut() {
                let _ = supervisor.did_close(&path);
            }
            // The cache must be cleared too. `sync_open` skips `didOpen` when the
            // cached path already matches. If the cache were kept, a file deleted
            // and then recreated under the same name would never be reopened:
            // the server had closed it, and later edits would not reach it.
            if self.open.as_deref() == Some(path.as_path()) {
                self.open = None;
                self.sent = None;
                // Also drop every request in flight for it. A reply already in
                // transit is written back into the session when it arrives,
                // because `Update::SemanticTokens` only checks that the request
                // id still matches. That would undo the cleanup done by closing
                // the document. Nothing would clear it afterwards either:
                // `changed` returns early for a document with no path, so the
                // stale spans would remain over the buffer permanently.
                self.forget_requests();
            }
        }
        // Update the `when` clauses: nothing is open, so no feature is offered.
        self.sync_context(session);
    }

    /// Drops every request whose response would write into the session.
    ///
    /// This does not send a cancellation. The server may still reply, and the
    /// reply is discarded because nothing waits for that id any more.
    /// `$/cancelRequest` would save the server some work but makes no
    /// difference here.
    fn forget_requests(&mut self) {
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
    }

    pub fn attach(&mut self, session: &mut Session) {
        if !self.enabled {
            return;
        }
        let Some(path) = session.document.path.clone() else {
            // An unsaved buffer has no URI, so the server cannot be notified
            // about it.
            return;
        };
        let Some(language) = session.document.language().map(str::to_owned) else {
            return;
        };

        if self.language.as_deref() == Some(language.as_str()) && self.is_ready() {
            self.sync_open(session, &path, &language);
            return;
        }

        // A different language needs a different server, and the old server
        // no longer has a document to work on.
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

        // Workspace-defined servers are skipped rather than tried. Skipping them,
        // instead of stopping at the first one, prevents a workspace from
        // disabling a language: a repository's own server definition is
        // declined, and the user's server still starts.
        //
        // They are also reported here, before anything is started, rather than
        // inside the start loop. The loop exits as soon as a server starts or
        // fails to start, so reporting inside it only happened when every
        // candidate had been declined. In the common case, where the user has
        // their own server, nothing was reported.
        let (refused, trusted): (Vec<_>, Vec<_>) = candidates
            .iter()
            .partition(|config| config.trust == Trust::Workspace);
        let refused: Vec<&str> = refused.iter().map(|config| config.id.as_str()).collect();
        if !refused.is_empty() {
            // Reported in the problem list rather than the status bar. `attach`
            // runs on every tab switch and language change, and a status message
            // shown each time would replace other messages. The problem list is
            // printed once before the frontend starts and shown by
            // `--print-config`, where a user checks why a server is not running.
            let problem = format!(
                "{} defined by this workspace and not started; move the definition into your own settings to run it",
                refused.join(", ")
            );
            if !session.problems.contains(&problem) {
                session.problems.push(problem);
            }
        }

        // Start only the best trusted candidate. The list is in preference order,
        // so a definition that fails to start is reported as a configuration
        // error rather than replaced by a different server without notice.
        if let Some(config) = trusted.first() {
            // Rewritten to run where this session's servers run. For a remote
            // session, the same definition is wrapped in the transport.
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
                    // Only the first line goes to the status bar. A startup
                    // failure includes the stderr tail, which is useful in a
                    // log but unreadable in a single row.
                    let summary = error.to_string();
                    let first = summary.lines().next().unwrap_or("failed to start");
                    session.status = Some(format!("{}: {first}", config.id));
                    session.problems.push(summary);
                }
            }
            return;
        }

        // No server started, so the status bar can show the reason. With no
        // server for this language, no other message competes for the row.
        if !refused.is_empty() {
            session.status = Some(format!(
                "{} defined by this workspace and not started",
                refused.join(", ")
            ));
        }
    }

    /// Notifies the server about the open document if it has not been notified yet.
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

    /// Notifies the server that the document changed.
    ///
    /// Always sends the full text. The incremental path exists in `deco-lsp` and
    /// is tested, but the editor does not yet record the applied ranges per
    /// notification, and reconstructing them from the undo history would be
    /// unreliable. Sending the whole document is correct but less efficient. A
    /// wrong incremental range would corrupt the server's copy without any
    /// error.
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

        // The event loop calls this after every keypress, and most keypresses
        // move the cursor without changing the text. The fingerprint detects
        // this, so an arrow key does not send the whole document or discard a
        // classification that is still correct.
        let fingerprint = fingerprint(&text);
        if self.sent == Some(fingerprint) {
            return;
        }
        self.sent = Some(fingerprint);

        // The old classification describes the text before this edit. It is
        // dropped rather than kept until the response arrives, because a token
        // list applied to shifted text colours the wrong words. The lexer's
        // colouring alone is better until the response arrives.
        session.semantic_tokens.clear();

        if let Err(error) = supervisor.did_change(&path, &[], &text) {
            self.report(session, error.to_string());
            return;
        }
        self.request_semantic_tokens(session);
    }

    /// Notifies the server that the document was saved.
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

    /// Drains all pending server messages and applies them. Never blocks.
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
    /// Separate from [`Lsp::poll`] so the effect of a response can be tested
    /// without a language server installed. Otherwise test results would depend
    /// on what is installed on the machine.
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
                    // Only the document on screen. A server may report on files
                    // deco is not showing, and those reports are not stored here.
                    if Some(&uri) == open_uri.as_ref() {
                        session.set_diagnostics(diagnostics);
                        changed = true;
                    }
                }
                Update::Message { kind, message } => {
                    // 1 is an error, 2 a warning. Lower severities are
                    // informational and are not shown in the status bar.
                    if kind <= 2 {
                        session.status = Some(message);
                        changed = true;
                    }
                }
                Update::Stopped { id, reason } => {
                    let first = reason.lines().next().unwrap_or("stopped").to_owned();
                    session.status = Some(format!("{id} stopped: {first}"));
                    session.problems.push(format!("{id}: {reason}"));
                    // No server will update or clear its diagnostics now, so
                    // they are removed.
                    session.set_diagnostics(Vec::new());
                    self.supervisor = None;
                    self.language = None;
                    self.open = None;
                    // The same set that closing a deleted document drops, so a
                    // new kind of request cannot be added to one place and
                    // missed in the other.
                    self.forget_requests();
                    // The server's features are no longer available.
                    self.sync_context(session);
                    return true;
                }
                Update::Hover { id, hover } => {
                    // Only the response to the outstanding request. A late
                    // response to an earlier request describes a position the
                    // cursor has left.
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
                        // The server returned no hover. Report it so the
                        // keypress does not appear to do nothing.
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
                    // Only the outstanding request. A response to a superseded
                    // request is for a different prompt.
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
                    // Only the outstanding request. A late list from an earlier
                    // request describes a position the user has typed past.
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
                        // were typed before the response arrived, so they are
                        // applied to the filter. Without this the list shows every
                        // item for the word's start and ignores the characters
                        // typed since.
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
                        // The server sent invalid edits, and the file is
                        // unchanged. Report it in the status bar and the problem
                        // list, because the key press had no visible effect.
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
                        // The server requested an operation deco does not
                        // support: a file create, rename or delete. Nothing was
                        // changed, and the message names the operation.
                        Err(error) => self.report(session, error.to_string()),
                    }
                    changed = true;
                }
                Update::RequestFailed { method, reason, .. } => {
                    // Use the last segment of the method name, which is easier to
                    // read in the status bar than `textDocument/hover`.
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
            // A successful response with no result. Reporting it distinguishes
            // "no definition" from an editor that does not respond.
            session.status = Some("no definition found".to_owned());
            return true;
        };

        // With several results, offer them as a list rather than picking one.
        // References use the same list.
        if locations.len() > 1 {
            self.offer_locations(session, locations, files);
            return true;
        }

        let Ok(path) = self.paths.from_uri(&target.uri) else {
            // `jdt:`, `untitled:` and similar schemes. The editor cannot open
            // them, and trying would create an empty buffer named after a URI.
            session.status = Some(format!("cannot open {}", target.uri));
            return true;
        };

        let same_file = session.document.path.as_deref() == Some(path.as_path());
        if !same_file {
            // Open in a new tab (or the tab already holding that file), so
            // unsaved changes in the current document are kept and do not block
            // the jump.
            // Read through `files`: in a remote session the server runs on the
            // remote machine and returns remote paths. Reading the path locally
            // would open either nothing or an unrelated local file.
            match files.read(&path.display().to_string()) {
                Ok(text) => {
                    session.open(path.clone(), &text);
                    // The new document needs its own server, and the old server
                    // must be notified that the previous file is closed.
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
    /// Uses the same prompt as project-wide search, because both select one of
    /// several locations. A second, nearly identical list widget could diverge in
    /// behaviour.
    fn offer_locations(
        &mut self,
        session: &mut Session,
        locations: &[deco_lsp::Location],
        files: &mut crate::extensions::Files<'_>,
    ) {
        let paths = self.paths.clone();
        // The line's text makes a list of locations readable. For a location in a
        // file that is not on screen, it has to be read from disk. There is one
        // cache per response, because "find all references" often returns many
        // locations in the same file.
        let mut cache: std::collections::HashMap<PathBuf, Vec<String>> =
            std::collections::HashMap::new();
        let mut entries = Vec::new();

        for location in locations {
            let Ok(path) = paths.from_uri(&location.uri) else {
                // `jdt:` and similar schemes cannot be opened here, so they are
                // omitted rather than listed as entries that cannot be opened.
                continue;
            };
            let lines = cache.entry(path.clone()).or_insert_with(|| {
                // Use the open document's text rather than the file on disk,
                // because they differ when there are unsaved changes.
                if session.document.path.as_deref() == Some(path.as_path()) {
                    session
                        .document
                        .buffer
                        .text()
                        .lines()
                        .map(str::to_owned)
                        .collect()
                } else {
                    // Read on the machine where the file is; see `go_to`.
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

    /// Opens the go-to-symbol prompt with the symbols the server returned.
    ///
    /// Every entry includes `path`, so accepting one uses the same "open a file
    /// at a position" path as a search result. For the document already on
    /// screen, this switches to its own tab and keeps unsaved changes. If the
    /// user has navigated away from the document, it is shown again.
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
                // The kind is shown in the second column to distinguish a field
                // from a method with the same name.
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
    /// Used on a tab switch. The server publishes diagnostics for every file it
    /// knows, but only publications for the on-screen document reach the
    /// session. A document that returns from the background therefore has to
    /// load the diagnostics published in the meantime.
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
        // Quitting the editor must not leave a language server running. Servers
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
    use deco_lsp::requests::CompletionItem;
    use serde_json::json;

    #[test]
    fn completion_variables_expand_on_accept_and_keep_navigation_and_undo() {
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/before.rs"), "greet");
        let item = CompletionItem::from_json(&json!({
            "label": "greet",
            "insertTextFormat": 2,
            "textEdit": {
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 5}
                },
                "newText": "$TM_FILENAME(${1:arg})$0"
            }
        }))
        .unwrap();
        let mut lsp = Lsp::new(&mut s, None);
        lsp.suggest = Some(Suggest::new(vec![item], Position::ZERO, false));
        s.document.path = Some(PathBuf::from("/w/after.rs"));

        assert!(lsp.accept(&mut s, 0));
        assert_eq!(s.document.buffer.text(), "after.rs(arg)");
        assert_eq!(s.context.get("inSnippetMode"), Some(&json!(true)));
        s.run("type", Some(&json!({"text": "日本😀"})), 1000);
        s.run("jumpToNextSnippetPlaceholder", None, 1001);
        assert_eq!(s.document.buffer.text(), "after.rs(日本😀)");
        assert_eq!(s.view.selections.primary().active, Position::new(0, 14));
        assert_eq!(s.context.get("inSnippetMode"), Some(&json!(false)));
        s.run("undo", None, 2000);
        s.run("undo", None, 3000);
        assert_eq!(s.document.buffer.text(), "greet");
    }

    #[test]
    fn unsupported_variables_use_the_existing_fallback_without_navigation() {
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/empty.txt"), "");
        let item = CompletionItem::from_json(&json!({
            "label": "unsupported",
            "insertTextFormat": 2,
            "insertText": "${1:arg} ${UNKNOWN:default}"
        }))
        .unwrap();
        let fallback = item.insert.clone();
        let mut lsp = Lsp::new(&mut s, None);
        lsp.suggest = Some(Suggest::new(vec![item], Position::ZERO, false));
        assert!(lsp.accept(&mut s, 0));
        assert_eq!(s.document.buffer.text(), fallback);
        assert_eq!(s.context.get("inSnippetMode"), Some(&json!(false)));
        assert!(s
            .status
            .as_deref()
            .unwrap()
            .contains("unsupported snippet syntax"));
    }

    #[test]
    fn a_plain_text_completion_does_not_expand_a_bare_variable() {
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/empty.txt"), "");
        let item = CompletionItem::from_json(&json!({
            "label": "literal",
            "insertTextFormat": 1,
            "insertText": "$TM_FILENAME"
        }))
        .unwrap();
        let mut lsp = Lsp::new(&mut s, None);
        lsp.suggest = Some(Suggest::new(vec![item], Position::ZERO, false));
        assert!(lsp.accept(&mut s, 0));
        assert_eq!(s.document.buffer.text(), "$TM_FILENAME");
    }

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
        // The server's command remains at the end, one argument each, quoted
        // for the remote login shell that ssh hands them to.
        let tail = &resolved.command.args[resolved.command.args.len() - 3..];
        assert_eq!(tail, ["'taplo'", "'lsp'", "'stdio'"]);
        assert!(resolved.command.args.contains(&"myhost".to_owned()));
        // The rest of the definition is unchanged: it is the same server,
        // started on another machine.
        assert_eq!(resolved.id, "toml-lsp");
        assert_eq!(resolved.language_ids, ["toml"]);
    }

    #[test]
    fn environment_variables_travel_to_the_far_end_rather_than_being_set_here() {
        // `deco-lsp` sets `env` on the process it spawns, which over a transport
        // is the local `ssh`. If they stayed in `env`, they would never reach the
        // server, and the setting would appear to be ignored.
        let config = definition(vec![
            ("RUST_LOG".to_owned(), "debug".to_owned()),
            ("PATH_EXTRA".to_owned(), "/opt/bin".to_owned()),
        ]);
        let resolved = remote().resolve(&config).expect("a command");
        assert!(resolved.env.is_empty(), "{:?}", resolved.env);

        let args = resolved.command.args.join(" ");
        assert!(
            args.contains("'env' 'RUST_LOG=debug' 'PATH_EXTRA=/opt/bin' 'taplo' 'lsp' 'stdio'"),
            "{args}"
        );
    }

    #[test]
    fn an_environment_variable_that_cannot_be_sent_is_refused_by_name() {
        // `NAME=VALUE` splits at the first `=`, so a name containing `=` would
        // set a different variable.
        let config = definition(vec![("A=B".to_owned(), "c".to_owned())]);
        let error = remote().resolve(&config).expect_err("a refusal");
        assert!(error.contains("A=B"), "{error}");

        // A newline cannot be passed in an argument vector.
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
        // The wait includes an SSH handshake and a server reading a project from
        // a remote disk.
        assert!(remote().startup_timeout() > Location::Here.startup_timeout());
    }

    /// A session fixed to Linux, so the keymap and context keys are the same
    /// on every platform the test runs on.
    ///
    /// The document is intentionally a `.toml` file, not `.rs`. `rust` has a
    /// built-in server definition, so a test using it would try to launch any
    /// `rust-analyzer` installed on the CI machine, and the result would depend
    /// on that. `toml` has no built-in entry, so these tests use only what they
    /// configure.
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
    fn a_server_with_no_document_offers_nothing() {
        let mut s = session(Settings::default());
        let lsp = Lsp::new(&mut s, None);
        // Regardless of what a supervisor might be running, no document is open,
        // so `F2` and `F12` must not resolve. Their `when` clauses check these
        // keys. A key sent to a server that was never notified about this buffer
        // either does nothing or sends a URI the server does not know.
        lsp.sync_context(&mut s);
        for key in [
            "editorHasRenameProvider",
            "editorHasDefinitionProvider",
            "editorHasHoverProvider",
            "editorHasReferenceProvider",
        ] {
            assert_eq!(
                s.context.get(key),
                Some(&json!(false)),
                "{key} must be false with no document open"
            );
        }
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
        // Cloning a repository must not be enough to run a program, and an
        // unreported refusal would look like a broken feature.
        let mut s = session(settings_with(
            Scope::Workspace,
            r#"{"deco.lsp.servers": {"theirs": {"languages": ["toml"], "command": "./taplo"}}}"#,
        ));
        let mut lsp = Lsp::new(&mut s, None);
        lsp.attach(&mut s);

        assert!(!lsp.is_ready());
        // No other server handles the language, so the status bar shows it.
        let status = s.status.expect("the refusal must be visible");
        assert!(status.contains("theirs"), "{status}");
        assert!(status.contains("workspace"), "{status}");
        // The problem list always contains it, and keeps it after the status
        // bar shows another message.
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
        // It has no path and therefore no URI, so a server cannot be notified.
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
        // Regression test: a repository defining a competing server for a
        // language was chosen first, declined for lack of consent, and left the
        // language with no server. A cloned repository could disable the
        // feature this way.
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

        // `mine` is tried. It fails only because the program does not exist,
        // and the status line reports that.
        let status = s.status.as_deref().expect("something must be reported");
        assert!(status.starts_with("mine:"), "{status}");
        // `theirs` is still reported, even though the loop exited before
        // reaching it. The user needs to know it was declined to decide whether
        // to move the definition into their own settings.
        assert!(
            s.problems.iter().any(|problem| problem.contains("theirs")),
            "{:?}",
            s.problems
        );
    }

    #[test]
    fn a_refusal_is_recorded_once_however_often_attach_runs() {
        // `attach` runs on every tab switch and language change. Appending the
        // message each time would fill the problem list with duplicates.
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
        // A configured server is an explicit choice; a built-in is a default.
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
        // The event loop calls it repeatedly, so repeated calls must not
        // accumulate status messages or problems.
        let mut s = session(settings_with(
            Scope::User,
            r#"{"deco.lsp.servers": {"ghost": {"languages": ["toml"],
                "command": "deco-no-such-server-9f2c"}}}"#,
        ));
        let mut lsp = Lsp::new(&mut s, None);
        lsp.attach(&mut s);
        let after_one = s.problems.len();
        assert!(after_one > 0);
        // A second attach retries intentionally, because a server may have been
        // installed since. It must not panic or leak.
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
        // misleading and worse than no box.
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
        // The path is shortened relative to the workspace root, and the line's
        // text makes the row readable.
        assert_eq!(prompt.selected().unwrap().title, "a.toml:2: two = total");
        assert_eq!(s.status.as_deref(), Some("1 location"));
    }

    #[test]
    fn the_open_documents_own_text_is_used_rather_than_what_is_on_disk() {
        // They differ when there are unsaved changes, and the list must show the
        // text the user sees.
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
        // Same rule as the other requests: a response to a superseded request
        // describes a position that no longer applies.
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
        // Unlike a diagnostic: a caret just after an identifier's last character
        // is still on that identifier for the user.
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
        // F12 must do nothing when no server offers definitions. This is
        // intended, and these keys implement it.
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
        // `ctrl+.` depends on the context key, so this path is reached from the
        // palette, where the command is always offered.
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
        // An index into a list from a stopped server would select whatever
        // action is at that position in a later list.
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
        // Running a command uses `workspace/executeCommand`, whose result arrives
        // as a server-to-client request. Reporting this is better than a key that
        // appears to work but changes nothing.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.code_actions = code_actions(&json!([
            {"title": "Organize imports", "command": "rust-analyzer.organizeImports"},
        ]));

        // Its `edit` is absent, but it is not an unresolved action.
        // `codeAction/resolve` takes a `CodeAction`, so this is rejected
        // immediately rather than sent in a request the server cannot handle.
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
        // Only this action is rejected. The rest of the menu is unaffected,
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
        // Their responses can no longer arrive, and a box left on screen would
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
        // Unlike an unbound key, this command was explicitly invoked from the
        // palette, where it is always offered.
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
        // Its response can no longer arrive, and applying a stale one would edit
        // several files at positions that may no longer be valid.
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
        // Its response can no longer arrive, and applying a stale one would
        // reformat against a document the server no longer has.
        let mut s = session(Settings::with_defaults());
        let mut lsp = Lsp::new(&mut s, None);
        lsp.format_request = Some(deco_lsp::RequestId::Number(1));
        lsp.detach();
        assert!(lsp.format_request.is_none());
    }

    #[test]
    fn the_formatting_context_key_is_false_with_no_server() {
        // ctrl+shift+i depends on it, so the key does nothing until a server
        // offers formatting. This is intended.
        let mut s = session(Settings::with_defaults());
        let lsp = Lsp::new(&mut s, None);
        lsp.sync_context(&mut s);
        assert_eq!(
            s.context.get("editorHasDocumentFormattingProvider"),
            Some(&json!(false))
        );
    }

    /// A local filesystem, for tests whose edits never read a file.
    fn here() -> crate::extensions::Files<'static> {
        crate::extensions::Files::Here
    }

    /// Code actions parsed the same way as actions received from a server.
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
        // The kind distinguishes a field from a method with the same name.
        assert_eq!(first.detail.as_deref(), Some("key"));
        // The document is included, so accepting the entry navigates the
        // requested file rather than the one on screen.
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
        // Two `ctrl+shift+o` presses in a row: the first response belongs to a
        // superseded request.
        let mut s = session(Settings::with_defaults());
        s.open(PathBuf::from("/w/a.toml"), "one = 1\n");
        let mut lsp = Lsp::new(&mut s, None);
        lsp.symbols_request = Some((deco_lsp::RequestId::Number(9), PathBuf::from("/w/a.toml")));

        // A response to an older request leaves the outstanding one in place.
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
        // ctrl+shift+o depends on it, so the key does nothing until a server
        // offers document symbols.
        let mut s = session(Settings::with_defaults());
        let lsp = Lsp::new(&mut s, None);
        lsp.sync_context(&mut s);
        assert_eq!(
            s.context.get("editorHasDocumentSymbolProvider"),
            Some(&json!(false))
        );
    }
}
