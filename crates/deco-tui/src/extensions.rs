//! Starting installed extensions and handling their requests.
//!
//! This module is in this crate rather than in `deco-editor` for the same reason
//! as the theme list and the file walk: the core has no filesystem access and
//! starts no processes. This module walks the extension directories, makes no
//! decisions (those are in [`deco_ext::catalogue`]), and owns the resulting host
//! processes.
//!
//! # Nothing starts on its own
//!
//! A host is started only when a command belonging to its extension is invoked.
//! The catalogue parses `onLanguage:` and `onStartupFinished`, but they are
//! intentionally not acted on yet. The first version starts a process only when
//! the user requests something, because that limits the impact of mistakes.
//! Opening a Rust file should not start three extensions before this path has
//! been tested in real use.
//!
//! # Starting does not block the editor
//!
//! The first container start on a machine pulls an image, which can take a long
//! time. Waiting for `$/ready` inline would freeze the editor for minutes. A host
//! is therefore started, the invoked command is stored, and both are advanced by
//! [`Hosts::poll`] from the event loop, in the same way as the language-server
//! client.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deco_editor::commands::PaletteEntry;
use deco_editor::Session;
use deco_ext::capability::{Broker, Decision, DefaultPolicy, ResolutionContext};
use deco_ext::catalogue::Catalogue;
use deco_ext::connection::{dispatch, Dispatch, Host, HostEvent};
use deco_ext::host::{HostConfig, HostLimits};
use deco_ext::protocol::{ErrorCode, Message, Response};
use deco_ext::sandbox::{self, Prepared, Sandbox};

/// How many extension directories are examined before the walk stops.
///
/// The same limit as the theme walk, for the same reason: a marketplace-managed
/// directory holds tens of extensions, so the limit only applies to abnormal
/// directories.
pub const MAX_EXTENSIONS: usize = 2_000;

/// How long a host may take to send `$/ready` in each mode.
///
/// Long for a container because the first start on a machine pulls an image,
/// which is slow but not stuck. The status bar shows the current state in both
/// modes.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_TIMEOUT_CONTAINER: Duration = Duration::from_secs(600);

/// How many lines of an extension's own output to keep.
const LOG_LINES: usize = 200;

/// Everything found under `roots`, in the order the roots were given.
///
/// A directory that is not an extension is skipped without a message, because
/// an extensions directory usually contains `.obsolete` and other metadata. A
/// manifest that exists but does not parse is reported, because otherwise the
/// extension would appear to be missing with no visible reason.
pub fn discover(roots: &[PathBuf]) -> Catalogue {
    let mut found: Vec<(PathBuf, deco_ext::Manifest)> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    let mut examined = 0usize;

    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        // Sorted, so the entry chosen on a collision does not depend on the
        // order in which the filesystem returns directories.
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
/// Listed whether or not the extension has started, because invoking a command
/// starts the extension. The detail column shows the extension's name rather
/// than the command identifier the core shows there. The title does not show
/// which extension a command belongs to, and that is often what the user needs
/// to choose.
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
/// Tried in order. A development checkout matches the first; an installed tree
/// matches one of the others. `DECO_HOST_BOOTSTRAP` overrides all of them and is
/// intended for packagers and tests.
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

/// An absolute path to `node`, for the modes that use the local installation.
///
/// Absolute because the host's environment has no `PATH` to search; see
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
    /// Where the extension is mounted, for translating paths in messages.
    prepared: Prepared,
    /// The capability broker for *this* extension. The extension's own
    /// declaration is the upper limit, so there is one broker per host.
    broker: Broker,
    /// The commands the extension registered, used to check whether a command
    /// can be run yet.
    registered: BTreeSet<String>,
    /// Commands invoked before the host was ready, in invocation order.
    queued: Vec<String>,
    /// Requests deco is waiting on, so a reply can be reported with its cause.
    asked: BTreeMap<u64, String>,
}

/// The extensions that are installed, and the hosts running some of them.
/// Where an extension's file requests are served from.
///
/// A remote session's files are on the remote machine, so an extension must
/// read them through the same connection as the editor. This enum prevents
/// reading the same path on the local machine, which would return data from a
/// different checkout without any error.
pub enum Files<'a> {
    /// The local filesystem.
    Here,
    /// The remote side of a remote session.
    Remote(&'a mut deco_remote::Client),
}

impl Files<'_> {
    /// Reads `path`, as text.
    ///
    /// Returns text rather than bytes, and rejects a file that is not UTF-8.
    /// deco's editor rejects such files for the same reason: an extension given
    /// replacement characters would write them back.
    ///
    /// Visible to the crate because the editor's multi-file edits have the same
    /// requirement. A rename that affects a file not open in any tab must read it
    /// from where the files are, which in a remote session is the remote
    /// machine.
    pub(crate) fn read(&mut self, path: &str) -> Result<String, String> {
        match self {
            Self::Here => std::fs::read_to_string(path).map_err(|error| error.to_string()),
            Self::Remote(client) => client.read(path).map_err(|error| error.to_string()),
        }
    }

    /// Creates a directory, and any parent it needs.
    fn create_directory(&mut self, path: &str) -> Result<(), String> {
        match self {
            Self::Here => std::fs::create_dir_all(path).map_err(|error| error.to_string()),
            Self::Remote(client) => client
                .create_directory(path)
                .map_err(|error| error.to_string()),
        }
    }

    /// Removes a path, recursively only when the caller requests it.
    fn delete(&mut self, path: &str, recursive: bool) -> Result<(), String> {
        match self {
            Self::Here => {
                let metadata =
                    std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
                // A link is removed as a link. Following it could delete a
                // target the extension was never granted. Listings report links
                // as links for the same reason.
                if metadata.is_dir() && !metadata.is_symlink() {
                    if recursive {
                        std::fs::remove_dir_all(path)
                    } else {
                        std::fs::remove_dir(path)
                    }
                } else {
                    std::fs::remove_file(path)
                }
                .map_err(|error| error.to_string())
            }
            Self::Remote(client) => client
                .delete(path, recursive)
                .map_err(|error| error.to_string()),
        }
    }

    /// Moves or copies one path to another.
    fn transfer(&mut self, source: &str, target: &str, copy: bool) -> Result<(), String> {
        match self {
            Self::Here => if copy {
                std::fs::copy(source, target).map(|_| ())
            } else {
                std::fs::rename(source, target)
            }
            .map_err(|error| error.to_string()),
            Self::Remote(client) => client
                .transfer(source, target, copy)
                .map_err(|error| error.to_string()),
        }
    }

    /// Metadata for `path` without reading its contents, in VS Code's `FileStat`
    /// shape.
    fn stat(&mut self, path: &str) -> Result<serde_json::Value, String> {
        match self {
            Self::Here => {
                // Uses `symlink_metadata`, so a link is reported as a link.
                // Following it could report on a file in a location the
                // extension was never granted.
                let metadata =
                    std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
                Ok(stat_of(&metadata))
            }
            Self::Remote(client) => client.stat(path).map_err(|error| error.to_string()),
        }
    }

    /// The direct entries of the directory at `path`, in VS Code's format:
    /// pairs of name and kind.
    fn read_directory(&mut self, path: &str) -> Result<Vec<(String, u32)>, String> {
        match self {
            Self::Here => {
                let mut listed = Vec::new();
                for entry in std::fs::read_dir(path).map_err(|error| error.to_string())? {
                    let Ok(entry) = entry else { continue };
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    listed.push((
                        entry.file_name().to_string_lossy().into_owned(),
                        kind_of(&metadata),
                    ));
                }
                // Sorted for the same reason as on the server: `read_dir` does not
                // guarantee an order, and results that change order between calls
                // cannot be compared.
                listed.sort();
                Ok(listed)
            }
            Self::Remote(client) => client
                .read_directory(path)
                .map_err(|error| error.to_string()),
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

/// A user-facing description of what an extension is requesting.
///
/// `method` narrows a capability that covers several operations. A stored
/// decision is described with an empty `method` and gets the capability's full
/// description, because the stored decision covers the whole capability.
///
/// The capability's `Debug` output shows its variant and bound, which is not
/// suitable for a prompt: `ReadFile { scope: Workspace }` is a Rust value, not a
/// question.
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
        // Use the method, not only the capability. `WriteFile` covers writing,
        // creating, deleting and moving. "change files under X" describes a save
        // accurately but understates a delete. The prompt should describe the
        // operation that is about to happen.
        Capability::WriteFile { scope } => match method {
            "fs.delete" => format!("delete files {}", where_(scope)),
            "fs.rename" => format!("move files {}", where_(scope)),
            "fs.copy" => format!("copy files {}", where_(scope)),
            "fs.createDirectory" => format!("create directories {}", where_(scope)),
            _ => format!("change files {}", where_(scope)),
        },
        Capability::Network { host } => format!("connect to {host}"),
        Capability::Process { program } => format!("run {program}"),
        Capability::Env { name } => format!("read the environment variable {name}"),
        Capability::Clipboard => "use the clipboard".to_owned(),
        Capability::Secrets => "store and read secrets".to_owned(),
        Capability::OpenExternal => "open a link in your browser".to_owned(),
        // Unreachable while every variant is listed. If a variant is added, a
        // description that names the method is better than none.
        #[allow(unreachable_patterns)]
        _ => format!("do {method}"),
    }
}

/// The file type in VS Code's numbering.
///
/// `File = 1`, `Directory = 2`, and `SymbolicLink = 64` added to the type of
/// the link target. The remote server uses the same numbering, for the same
/// reason: these values are passed to VS Code's API.
fn kind_of(metadata: &std::fs::Metadata) -> u32 {
    let mut kind = if metadata.is_dir() { 2 } else { 1 };
    if metadata.is_symlink() {
        kind += 64;
    }
    kind
}

/// A local file's stat in the shape VS Code's `FileStat` has.
///
/// Times are milliseconds since the epoch, the unit JavaScript uses. A time the
/// platform does not provide is reported as 0.
fn stat_of(metadata: &std::fs::Metadata) -> serde_json::Value {
    let millis = |time: std::io::Result<std::time::SystemTime>| -> u64 {
        time.ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_millis() as u64)
            .unwrap_or(0)
    };
    serde_json::json!({
        "type": kind_of(metadata),
        "ctime": millis(metadata.created()),
        "mtime": millis(metadata.modified()),
        "size": metadata.len(),
    })
}

/// Messages produced by one poll, collected and applied afterwards.
///
/// One struct because the two lists are always passed together, and passing
/// them separately would make function signatures too long.
#[derive(Default)]
struct Said {
    /// For deco's own record of extension activity.
    notes: Vec<String>,
    /// For the status bar; the last one is shown.
    statuses: Vec<String>,
}

/// A request held while the user is asked about it.
struct Asking {
    /// The extension that made the request.
    extension: String,
    /// The request, answered once there is a decision.
    request: deco_ext::protocol::Request,
    /// The capability the broker needs a decision about.
    capability: deco_ext::capability::Capability,
}

pub struct Hosts {
    catalogue: Catalogue,
    running: BTreeMap<String, Running>,
    /// The host's output and deco's refusals, newest last.
    log: VecDeque<String>,
    /// Messages to show the user once.
    problems: Vec<String>,
    bootstrap: Option<PathBuf>,
    node: Option<PathBuf>,
    /// The file where decisions are stored.
    ///
    /// `None` means decisions are not kept after this session ends. Tests use
    /// this, and deco uses it when it cannot determine its configuration
    /// directory.
    permissions_file: Option<PathBuf>,
    permissions: deco_ext::permissions::Permissions,
    /// The decisions in the most recently offered list, in list order.
    ///
    /// The selected entry is an index into this list rather than a string
    /// encoding of the extension and capability. A string would have to be
    /// parsed back, and a capability's `Debug` output is not meant to be parsed.
    decisions: Vec<(String, deco_ext::capability::Capability)>,
    /// The open permission question, if any.
    ///
    /// Only one at a time, intentionally. Two prompts cannot be on screen at
    /// once, and a queue would ask about requests the extension may have
    /// abandoned long ago. A request from a second extension while one is open
    /// is refused with that reason.
    asking: Option<Asking>,
    /// The workspace folders that a `workspace`-scoped capability refers to.
    ///
    /// Without them a grant of `readFile: workspace` covers no path, because a
    /// scope resolves to the roots it is given and an empty list contains
    /// nothing. In a remote session these are the *remote* directories, so an
    /// extension's paths refer to the same locations as the session's.
    workspace_roots: Vec<PathBuf>,
}

impl Hosts {
    /// Takes a catalogue and locates what is needed to start its extensions.
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
            permissions_file: None,
            permissions: deco_ext::permissions::Permissions::default(),
            decisions: Vec::new(),
            workspace_roots,
        }
    }

    /// Stores decisions in `path`, starting from the decisions already there.
    ///
    /// A file that cannot be read is reported and treated as empty. Refusing to
    /// start the editor because of a damaged permissions file would be worse,
    /// and asking again can be recovered from.
    pub fn remembering(mut self, path: PathBuf) -> Self {
        match deco_ext::permissions::Permissions::load(&path) {
            Ok(permissions) => self.permissions = permissions,
            Err(error) => self.problems.push(format!(
                "extension permissions: {error}; deco will ask again"
            )),
        }
        self.permissions_file = Some(path);
        self
    }

    /// Nothing installed and nothing running.
    pub fn empty() -> Self {
        Self::new(Catalogue::default())
    }

    /// What is installed.
    pub fn catalogue(&self) -> &Catalogue {
        &self.catalogue
    }

    /// Problems found during discovery, for the frontend to show in the same way
    /// as settings problems.
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
    /// Returns `false` when no extension owns the identifier, so the caller can
    /// report it as unknown. A mistyped keybinding must not be ignored by the
    /// extension code.
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
                // Still starting. The command is queued rather than refused, so
                // the user does not have to invoke it again when the host is ready.
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
            // A theme reaching this point means the catalogue and this module
            // disagree about what can run. Report it instead of hiding it.
            return Err(format!("{} has no code to run", extension.label));
        }
        let bootstrap = self.bootstrap.clone().ok_or_else(|| {
            "deco cannot find its own extension host; set DECO_HOST_BOOTSTRAP to \
             extension-host/src/bootstrap.js"
                .to_owned()
        })?;

        let policy = sandbox::policy(&session.settings);
        // In a container the image supplies Node, so a machine without Node can
        // still run extensions. This is the purpose of pinning the runtime.
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
            // The host's code and this extension only. The workspace is not
            // included, because an extension reads files through the broker.
            // Other extensions are not included either.
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
        // Read from the user's setting: `prompt` asks, `allow` grants a declared
        // capability without asking, and `deny` refuses without asking.
        let policy = session
            .settings
            .get(deco_ext::capability::DEFAULT_POLICY_KEY)
            .and_then(|value| value.as_str())
            .and_then(DefaultPolicy::parse)
            .unwrap_or(DefaultPolicy::Prompt);
        // Earlier decisions, if they were made for this version. After an update
        // the user is intentionally asked again; see `deco_ext::permissions`.
        let version = self
            .catalogue
            .by_id(id)
            .map(|entry| entry.version.clone())
            .unwrap_or_default();
        let remembered = self
            .permissions
            .for_extension(id, &version)
            .cloned()
            .unwrap_or_default();
        if let Some(older) = self.permissions.stale_for(id, &version) {
            // Reported explicitly, because from the prompt alone "deco lost my
            // answer" and "this extension changed since then" look the same.
            self.note(&format!(
                "{id} was decided about at {older} and is now {version}, so its permissions are \
                 being asked again"
            ));
        }
        let broker = Broker::new(
            self.declared(id),
            remembered,
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

    /// The capabilities declared in an extension's manifest.
    ///
    /// Read from the manifest again rather than stored in the catalogue. This is
    /// the upper limit the broker enforces, and fewer copies mean fewer places
    /// where it can be wrong.
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
            // reports "command not found"; naming the extension is more useful.
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
    /// Called from the event loop, like the language-server client, because
    /// nothing else would detect that a host started earlier has become ready.
    pub fn poll(&mut self, session: &mut Session, files: &mut Files<'_>, now_ms: u64) {
        let ids: Vec<String> = self.running.keys().cloned().collect();
        for id in ids {
            self.poll_one(&id, session, files, now_ms);
        }
    }

    /// `now_ms` is the editor's clock, recorded in the undo step of an applied
    /// edit. It is the same value a keystroke would use.
    fn poll_one(&mut self, id: &str, session: &mut Session, files: &mut Files<'_>, now_ms: u64) {
        let mut reported = Said::default();
        let mut dead = false;
        // Collected rather than stored directly, because the host being drained
        // is borrowed from `self` for the duration of this loop.
        let mut pending: Option<Asking> = None;

        let Some(running) = self.running.get_mut(id) else {
            return;
        };
        let label = self
            .catalogue
            .by_id(id)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| id.to_owned());

        // A fixed budget rather than draining everything, so an extension that
        // logs in a loop cannot block the event loop.
        for _ in 0..64 {
            match running.host.poll() {
                Some(HostEvent::Message(Message::Request(request))) => {
                    let reply = match dispatch(&running.broker, &request) {
                        Dispatch::Refused(response) => {
                            reported.notes.push(format!(
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
                        // deco cannot ask the user yet, and a missing prompt must
                        // not be treated as consent. The request is refused and
                        // the reason is logged, so the extension's behaviour can
                        // be explained.
                        // Held rather than answered: the extension is waiting on
                        // a promise, so nothing is sent until there is a
                        // decision. Its host keeps running and its other requests
                        // are still served.
                        Dispatch::Consent { capability } if pending.is_none() => {
                            pending = Some(Asking {
                                extension: id.to_owned(),
                                request: request.clone(),
                                capability,
                            });
                            continue;
                        }
                        // A second question while one is open. Refused rather than
                        // queued, with a reason that describes the situation. A
                        // queue could ask about a request the extension abandoned
                        // long before the prompt was shown.
                        Dispatch::Consent { capability } => {
                            reported.notes.push(format!(
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
                            &mut reported,
                            files,
                            session,
                            now_ms,
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
                            reported
                                .statuses
                                .push(format!("{label}: {what} failed — {}", error.message));
                        }
                        (Some("$/activate"), None) => {
                            running.state = State::Active;
                        }
                        (Some("$/executeCommand"), None) => {
                            // The extension's return value. Shown only when it is
                            // a string, because the status bar can only display
                            // text. Other values would be for the caller, and
                            // there is no caller yet.
                            let said = response
                                .result
                                .as_ref()
                                .and_then(|value| value.as_str())
                                .map(str::to_owned);
                            let what = asked.unwrap_or_else(|| "a command".to_owned());
                            reported.statuses.push(match said {
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
                            reported.notes.push(format!("{label}: {error}"));
                            dead = true;
                            break;
                        }
                        running.state = State::Activating;
                    }
                    "log.append" => {
                        if let Some(message) = note.params["message"].as_str() {
                            reported.notes.push(format!("{label}: {message}"));
                        }
                    }
                    _ => {}
                },
                Some(HostEvent::Garbled(line)) => {
                    reported
                        .notes
                        .push(format!("{label}: unreadable line — {line}"));
                }
                Some(HostEvent::Closed) => {
                    dead = true;
                    break;
                }
                None => break,
            }
        }

        // Ready, so send the extension to load. Separate from the `$/ready` arm
        // so that each loop iteration does one thing.
        if running.state == State::Activating && running.asked.is_empty() {
            let Some(extension) = self.catalogue.by_id(id) else {
                return;
            };
            let (Some(main), Some(path)) = (
                extension.main.clone(),
                running.prepared.seen_by_host(&extension.root),
            ) else {
                reported
                    .notes
                    .push(format!("{label}: its directory is not visible to the host"));
                dead = true;
                self.finish(id, dead, reported, session);
                return;
            };
            match running.host.activate(&path, &main) {
                Ok(id) => {
                    running.asked.insert(id, "activate".to_owned());
                }
                Err(error) => {
                    reported
                        .notes
                        .push(format!("{label}: could not be activated — {error}"));
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
            reported.notes.push(format!(
                "{label} did not start within {}s; stderr:\n{}",
                running.timeout.as_secs(),
                running.host.errors()
            ));
            dead = true;
        }

        // Asked here rather than inside the drain loop, where the host is
        // borrowed. A question from a host that has since exited is dropped,
        // because there is no request left to answer.
        if let (Some(asking), false) = (pending, dead) {
            let what = format!(
                "{label} wants to {}",
                describe(&asking.capability, &asking.request.method)
            );
            self.asking = Some(asking);
            session.ask_extension_consent(&what);
        }

        self.finish(id, dead, reported, session);
    }

    /// Offers every decision made in this session, newest extension first.
    ///
    /// This allows a mistaken answer to be undone. Otherwise an accidental `deny`
    /// makes the extension fail for the rest of the session, with no way to undo
    /// it and no indication that a decision is the cause.
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
        // The message is set on the status bar here rather than returned. This is
        // invoked from a palette entry whose caller does not forward an outcome,
        // so a returned message would be dropped and the command would have no
        // visible effect.
        match session.offer_extension_permissions(entries) {
            deco_editor::commands::Outcome::Handled => true,
            deco_editor::commands::Outcome::Message(said) => {
                session.status = Some(said);
                false
            }
            _ => false,
        }
    }

    /// Revokes the decision the user selected from that list.
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
        self.write_down(&id);
        // Describe the decision in the same terms as the original prompt, and
        // what happens next. The user is not asked again now, because no request
        // is pending.
        session.status = Some(format!(
            "{label} will ask again about {}",
            describe(&capability, "")
        ));
        self.note(&format!(
            "{label}: forgot the decision about {}",
            describe(&capability, "")
        ));
    }

    /// Saves the current decisions for `id`, if decisions are being saved.
    ///
    /// Saved after every change rather than at shutdown. An editor is often
    /// killed rather than exited cleanly, and a decision saved only on a clean
    /// exit would be asked again without a visible reason.
    fn write_down(&mut self, id: &str) {
        let Some(path) = self.permissions_file.clone() else {
            return;
        };
        let Some(running) = self.running.get(id) else {
            return;
        };
        let grants = running.broker.grants().clone();
        let version = self
            .catalogue
            .by_id(id)
            .map(|entry| entry.version.clone())
            .unwrap_or_default();
        self.permissions.set(id, &version, grants);
        if let Err(error) = self.permissions.save(&path) {
            // Reported rather than ignored. The decision still applies for this
            // session; it will only be asked again in a later session.
            self.note(&format!(
                "extension permissions: {error}; this session's answers still hold"
            ));
        }
    }

    /// Applies the user's answer to the request that was waiting on it.
    ///
    /// The decision is stored in that extension's broker, so a later request
    /// covered by the same grant is not asked again. A refusal is also stored,
    /// which prevents an extension from asking in a loop.
    pub fn answer_consent(
        &mut self,
        session: &mut Session,
        allow: bool,
        files: &mut Files<'_>,
        now_ms: u64,
    ) {
        let Some(asking) = self.asking.take() else {
            return;
        };
        let label = self
            .catalogue
            .by_id(&asking.extension)
            .map(|entry| entry.label.clone())
            .unwrap_or_else(|| asking.extension.clone());
        let Some(running) = self.running.get_mut(&asking.extension) else {
            // The host exited while the question was on screen.
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
        self.write_down(&asking.extension);
        let Some(running) = self.running.get_mut(&asking.extension) else {
            return;
        };

        let mut reported = Said::default();
        // Dispatched again rather than served directly. The answer is a grant,
        // and the broker checks again whether the grant covers this request.
        // Otherwise a "yes" for one path could serve a request for another.
        let reply = match dispatch(&running.broker, &asking.request) {
            Dispatch::Allowed => Self::mediated(
                running,
                &asking.request,
                &label,
                &mut reported,
                files,
                session,
                now_ms,
            ),
            Dispatch::Refused(response) => response,
            // Still needs consent after the answer, which means the grant does
            // not cover the request. Refuse it.
            Dispatch::Consent { .. } => Response::err(
                asking.request.id,
                ErrorCode::PermissionDenied,
                "that permission does not cover this request",
            ),
        };
        let dead = running.host.send(&Message::Response(reply)).is_err();
        reported.notes.push(format!(
            "{label}: {} was {}",
            asking.request.method,
            if allow { "allowed" } else { "refused" }
        ));
        self.finish(&asking.extension, dead, reported, session);
    }

    /// Records the output of one poll, and drops the host if it has exited.
    fn finish(&mut self, id: &str, dead: bool, said: Said, session: &mut Session) {
        for note in said.notes {
            self.note(&note);
        }
        if let Some(last) = said.statuses.last() {
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
    /// Only the mediated surface is implemented: registering a command, showing a
    /// message, and logging. Every other method is refused *by name* rather than
    /// answered with an empty value. An extension can handle a refusal, but not
    /// an incorrect empty result such as an empty list of open editors.
    fn mediated(
        running: &mut Running,
        request: &deco_ext::protocol::Request,
        label: &str,
        said: &mut Said,
        files: &mut Files<'_>,
        session: &mut Session,
        now_ms: u64,
    ) -> Response {
        let (notes, statuses) = (&mut said.notes, &mut said.statuses);
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
                // Message buttons are not supported yet, so the response is
                // `undefined`. VS Code returns the same value when a message is
                // dismissed, so extensions already handle it.
                Response::ok(request.id, serde_json::Value::Null)
            }
            // Reached only after the broker has allowed the request. The broker
            // checks that the path is inside a scope the manifest declared and
            // the user did not decline, so the path is safe to use.
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
                    // Pass through the operating system's message. "no such file"
                    // and "permission denied" are the errors an extension can act
                    // on, and rewording them would hide which one occurred.
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
                // The shim sends `content`. A missing value writes an empty file
                // rather than returning an error, because an extension may intend
                // to truncate the file.
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
            "fs.stat" => {
                let path = text("path");
                if path.is_empty() {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "a stat needs a path",
                    );
                }
                match files.stat(&path) {
                    Ok(stat) => Response::ok(request.id, stat),
                    Err(reason) => Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        format!("could not stat {path}: {reason}"),
                    ),
                }
            }
            "fs.readDirectory" => {
                let path = text("path");
                if path.is_empty() {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "a listing needs a path",
                    );
                }
                match files.read_directory(&path) {
                    // Pairs of name and kind, the same result as VS Code's
                    // `readDirectory`, so extensions written for VS Code can use
                    // it unchanged.
                    Ok(entries) => Response::ok(
                        request.id,
                        serde_json::json!(entries
                            .into_iter()
                            .map(|(name, kind)| serde_json::json!([name, kind]))
                            .collect::<Vec<_>>()),
                    ),
                    Err(reason) => Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        format!("could not list {path}: {reason}"),
                    ),
                }
            }
            "fs.createDirectory" => {
                let path = text("path");
                if path.is_empty() {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "a directory needs a path",
                    );
                }
                match files.create_directory(&path) {
                    Ok(()) => Response::ok(request.id, serde_json::Value::Null),
                    Err(reason) => Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        format!("could not create {path}: {reason}"),
                    ),
                }
            }
            "fs.delete" => {
                let path = text("path");
                if path.is_empty() {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "a delete needs a path",
                    );
                }
                let options = &request.params["options"];
                // deco has no trash, so the request is refused rather than
                // performed as a permanent delete. The extension asked for a
                // recoverable delete, and a permanent one would appear to
                // succeed.
                if options["useTrash"].as_bool() == Some(true) {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "deco has no trash, and will not delete permanently instead",
                    );
                }
                match files.delete(&path, options["recursive"].as_bool().unwrap_or(false)) {
                    Ok(()) => Response::ok(request.id, serde_json::Value::Null),
                    Err(reason) => Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        format!("could not delete {path}: {reason}"),
                    ),
                }
            }
            "fs.rename" | "fs.copy" => {
                let source = text("source");
                let target = text("target");
                if source.is_empty() || target.is_empty() {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "a move needs a source and a target",
                    );
                }
                // The broker checked the *target*, because a request carries
                // only one capability. The source is also written, because moving
                // a file out of a directory changes that directory. It is checked
                // here and must already be granted: a second question cannot be
                // asked while this request is held for the first.
                let wanted = deco_ext::capability::Capability::WriteFile {
                    scope: deco_ext::capability::PathScope::Subtree {
                        path: PathBuf::from(&source),
                    },
                };
                if running.broker.check(&wanted) != deco_ext::capability::CheckResult::Allowed {
                    notes.push(format!(
                        "{label}: refused {} — {source} is not covered",
                        request.method
                    ));
                    return Response::err(
                        request.id,
                        ErrorCode::PermissionDenied,
                        format!("{source} is outside every granted scope"),
                    );
                }
                match files.transfer(&source, &target, request.method == "fs.copy") {
                    Ok(()) => Response::ok(request.id, serde_json::Value::Null),
                    Err(reason) => Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        format!("could not move {source} to {target}: {reason}"),
                    ),
                }
            }
            // An edit applied through the editor rather than directly to the
            // file. The broker has already checked it as a write.
            "workspace.applyEdit" => {
                let path = text("path");
                if path.is_empty() {
                    return Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        "an edit needs a path",
                    );
                }
                let edits = deco_lsp::TextEdit::list_from_json(&request.params["edits"]);
                if edits.is_empty() {
                    // No edits is a success. An extension that computed no
                    // changes has not failed.
                    return Response::ok(request.id, serde_json::json!(true));
                }
                // Apply to the open document first, if the path is open. Writing
                // the file instead would be overwritten by the next save of the
                // buffer, which does not contain the edit, and the edit would be
                // lost without an error.
                if let Some(applied) = session.apply_edits_to_path(Path::new(&path), &edits, now_ms)
                {
                    return match applied {
                        Ok(count) => {
                            statuses.push(format!("{label}: {count} edits applied to {path}"));
                            Response::ok(request.id, serde_json::json!(true))
                        }
                        Err(error) => Response::err(
                            request.id,
                            ErrorCode::InvalidParams,
                            format!("could not edit {path}: {error}"),
                        ),
                    };
                }
                // Not open, so edit the file directly: read, apply, write. All
                // three use the same `Files`, so a remote session edits the file
                // on the remote machine.
                let edited = files.read(&path).and_then(|text| {
                    let mut buffer = deco_core::buffer::Buffer::from_text(&text);
                    let changes: Vec<deco_core::Change> = edits
                        .iter()
                        .filter(|edit| !edit.is_noop())
                        .map(|edit| {
                            deco_core::Change::replace(
                                deco_core::position::Range::new(
                                    buffer.clamp_position(edit.range.start),
                                    buffer.clamp_position(edit.range.end),
                                ),
                                edit.new_text.clone(),
                            )
                        })
                        .collect();
                    if changes.is_empty() {
                        return Ok(None);
                    }
                    // Same rule as the editor: overlapping edits have no defined
                    // result, and guessing could corrupt the file without an error.
                    let transaction = deco_core::Transaction::new(changes)
                        .map_err(|_| "the edits overlap".to_owned())?;
                    buffer.apply(&transaction);
                    Ok(Some(buffer.text()))
                });
                match edited {
                    Ok(None) => Response::ok(request.id, serde_json::json!(true)),
                    Ok(Some(text)) => match files.write(&path, &text) {
                        Ok(()) => {
                            statuses.push(format!("{label}: edited {path}"));
                            Response::ok(request.id, serde_json::json!(true))
                        }
                        Err(reason) => Response::err(
                            request.id,
                            ErrorCode::InvalidParams,
                            format!("could not write {path}: {reason}"),
                        ),
                    },
                    Err(reason) => Response::err(
                        request.id,
                        ErrorCode::InvalidParams,
                        format!("could not edit {path}: {reason}"),
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
            // `MethodNotFound` rather than a new code. For the extension, deco
            // does not implement this method, and extensions already handle this
            // error.
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

    /// Stops every host, requesting a graceful shutdown first.
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
/// deco does not fall back to a weaker sandbox without notice, so the active
/// sandbox must always be discoverable. The configuration dump is where users
/// look for it.
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
                // Printed because in this state extensions will not start, and
                // this output is where users look for the reason.
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
        // Metadata that an extensions directory usually contains.
        std::fs::create_dir_all(root.join(".obsolete")).expect("a directory");

        let catalogue = discover(std::slice::from_ref(&root));
        assert_eq!(catalogue.extensions.len(), 1);
        assert_eq!(catalogue.extensions[0].id, "acme.tools");
        assert!(catalogue.problems.is_empty(), "{:?}", catalogue.problems);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_manifest_that_does_not_parse_is_reported_rather_than_ignored() {
        // Without a report, the installed extension would appear to be missing
        // with no visible reason.
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
        // The title does not show which extension a command belongs to, and that
        // is often what the user needs to choose.
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
        // The caller reports it as unknown, so a mistyped keybinding is reported
        // instead of being ignored here.
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
        // Simulates a deco installation without its host code.
        hosts.bootstrap = None;
        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            deco_keymap::binding::Platform::Linux,
        );

        // The command is owned by an extension, so this module handles it, and
        // it is refused with a reason rather than ignored.
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
        // A checkout runs `target/debug/deco`; an installed tree has the host
        // beside the binary or under `share`. Candidates are a list so the order
        // is reviewable.
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

        // The container result depends on what is installed on the test machine,
        // so this accepts either possible result.
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
