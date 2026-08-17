//! Starting the extensions that are installed, and answering them.
//!
//! Here rather than in `deco-editor` for the reason the theme list and the file
//! walk are: the core has no filesystem and starts no processes. This module
//! walks the extension directories, decides nothing (that is
//! [`deco_ext::catalogue`]), and owns the host processes that result.
//!
//! # Nothing starts on its own
//!
//! A host is started when a command belonging to its extension is invoked, and at
//! no other time. `onLanguage:` and `onStartupFinished` are understood by the
//! catalogue and deliberately not acted on yet: the first version of this should
//! start a process only when the user asked for something, because that is the
//! version where a mistake costs the least. Opening a Rust file should not start
//! three extensions before that path has been used in anger.
//!
//! # Starting does not block the editor
//!
//! The first container start on a machine pulls an image, which takes as long as
//! it takes. Waiting for `$/ready` inline would freeze the editor for minutes, so
//! a host is started, the invoked command is remembered, and both are advanced by
//! [`Hosts::poll`] from the event loop — the same shape as the language-server
//! client next door.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deco_editor::commands::PaletteEntry;
use deco_editor::Session;
use deco_ext::capability::{Broker, Decision, DefaultPolicy, GrantStore, ResolutionContext};
use deco_ext::catalogue::Catalogue;
use deco_ext::connection::{dispatch, Dispatch, Host, HostEvent};
use deco_ext::host::{HostConfig, HostLimits};
use deco_ext::protocol::{ErrorCode, Message, Response};
use deco_ext::sandbox::{self, Prepared, Sandbox};

/// How many extension directories are examined before the walk gives up.
///
/// The same bound the theme walk uses, and for the same reason: a
/// marketplace-managed directory holds tens of extensions, and a number this size
/// only ever stops something pathological.
pub const MAX_EXTENSIONS: usize = 2_000;

/// How long a host may take to say `$/ready` in each mode.
///
/// Generous for a container because the first start on a machine pulls an image,
/// and a pull is not a hang. The status bar says what is happening either way.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_TIMEOUT_CONTAINER: Duration = Duration::from_secs(600);

/// How many lines of an extension's own output to keep.
const LOG_LINES: usize = 200;

/// Everything found under `roots`, in the order the roots were given.
///
/// A directory that is not an extension is skipped in silence: an extensions
/// directory routinely holds `.obsolete` and other bookkeeping. A manifest that
/// *is* there and does not parse is a problem worth reporting, because the
/// extension will appear not to exist and the reason is invisible.
pub fn discover(roots: &[PathBuf]) -> Catalogue {
    let mut found: Vec<(PathBuf, deco_ext::Manifest)> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    let mut examined = 0usize;

    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        // Sorted, so what wins a collision does not depend on the order the
        // filesystem happens to hand directories back in.
        let mut directories: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect();
        directories.sort();

        for directory in directories {
            if examined >= MAX_EXTENSIONS {
                break;
            }
            examined += 1;
            let Ok(source) = std::fs::read_to_string(directory.join("package.json")) else {
                continue;
            };
            match deco_ext::Manifest::parse(&source) {
                Ok(manifest) => found.push((directory, manifest)),
                Err(error) => unreadable.push(format!(
                    "{}: {error}",
                    directory.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }
    }

    let mut catalogue = Catalogue::build(found);
    catalogue.problems.extend(unreadable);
    catalogue
}

/// Palette rows for every command an extension contributes.
///
/// Listed whether or not the extension has started, because invoking one is what
/// starts it. The detail column is the extension's name rather than the command
/// identifier the core uses there: for an extension command, "which extension is
/// this" is the thing the title does not tell you and that decides whether you
/// want it.
pub fn rows(catalogue: &Catalogue) -> Vec<PaletteEntry> {
    catalogue
        .contributed_commands()
        .into_iter()
        .map(|(extension, command)| PaletteEntry {
            id: command.command.clone(),
            title: command.label(),
            at: None,
            detail: Some(extension.label.clone()),
        })
        .collect()
}

/// Where deco's own host code might be, given the running executable.
///
/// Tried in order. A development checkout finds the first; an installed tree
/// finds one of the others. `DECO_HOST_BOOTSTRAP` overrides all of them, which is
/// what a packager or a test uses.
pub fn bootstrap_candidates(exe: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let host = Path::new("extension-host").join("src").join("bootstrap.js");
    // `target/debug/deco` and `target/release/deco`, from a checkout.
    for up in [3usize, 2] {
        if let Some(base) = exe.ancestors().nth(up) {
            candidates.push(base.join(&host));
        }
    }
    if let Some(base) = exe.parent() {
        // Beside the binary, and the layout `cargo xtask dist` produces.
        candidates.push(base.join(&host));
        if let Some(prefix) = base.parent() {
            candidates.push(prefix.join("share").join("deco").join(&host));
        }
    }
    candidates
}

/// The host bootstrap, if it can be found.
pub fn find_bootstrap() -> Option<PathBuf> {
    if let Some(given) = std::env::var_os("DECO_HOST_BOOTSTRAP") {
        let path = PathBuf::from(given);
        return path.is_file().then_some(path);
    }
    let exe = std::env::current_exe().ok()?;
    bootstrap_candidates(&exe)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// An absolute path to `node`, for the modes that need the machine's own.
///
/// Absolute because the host's environment carries no `PATH` to search — see
/// `Host::spawn`. Not needed in a container, where the image supplies Node.
fn find_node(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let file = if cfg!(windows) { "node.exe" } else { "node" };
    std::env::split_paths(&path?)
        .map(|directory| directory.join(file))
        .find(|candidate| candidate.is_file())
}

/// What a host is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Started, waiting for `$/ready`.
    Starting,
    /// Ready, and sent `$/activate`.
    Activating,
    /// The extension's `activate` returned.
    Active,
}

/// One extension's host process.
struct Running {
    host: Host,
    state: State,
    started: Instant,
    timeout: Duration,
    /// Where the extension is mounted, for translating the paths on the wire.
    prepared: Prepared,
    /// The capability broker for *this* extension: its own declaration is the
    /// ceiling, so one per host rather than one shared.
    broker: Broker,
    /// What it registered, which is how deco knows a command can be run yet.
    registered: BTreeSet<String>,
    /// Commands invoked before it was ready, in the order they were asked for.
    queued: Vec<String>,
    /// Requests deco is waiting on, so a reply can be reported against its cause.
    asked: BTreeMap<u64, String>,
}

/// The extensions that are installed, and the hosts running some of them.
/// Where an extension's file requests are actually served from.
///
/// A remote session's files are on the other machine, so an extension reading
/// one has to read it through the same connection the editor does. The
/// alternative — reading whatever happens to be at that path on this machine —
/// is the failure mode this whole enum exists to make impossible: it would
/// silently answer from a checkout that is not the one being edited.
pub enum Files<'a> {
    /// This machine's filesystem.
    Here,
    /// The far end of a remote session.
    Remote(&'a mut deco_remote::Client),
}

impl Files<'_> {
    /// Reads `path`, as text.
    ///
    /// Text rather than bytes, and a file that is not UTF-8 is refused: deco's
    /// own editor refuses one for the same reason, and an extension handed
    /// replacement characters would write them back.
    fn read(&mut self, path: &str) -> Result<String, String> {
        match self {
            Self::Here => std::fs::read_to_string(path).map_err(|error| error.to_string()),
            Self::Remote(client) => client.read(path).map_err(|error| error.to_string()),
        }
    }

    /// Writes `text` to `path`.
    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        match self {
            Self::Here => std::fs::write(path, text).map_err(|error| error.to_string()),
            Self::Remote(client) => client.write(path, text).map_err(|error| error.to_string()),
        }
    }
}

/// What an extension is asking for, in the words a person has to decide about.
///
/// The capability's own `Debug` names its variant and its bound, which is exactly
/// the wrong register for a prompt: `ReadFile { scope: Workspace }` is a Rust
/// value, not a question.
fn describe(capability: &deco_ext::capability::Capability, method: &str) -> String {
    use deco_ext::capability::{Capability, PathScope};
    let where_ = |scope: &PathScope| match scope {
        PathScope::Workspace => "in this workspace".to_owned(),
        PathScope::ExtensionStorage => "in its own storage".to_owned(),
        PathScope::ExtensionInstall => "in its own directory".to_owned(),
        PathScope::Subtree { path } => format!("under {}", path.display()),
    };
    match capability {
        Capability::ReadFile { scope } => format!("read files {}", where_(scope)),
        Capability::WriteFile { scope } => format!("change files {}", where_(scope)),
        Capability::Network { host } => format!("connect to {host}"),
        Capability::Process { program } => format!("run {program}"),
        Capability::Env { name } => format!("read the environment variable {name}"),
        Capability::Clipboard => "use the clipboard".to_owned(),
        Capability::Secrets => "store and read secrets".to_owned(),
        Capability::OpenExternal => "open a link in your browser".to_owned(),
        // Unreachable while every variant is listed, and a description that names
        // the method beats one that says nothing if a variant is ever added.
        #[allow(unreachable_patterns)]
        _ => format!("do {method}"),
    }
}

/// A request held while the user is asked about it.
struct Asking {
    /// Which extension asked.
    extension: String,
    /// The request itself, to answer once there is a decision.
    request: deco_ext::protocol::Request,
    /// What the broker wants a decision about.
    capability: deco_ext::capability::Capability,
}

pub struct Hosts {
    catalogue: Catalogue,
    running: BTreeMap<String, Running>,
    /// The host's own output and deco's refusals, newest last.
    log: VecDeque<String>,
    /// Everything worth telling the user once, in the words they will read.
    problems: Vec<String>,
    bootstrap: Option<PathBuf>,
    node: Option<PathBuf>,
    /// What the last-offered list of decisions stood for, in the order it was
    /// offered.
    ///
    /// The choice carries an index into this rather than a spelling of the
    /// extension and capability: encoding them into a string would mean parsing
    /// one back out, and a capability's `Debug` is not a format anything should
    /// have to read.
    decisions: Vec<(String, deco_ext::capability::Capability)>,
    /// The one permission question that is open, if any.
    ///
    /// One at a time, deliberately: two prompts cannot be on screen at once, and
    /// a queue of them would mean answering a question about an extension whose
    /// request has long since been abandoned. A second extension asking while
    /// one is open is refused with that as the reason.
    asking: Option<Asking>,
    /// The workspace folders a `workspace`-scoped capability stands for.
    ///
    /// Without them a grant of `readFile: workspace` covers no concrete path at
    /// all, because a scope resolves to the roots it is given and an empty list
    /// contains nothing. In a remote session these are the *far end's*
    /// directories, which is what makes an extension's path requests mean the
    /// same thing as the session's.
    workspace_roots: Vec<PathBuf>,
}

impl Hosts {
    /// Takes a catalogue and finds what starting one of its extensions needs.
    pub fn new(catalogue: Catalogue) -> Self {
        Self::rooted(catalogue, Vec::new())
    }

    /// The same, with the workspace folders extensions may be granted.
    pub fn rooted(catalogue: Catalogue, workspace_roots: Vec<PathBuf>) -> Self {
        let problems = catalogue.problems.clone();
        Self {
            catalogue,
            running: BTreeMap::new(),
            log: VecDeque::new(),
            problems,
            bootstrap: find_bootstrap(),
            node: find_node(std::env::var_os("PATH").as_deref()),
            asking: None,
            decisions: Vec::new(),
            workspace_roots,
        }
    }

    /// Nothing installed and nothing running.
    pub fn empty() -> Self {
        Self::new(Catalogue::default())
    }

    /// What is installed.
    pub fn catalogue(&self) -> &Catalogue {
        &self.catalogue
    }

    /// Problems found while looking, for the frontend to show as it shows the
    /// settings ones.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// The extension output kept so far, oldest first.
    pub fn log(&self) -> impl Iterator<Item = &str> {
        self.log.iter().map(String::as_str)
    }

    /// How many hosts are running.
    pub fn started(&self) -> usize {
        self.running.len()
    }

    /// Runs `command` if an extension contributes it.
    ///
    /// `false` when nothing here owns the identifier, which leaves the caller to
    /// report it as unknown — a mistyped keybinding must not be swallowed by the
    /// extension machinery.
    pub fn run_command(&mut self, session: &mut Session, command: &str) -> bool {
        let Some(owner) = self.catalogue.owner_of(command) else {
            return false;
        };
        let id = owner.id.clone();
        let label = owner.label.clone();

        if let Some(running) = self.running.get_mut(&id) {
            if running.state == State::Active {
                Self::execute(running, session, command, &label);
            } else {
                // Still starting. Remembered rather than refused: the user asked
                // once and should not have to ask again when it is ready.
                running.queued.push(command.to_owned());
                session.status = Some(format!("{label} is still starting…"));
            }
            return true;
        }

        match self.start(&id, session) {
            Ok(()) => {
                if let Some(running) = self.running.get_mut(&id) {
                    running.queued.push(command.to_owned());
                }
                session.status = Some(format!("starting {label}…"));
            }
            Err(why) => {
                self.note(&why);
                session.status = Some(why);
            }
        }
        true
    }

    /// Starts the host for one extension.
    fn start(&mut self, id: &str, session: &Session) -> Result<(), String> {
        let extension = self
            .catalogue
            .by_id(id)
            .ok_or_else(|| format!("{id} is not installed"))?;
        if extension.main.is_none() {
            // A theme reaching here would mean the catalogue and this disagree
            // about what can run, which is worth saying rather than papering over.
            return Err(format!("{} has no code to run", extension.label));
        }
        let bootstrap = self.bootstrap.clone().ok_or_else(|| {
            "deco cannot find its own extension host; set DECO_HOST_BOOTSTRAP to \
             extension-host/src/bootstrap.js"
                .to_owned()
        })?;

        let policy = sandbox::policy(&session.settings);
        // In a container the image supplies Node, so a machine without one can
        // still run extensions — which is the point of pinning the runtime.
        let node = match policy {
            Sandbox::Container => self.node.clone().unwrap_or_else(|| PathBuf::from("node")),
            Sandbox::Process => self.node.clone().ok_or_else(|| {
                format!(
                    "no `node` on the PATH, which `{}: \"process\"` needs; \
                     the default container supplies its own",
                    sandbox::SANDBOX_KEY
                )
            })?,
        };

        let host_root = bootstrap
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| format!("{} is not inside a host directory", bootstrap.display()))?
            .to_path_buf();
        let config = HostConfig {
            node,
            bootstrap,
            // The host's own code and this one extension. Not the workspace: an
            // extension reads files through the broker, so it needs no view of
            // the project — and not the other extensions either.
            readable_roots: vec![host_root, extension.root.clone()],
            cwd: extension.root.clone(),
            limits: HostLimits::default(),
            node_permission_model: true,
            allow_code_generation: false,
        };

        let prepared = sandbox::prepare(
            &session.settings,
            &config,
            id,
            std::env::var_os("PATH").as_deref(),
        )
        .map_err(|error| error.to_string())?;
        for scope in &prepared.ignored {
            self.note(&format!(
                "{scope:?} settings tried to choose the extension sandbox and were ignored"
            ));
        }

        let host = Host::spawn(&prepared.spec).map_err(|error| error.to_string())?;
        let timeout = match prepared.sandbox {
            Sandbox::Container => READY_TIMEOUT_CONTAINER,
            Sandbox::Process => READY_TIMEOUT,
        };
        // The user's setting, not a constant: `prompt` asks, `allow` serves a
        // declared capability without asking, and `deny` refuses without asking.
        let policy = session
            .settings
            .get(deco_ext::capability::DEFAULT_POLICY_KEY)
            .and_then(|value| value.as_str())
            .and_then(DefaultPolicy::parse)
            .unwrap_or(DefaultPolicy::Prompt);
        let broker = Broker::new(
            self.declared(id),
            GrantStore::default(),
            policy,
            ResolutionContext {
                workspace_roots: self.workspace_roots.clone(),
                ..ResolutionContext::default()
            },
        );
        self.running.insert(
            id.to_owned(),
            Running {
                host,
                state: State::Starting,
                started: Instant::now(),
                timeout,
                prepared,
                broker,
                registered: BTreeSet::new(),
                queued: Vec::new(),
                asked: BTreeMap::new(),
            },
        );
        Ok(())
    }

    /// What an extension's manifest declared it wants.
    ///
    /// Read from the manifest again rather than kept in the catalogue: this is the
    /// ceiling the broker enforces, and the fewer copies of it there are the
    /// fewer places it can be wrong.
    fn declared(&self, id: &str) -> Vec<deco_ext::Capability> {
        let Some(extension) = self.catalogue.by_id(id) else {
            return Vec::new();
        };
        let Ok(source) = std::fs::read_to_string(extension.root.join("package.json")) else {
            return Vec::new();
        };
        deco_ext::Manifest::parse(&source)
            .map(|manifest| manifest.capabilities().0)
            .unwrap_or_default()
    }

    /// Sends `$/executeCommand` for a host that is ready for it.
    fn execute(running: &mut Running, session: &mut Session, command: &str, label: &str) {
        if !running.registered.contains(command) {
            // Contributed in the manifest but never registered in code. VS Code
            // reports this as "command not found"; saying which extension was
            // asked is more use than that.
            session.status = Some(format!("{label} did not register `{command}`"));
            return;
        }
        match running.host.execute_command(command, serde_json::json!([])) {
            Ok(id) => {
                running.asked.insert(id, command.to_owned());
            }
            Err(error) => {
                session.status = Some(format!("could not reach {label}: {error}"));
            }
        }
    }

    /// Advances every host: drains what arrived, and runs what was waiting.
    ///
    /// Called from the event loop, like the language-server client, because a host
    /// that started a minute ago is ready now and nothing else would notice.
    pub fn poll(&mut self, session: &mut Session, files: &mut Files<'_>) {
        let ids: Vec<String> = self.running.keys().cloned().collect();
        for id in ids {
            self.poll_one(&id, session, files);
        }
    }

    fn poll_one(&mut self, id: &str, session: &mut Session, files: &mut Files<'_>) {
        let mut notes: Vec<String> = Vec::new();
        let mut statuses: Vec<String> = Vec::new();
        let mut dead = false;
        // Collected rather than stored directly, because the host being drained is
        // borrowed out of `self` for the length of this loop.
        let mut pending: Option<Asking> = None;

        let Some(running) = self.running.get_mut(id) else {
            return;
        };
        let label = self
            .catalogue
            .by_id(id)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| id.to_owned());

        // A budget rather than "everything available": an extension that logs in a
        // loop must not be able to hold the event loop open.
        for _ in 0..64 {
            match running.host.poll() {
                Some(HostEvent::Message(Message::Request(request))) => {
                    let reply = match dispatch(&running.broker, &request) {
                        Dispatch::Refused(response) => {
                            notes.push(format!(
                                "{label}: refused {} — {}",
                                request.method,
                                response
                                    .error
                                    .as_ref()
                                    .map(|e| e.message.as_str())
                                    .unwrap_or("no reason given")
                            ));
                            response
                        }
                        // Asking the user is the one thing deco cannot do yet, and
                        // a prompt that does not exist must not become a silent
                        // yes. Refused, and said out loud so the reason is not a
                        // mystery when the extension misbehaves.
                        // Held rather than answered: the extension is waiting on
                        // a promise, so there is nothing to send until there is a
                        // decision. Its host stays alive and its other requests
                        // keep being served.
                        Dispatch::Consent { capability } if pending.is_none() => {
                            pending = Some(Asking {
                                extension: id.to_owned(),
                                request: request.clone(),
                                capability,
                            });
                            continue;
                        }
                        // A second question while one is open. Refused rather than
                        // queued, and the reason names the situation: a queue would
                        // mean asking about a request the extension abandoned long
                        // before anyone read the prompt.
                        Dispatch::Consent { capability } => {
                            notes.push(format!(
                                "{label}: {} needs a decision about {capability:?}, and another \
                                 permission question is already open — refused",
                                request.method
                            ));
                            Response::err(
                                request.id,
                                ErrorCode::PermissionDenied,
                                "another permission question is open, so this is refused",
                            )
                        }
                        Dispatch::Allowed => Self::mediated(
                            running,
                            &request,
                            &label,
                            &mut notes,
                            &mut statuses,
                            files,
                        ),
                    };
                    if running.host.send(&Message::Response(reply)).is_err() {
                        dead = true;
                        break;
                    }
                }
                Some(HostEvent::Message(Message::Response(response))) => {
                    let asked = running.asked.remove(&response.id);
                    let method = running.host.answered(response.id);
                    match (method.as_deref(), response.error) {
                        (_, Some(error)) => {
                            let what = asked.unwrap_or_else(|| {
                                method.clone().unwrap_or_else(|| "a request".to_owned())
                            });
                            statuses.push(format!("{label}: {what} failed — {}", error.message));
                        }
                        (Some("$/activate"), None) => {
                            running.state = State::Active;
                        }
                        (Some("$/executeCommand"), None) => {
                            // The extension's own return value. Shown when it is a
                            // string, because that is the only shape a status bar
                            // can honestly render; anything else is the caller's
                            // business and there is no caller yet.
                            let said = response
                                .result
                                .as_ref()
                                .and_then(|value| value.as_str())
                                .map(str::to_owned);
                            let what = asked.unwrap_or_else(|| "a command".to_owned());
                            statuses.push(match said {
                                Some(text) if !text.is_empty() => format!("{label}: {text}"),
                                _ => format!("{label}: {what} ran"),
                            });
                        }
                        _ => {}
                    }
                }
                Some(HostEvent::Message(Message::Notification(note))) => match note.method.as_str()
                {
                    "$/ready" => {
                        if let Err(error) = deco_ext::connection::agrees_on_protocol(&note) {
                            notes.push(format!("{label}: {error}"));
                            dead = true;
                            break;
                        }
                        running.state = State::Activating;
                    }
                    "log.append" => {
                        if let Some(message) = note.params["message"].as_str() {
                            notes.push(format!("{label}: {message}"));
                        }
                    }
                    _ => {}
                },
                Some(HostEvent::Garbled(line)) => {
                    notes.push(format!("{label}: unreadable line — {line}"));
                }
                Some(HostEvent::Closed) => {
                    dead = true;
                    break;
                }
                None => break,
            }
        }

        // Ready, so tell it what to load. Separate from the `$/ready` arm so that
        // one iteration of the loop does one thing.
        if running.state == State::Activating && running.asked.is_empty() {
            let Some(extension) = self.catalogue.by_id(id) else {
                return;
            };
            let (Some(main), Some(path)) = (
                extension.main.clone(),
                running.prepared.seen_by_host(&extension.root),
            ) else {
                notes.push(format!("{label}: its directory is not visible to the host"));
                dead = true;
                self.finish(id, dead, notes, statuses, session);
                return;
            };
            match running.host.activate(&path, &main) {
                Ok(id) => {
                    running.asked.insert(id, "activate".to_owned());
                }
                Err(error) => {
                    notes.push(format!("{label}: could not be activated — {error}"));
                    dead = true;
                }
            }
        }

        if !dead && running.state == State::Active && !running.queued.is_empty() {
            let waiting = std::mem::take(&mut running.queued);
            for command in waiting {
                Self::execute(running, session, &command, &label);
            }
        }

        if !dead && running.state != State::Active && running.started.elapsed() > running.timeout {
            notes.push(format!(
                "{label} did not start within {}s; stderr:\n{}",
                running.timeout.as_secs(),
                running.host.errors()
            ));
            dead = true;
        }

        // Asked here rather than inside the drain loop, where the host is
        // borrowed. A question raised by a host that has since died is dropped:
        // there is nothing left to answer.
        if let (Some(asking), false) = (pending, dead) {
            let what = format!(
                "{label} wants to {}",
                describe(&asking.capability, &asking.request.method)
            );
            self.asking = Some(asking);
            session.ask_extension_consent(&what);
        }

        self.finish(id, dead, notes, statuses, session);
    }

    /// Offers every decision made in this session, newest extension first.
    ///
    /// The point is a mistaken answer. A `deny` chosen in a hurry otherwise means
    /// that extension quietly fails for the rest of the session, with nothing to
    /// undo it and no hint that a decision is the reason.
    pub fn offer_permissions(&mut self, session: &mut Session) -> bool {
        self.decisions.clear();
        let mut entries = Vec::new();
        for (id, running) in &self.running {
            let label = self
                .catalogue
                .by_id(id)
                .map(|entry| entry.label.clone())
                .unwrap_or_else(|| id.clone());
            let grants = running.broker.grants();
            for (capability, decided) in grants
                .allowed
                .iter()
                .map(|c| (c, "allowed"))
                .chain(grants.denied.iter().map(|c| (c, "refused")))
            {
                entries.push(deco_editor::commands::PaletteEntry::new(
                    &self.decisions.len().to_string(),
                    &format!("{label}: {decided} — {}", describe(capability, "")),
                ));
                self.decisions.push((id.clone(), capability.clone()));
            }
        }
        // The message is put on the status bar here rather than returned for
        // somebody to forward: this is invoked from a palette entry, whose caller
        // has no outcome to hand onward, so a returned message would be dropped
        // and the command would do nothing visible at all.
        match session.offer_extension_permissions(entries) {
            deco_editor::commands::Outcome::Handled => true,
            deco_editor::commands::Outcome::Message(said) => {
                session.status = Some(said);
                false
            }
            _ => false,
        }
    }

    /// Takes back the decision the user picked out of that list.
    pub fn forget_permission(&mut self, session: &mut Session, chosen: &str) {
        let Some((id, capability)) = chosen
            .parse::<usize>()
            .ok()
            .and_then(|index| self.decisions.get(index))
            .cloned()
        else {
            return;
        };
        let label = self
            .catalogue
            .by_id(&id)
            .map(|entry| entry.label.clone())
            .unwrap_or_else(|| id.clone());
        let Some(running) = self.running.get_mut(&id) else {
            session.status = Some(format!("{label} is not running any more"));
            return;
        };
        running.broker.forget(&capability);
        // Said in the terms the decision was made in, and what happens next: the
        // extension is not re-asked now, because nothing is asking now.
        session.status = Some(format!(
            "{label} will ask again about {}",
            describe(&capability, "")
        ));
        self.note(&format!(
            "{label}: forgot the decision about {}",
            describe(&capability, "")
        ));
    }

    /// Applies the user's answer to the request that was waiting on it.
    ///
    /// The decision is remembered on that extension's broker, so a second request
    /// covered by the same grant is not asked about again — and a refusal is
    /// remembered too, which is what stops an extension asking in a loop.
    pub fn answer_consent(&mut self, session: &mut Session, allow: bool, files: &mut Files<'_>) {
        let Some(asking) = self.asking.take() else {
            return;
        };
        let label = self
            .catalogue
            .by_id(&asking.extension)
            .map(|entry| entry.label.clone())
            .unwrap_or_else(|| asking.extension.clone());
        let Some(running) = self.running.get_mut(&asking.extension) else {
            // The host died while the question was on screen.
            self.note(&format!("{label} was gone by the time you answered"));
            return;
        };
        running.broker.remember(
            asking.capability.clone(),
            if allow {
                Decision::Allow
            } else {
                Decision::Deny
            },
        );

        let mut notes: Vec<String> = Vec::new();
        let mut statuses: Vec<String> = Vec::new();
        // Re-dispatched rather than served directly: the answer is a grant, and
        // whether the grant covers this request is the broker's decision to make
        // a second time. Anything else would let a "yes" to one path serve a
        // request for another.
        let reply = match dispatch(&running.broker, &asking.request) {
            Dispatch::Allowed => Self::mediated(
                running,
                &asking.request,
                &label,
                &mut notes,
                &mut statuses,
                files,
            ),
            Dispatch::Refused(response) => response,
            // Answered and still asking: nothing sensible remains but to refuse,
            // and it would mean the grant did not cover what was asked.
            Dispatch::Consent { .. } => Response::err(
                asking.request.id,
                ErrorCode::PermissionDenied,
                "that permission does not cover this request",
            ),
        };
        let dead = running.host.send(&Message::Response(reply)).is_err();
        notes.push(format!(
            "{label}: {} was {}",
            asking.request.method,
            if allow { "allowed" } else { "refused" }
        ));
        self.finish(&asking.extension, dead, notes, statuses, session);
    }

    /// Records what one poll produced, and drops the host if it is gone.
    fn finish(
        &mut self,
        id: &str,
        dead: bool,
        notes: Vec<String>,
        statuses: Vec<String>,
        session: &mut Session,
    ) {
        for note in notes {
            self.note(&note);
        }
        if let Some(last) = statuses.last() {
            session.status = Some(last.clone());
        }
        if dead {
            if let Some(mut running) = self.running.remove(id) {
                let label = self
                    .catalogue
                    .by_id(id)
                    .map(|e| e.label.clone())
                    .unwrap_or_else(|| id.to_owned());
                let errors = running.host.errors();
                running.host.shutdown();
                if !errors.trim().is_empty() {
                    self.note(&format!("{label} stopped; stderr:\n{errors}"));
                } else {
                    self.note(&format!("{label} stopped"));
                }
                session.status = Some(format!("{label} stopped"));
            }
        }
    }

    /// Handles a request the broker allowed.
    ///
    /// Only the mediated surface is implemented: registering a command, saying
    /// something, and logging. Everything else is refused *by name* rather than
    /// answered with a plausible empty value — an extension told "no" can cope,
    /// while one told "here is your empty list of open editors" cannot.
    fn mediated(
        running: &mut Running,
        request: &deco_ext::protocol::Request,
        label: &str,
        notes: &mut Vec<String>,
        statuses: &mut Vec<String>,
        files: &mut Files<'_>,
    ) -> Response {
        let text = |key: &str| {
            request.params[key]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_default()
        };
        match request.method.as_str() {
            "commands.registerCommand" => {
                let command = text("command");
                if command.is_empty() {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "a command needs an identifier",
                    );
                }
                running.registered.insert(command);
                Response::ok(request.id, serde_json::Value::Null)
            }
            "window.showInformationMessage"
            | "window.showWarningMessage"
            | "window.showErrorMessage"
            | "window.setStatusBarMessage" => {
                let message = text("message");
                if !message.is_empty() {
                    statuses.push(format!("{label}: {message}"));
                }
                // The buttons an extension offered are not answered: there is no
                // way to press one yet, and `undefined` is what VS Code returns
                // when a message is dismissed, so this is a shape extensions
                // already handle.
                Response::ok(request.id, serde_json::Value::Null)
            }
            // Reached only once the broker has allowed it, which is what makes
            // the path safe to act on: the check is that it falls inside a scope
            // the manifest declared and the user did not decline.
            "fs.readFile" => {
                let path = text("path");
                if path.is_empty() {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "a read needs a path",
                    );
                }
                match files.read(&path) {
                    Ok(contents) => Response::ok(request.id, serde_json::json!(contents)),
                    // The operating system's words, not deco's: "no such file" and
                    // "permission denied" are the two answers an extension can do
                    // something about, and paraphrasing them loses which it was.
                    Err(reason) => Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        format!("could not read {path}: {reason}"),
                    ),
                }
            }
            "fs.writeFile" => {
                let path = text("path");
                if path.is_empty() {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "a write needs a path",
                    );
                }
                // `content` is what the shim sends, and an absent one is an empty
                // file rather than an error: truncating is a thing an extension
                // may legitimately mean.
                let contents = text("content");
                match files.write(&path, &contents) {
                    Ok(()) => Response::ok(request.id, serde_json::Value::Null),
                    Err(reason) => Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        format!("could not write {path}: {reason}"),
                    ),
                }
            }
            "log.append" => {
                let message = text("message");
                if !message.is_empty() {
                    notes.push(format!("{label}: {message}"));
                }
                Response::ok(request.id, serde_json::Value::Null)
            }
            // `MethodNotFound` rather than a new code: from the extension's side
            // that is exactly what it is — deco does not answer this method — and
            // an error an extension already handles beats a novel one.
            other => Response::err(
                request.id,
                ErrorCode::MethodNotFound,
                format!("deco does not implement {other} yet"),
            ),
        }
    }

    /// Keeps a line, dropping the oldest once there are too many.
    fn note(&mut self, line: &str) {
        if self.log.len() >= LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line.to_owned());
    }

    /// Stops every host, politely first.
    pub fn shutdown(&mut self) {
        for (_, mut running) in std::mem::take(&mut self.running) {
            running.host.shutdown();
        }
    }
}

impl Drop for Hosts {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One line saying how extensions would be run, for `--print-config`.
///
/// The whole point of refusing to degrade silently is that the answer to "which
/// sandbox am I getting" is always available, and a configuration dump is where
/// someone looks for it.
pub fn sandbox_summary(settings: &deco_config::Settings) -> String {
    match sandbox::policy(settings) {
        Sandbox::Process => "process (no container; asked for explicitly)".to_owned(),
        Sandbox::Container => {
            match sandbox::ContainerConfig::resolve(settings, std::env::var_os("PATH").as_deref()) {
                Ok(container) => format!(
                    "container via {} ({})",
                    container.runtime.display(),
                    container.image
                ),
                // Worth printing rather than hiding: this is the state in which
                // extensions will refuse to start, and this is where someone
                // would look to find out why.
                Err(error) => format!("container — unavailable: {error}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory holding one extension.
    fn install(root: &Path, name: &str, manifest: &str) -> PathBuf {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).expect("a directory");
        std::fs::write(directory.join("package.json"), manifest).expect("a manifest");
        directory
    }

    fn temporary(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "deco-extensions-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a temp directory");
        root
    }

    #[test]
    fn a_missing_extensions_directory_is_not_an_error() {
        // Having installed nothing is the normal case.
        let catalogue = discover(&[PathBuf::from("/nowhere/at/all")]);
        assert!(catalogue.extensions.is_empty());
        assert!(catalogue.problems.is_empty());
    }

    #[test]
    fn extensions_are_found_and_a_directory_that_is_not_one_is_skipped() {
        let root = temporary("found");
        install(
            &root,
            "acme.tools-1.0.0",
            r#"{
  "name": "tools",
  "publisher": "acme",
  "displayName": "Acme Tools",
  "main": "./out/extension.js",
  "contributes": { "commands": [{ "command": "acme.doThing", "title": "Do The Thing" }] }
}"#,
        );
        // Bookkeeping an extensions directory routinely holds.
        std::fs::create_dir_all(root.join(".obsolete")).expect("a directory");

        let catalogue = discover(std::slice::from_ref(&root));
        assert_eq!(catalogue.extensions.len(), 1);
        assert_eq!(catalogue.extensions[0].id, "acme.tools");
        assert!(catalogue.problems.is_empty(), "{:?}", catalogue.problems);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_manifest_that_does_not_parse_is_reported_rather_than_ignored() {
        // Silence here means an extension that is installed and absent, with the
        // reason nowhere.
        let root = temporary("broken");
        install(&root, "acme.broken", "{ this is not json");
        let catalogue = discover(std::slice::from_ref(&root));
        assert!(catalogue.extensions.is_empty());
        assert_eq!(catalogue.problems.len(), 1);
        assert!(catalogue.problems[0].contains("acme.broken"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn what_wins_a_collision_does_not_depend_on_the_filesystems_order() {
        // Two versions of one extension. Sorted by directory name, so the same
        // copy wins on every machine and on every run.
        let root = temporary("twice");
        let manifest = r#"{ "name": "tools", "publisher": "acme", "main": "./m.js" }"#;
        install(&root, "acme.tools-2.0.0", manifest);
        install(&root, "acme.tools-1.0.0", manifest);
        let catalogue = discover(std::slice::from_ref(&root));
        assert_eq!(catalogue.extensions.len(), 1);
        assert!(catalogue.extensions[0].root.ends_with("acme.tools-1.0.0"));
        assert_eq!(catalogue.problems.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_palette_shows_the_extensions_name_beside_its_command() {
        // Which extension a command comes from is what the title does not say and
        // what decides whether you want it.
        let root = temporary("rows");
        install(
            &root,
            "acme.tools",
            r#"{
  "name": "tools",
  "publisher": "acme",
  "displayName": "Acme Tools",
  "main": "./m.js",
  "contributes": {
    "commands": [{ "command": "acme.doThing", "title": "Do The Thing", "category": "Acme" }]
  }
}"#,
        );
        let catalogue = discover(std::slice::from_ref(&root));
        let rows = rows(&catalogue);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "acme.doThing");
        assert_eq!(rows[0].title, "Acme: Do The Thing");
        assert_eq!(rows[0].detail.as_deref(), Some("Acme Tools"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_theme_extension_contributes_no_palette_entries_and_no_host() {
        let root = temporary("theme");
        install(
            &root,
            "someone.theme",
            r#"{
  "name": "theme",
  "publisher": "someone",
  "activationEvents": ["*"],
  "contributes": { "themes": [{ "label": "Midnight", "uiTheme": "vs-dark", "path": "./t.json" }] }
}"#,
        );
        let catalogue = discover(std::slice::from_ref(&root));
        assert_eq!(catalogue.extensions.len(), 1);
        assert!(rows(&catalogue).is_empty());
        assert_eq!(catalogue.code_extensions().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_command_nothing_contributes_is_not_this_modules_business() {
        // The caller reports it as unknown, which is what makes a mistyped
        // keybinding legible instead of being swallowed here.
        let mut hosts = Hosts::empty();
        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );
        assert!(!hosts.run_command(&mut session, "editor.action.commentLine"));
        assert_eq!(hosts.started(), 0);
        assert!(session.status.is_none());
    }

    #[test]
    fn an_extension_whose_host_cannot_be_found_says_so_and_starts_nothing() {
        let root = temporary("nohost");
        install(
            &root,
            "acme.tools",
            r#"{
  "name": "tools",
  "publisher": "acme",
  "displayName": "Acme Tools",
  "main": "./m.js",
  "contributes": { "commands": [{ "command": "acme.doThing", "title": "Do" }] }
}"#,
        );
        let mut hosts = Hosts::new(discover(std::slice::from_ref(&root)));
        // As if deco were installed without its host code.
        hosts.bootstrap = None;
        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );

        // Owned — so the palette entry is this module's to answer — and refused
        // with the reason rather than by doing nothing.
        assert!(hosts.run_command(&mut session, "acme.doThing"));
        assert_eq!(hosts.started(), 0);
        let said = session.status.expect("a reason");
        assert!(said.contains("extension host"), "{said}");
        assert!(said.contains("DECO_HOST_BOOTSTRAP"), "{said}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn process_mode_without_node_refuses_and_names_the_setting() {
        let root = temporary("nonode");
        install(
            &root,
            "acme.tools",
            r#"{
  "name": "tools",
  "publisher": "acme",
  "main": "./m.js",
  "contributes": { "commands": [{ "command": "acme.doThing", "title": "Do" }] }
}"#,
        );
        let mut hosts = Hosts::new(discover(std::slice::from_ref(&root)));
        hosts.bootstrap = Some(root.join("bootstrap.js"));
        hosts.node = None;
        let mut settings = deco_config::Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            sandbox::SANDBOX_KEY,
            serde_json::json!("process"),
        );
        let mut session = Session::new(settings, None, deco_keymap::binding::Platform::Linux);

        assert!(hosts.run_command(&mut session, "acme.doThing"));
        let said = session.status.expect("a reason");
        assert!(said.contains("node"), "{said}");
        assert!(said.contains(sandbox::SANDBOX_KEY), "{said}");
        assert_eq!(hosts.started(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_bootstrap_is_looked_for_beside_the_binary_and_up_the_tree() {
        // A checkout runs `target/debug/deco`; an installed tree has it beside the
        // binary or under `share`. Written as a list so the order is reviewable.
        let exe = if cfg!(windows) {
            PathBuf::from("C:\\src\\deco\\target\\debug\\deco.exe")
        } else {
            PathBuf::from("/src/deco/target/debug/deco")
        };
        let candidates = bootstrap_candidates(&exe);
        assert!(
            candidates.iter().all(|path| path.is_absolute()),
            "{candidates:?}"
        );
        assert!(candidates.iter().any(|path| path
            .ends_with(Path::new("deco/extension-host/src/bootstrap.js"))
            || path.ends_with(Path::new("deco\\extension-host\\src\\bootstrap.js"))));
        assert!(candidates
            .iter()
            .any(|path| path.to_string_lossy().contains(if cfg!(windows) {
                "share\\deco"
            } else {
                "share/deco"
            })));
    }

    #[test]
    fn the_summary_says_which_sandbox_and_why_not_when_there_is_none() {
        let mut settings = deco_config::Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            sandbox::SANDBOX_KEY,
            serde_json::json!("process"),
        );
        assert!(sandbox_summary(&settings).starts_with("process"));

        // The container case depends on what is installed on the machine running
        // the test, so this asserts the shape of both answers rather than one.
        let settings = deco_config::Settings::with_defaults();
        let said = sandbox_summary(&settings);
        assert!(
            said.starts_with("container"),
            "the default should be a container: {said}"
        );
        assert!(
            said.contains("sha256:") || said.contains("unavailable"),
            "either the pinned image or the reason there is none: {said}"
        );
    }
}
