//! The world a scenario runs in: a home directory, a workspace, and the files
//! in both of them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use deco_config::paths::{ConfigPaths, Env, Layout};
use deco_keymap::binding::Platform;

use crate::editor::Editor;

/// Distinguishes two scenarios built in the same process, so that tests running
/// on different threads cannot land in the same directory.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A machine deco is about to start on.
///
/// Everything is under one temporary directory: `home/` holds the configuration
/// deco and VS Code would read, and `work/` is the folder the files are in. The
/// directory is removed when the scenario is dropped — unless the test failed, in
/// which case it is left behind and its path printed, because the fastest way to
/// understand a failing end-to-end test is to look at what it built.
pub struct Scenario {
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    layout: Layout,
    platform: Platform,
    size: (u16, u16),
    /// Written at launch rather than when it is set, because the harness has a
    /// key of its own to put in front of it — see [`Scenario::language_servers`].
    user_settings: Option<String>,
    /// Written at launch for the same reason, and to the same rule: a scenario
    /// about deco reading VS Code's file has to have VS Code's file be the one
    /// the harness's own keys are in, or writing them would create the deco file
    /// whose absence is the thing being tested.
    vscode_settings: Option<String>,
    language_servers: bool,
    /// A workspace for the far end that is not this machine's, once a scenario
    /// has put a file on it — see [`Scenario::remote_file`].
    remote: Option<PathBuf>,
    /// The file the far end serves as its own machine settings, if a scenario
    /// set one — see [`Scenario::remote_machine_settings`].
    machine_settings: Option<PathBuf>,
}

impl Scenario {
    /// A new, empty machine. `name` only has to be unique enough to recognise in
    /// a directory listing.
    pub fn new(name: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("deco-e2e-{name}-{}-{unique}", std::process::id()));
        // A previous run that was killed rather than dropped leaves its
        // directory behind, and a scenario starting inside one would inherit
        // files it never wrote.
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let workspace = root.join("work");
        std::fs::create_dir_all(&home).expect("a home directory");
        std::fs::create_dir_all(&workspace).expect("a workspace");
        Self {
            root,
            home,
            workspace,
            // Not `Layout::host()`. A scenario asserting about where settings are
            // read from would otherwise assert something different on each
            // platform, and the three layouts are all reachable from any of them.
            layout: Layout::Xdg,
            platform: Platform::Linux,
            size: (80, 24),
            user_settings: None,
            vscode_settings: None,
            language_servers: false,
            remote: None,
            machine_settings: None,
        }
    }

    // ---- what kind of machine this is -------------------------------------

    /// Which platform's configuration directory layout applies.
    pub fn layout(mut self, layout: Layout) -> Self {
        self.layout = layout;
        self
    }

    /// Which platform's keybindings win — `key` against `mac`, and the
    /// `isMac`-style context keys.
    pub fn platform(mut self, platform: Platform) -> Self {
        self.platform = platform;
        self
    }

    /// The terminal size, in cells. The default is 80×24.
    pub fn size(mut self, width: u16, height: u16) -> Self {
        self.size = (width, height);
        self
    }

    // ---- what is on it ----------------------------------------------------

    /// Writes a file into the workspace, creating whatever directories it needs.
    pub fn file(self, relative: &str, contents: &str) -> Self {
        let path = self.workspace.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a directory for the file");
        }
        std::fs::write(&path, contents).expect("a file");
        self
    }

    /// Writes a file as raw bytes, for the contents a `&str` cannot hold — a
    /// UTF-8 BOM, a lone CR, a file that is not valid UTF-8 at all.
    pub fn bytes(self, relative: &str, contents: &[u8]) -> Self {
        let path = self.workspace.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a directory for the file");
        }
        std::fs::write(&path, contents).expect("a file");
        self
    }

    /// deco's own `settings.json`.
    pub fn user_settings(mut self, json: &str) -> Self {
        self.user_settings = Some(json.to_owned());
        self
    }

    /// Lets this machine start language servers.
    ///
    /// Off by default, and the default is the point. A scenario is meant to say
    /// the same thing on every machine it runs on, and a machine with
    /// `rust-analyzer` installed is a different machine from one without: the
    /// status line carries a server's message, a completion list opens on a `.`,
    /// and a scenario about saving a file starts failing because of something it
    /// never mentioned. So the machine a scenario gets is one with no language
    /// servers installed, expressed the way a user would express it —
    /// `"deco.lsp.enabled": false` in `settings.json`.
    ///
    /// Turn it on to write a scenario about the language-server path, and give
    /// the machine a server it can really run through `deco.lsp.servers`.
    pub fn language_servers(mut self, enabled: bool) -> Self {
        self.language_servers = enabled;
        self
    }

    /// Installs a language server for `language` that this scenario can rely on.
    ///
    /// The server is [`examples/language_server.rs`], a real program on a real
    /// pipe speaking real LSP — not a stub the editor is handed. `role` is
    /// `argv[1]` and selects what it offers; `"full"` answers everything.
    ///
    /// Written the way a user writes it, into `deco.lsp.servers`, so the
    /// configuration path is on the way in too. Turns [`Scenario::language_servers`]
    /// on, because a machine with a server on it is one where they are enabled.
    ///
    /// [`examples/language_server.rs`]: https://github.com/sabas0ba/deco/blob/main/crates/deco-e2e/examples/language_server.rs
    pub fn language_server(mut self, language: &str, role: &str) -> Self {
        self.language_servers = true;
        let program = fake_server();
        // Through `serde_json` rather than `format!`: on Windows the path is
        // full of backslashes, every one of which has to be escaped to survive
        // being read back as JSON.
        let definition = serde_json::json!({
            "deco.lsp.servers": {
                "fake": {
                    "languages": [language],
                    "command": program.to_string_lossy(),
                    "args": [role],
                },
            },
        });
        let text = serde_json::to_string_pretty(&definition).expect("serialisable");
        // Spliced into whatever the scenario already asked for, so a scenario can
        // have both a server and settings of its own.
        self.user_settings = Some(match self.user_settings.take() {
            Some(existing) => splice(&text, &existing),
            None => text,
        });
        self
    }

    /// Writes a file onto the far end, in a directory this machine's workspace
    /// is not.
    ///
    /// Without this, [`Scenario::launch_remote`] serves the scenario's own
    /// workspace, and "the far end" and "this machine" are one directory — which
    /// means a scenario cannot tell a file that came over the connection from
    /// one that was read off the local disk, and cannot tell a write that went to
    /// the server from a write that went here. Both look identical when the two
    /// are the same folder.
    ///
    /// Using this makes them different folders, so those questions have answers.
    /// [`Editor::on_disk`] then looks at the far end's, because that is where a
    /// remote session's files are.
    pub fn remote_file(mut self, relative: &str, contents: &str) -> Self {
        let remote = self.root.join("remote");
        let path = remote.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a directory on the far end");
        }
        std::fs::write(&path, contents).expect("a file on the far end");
        self.remote = Some(remote);
        self
    }

    /// The directory the far end serves: its own if a scenario gave it one, and
    /// otherwise this machine's workspace.
    pub(crate) fn served_workspace(&self) -> PathBuf {
        self.remote
            .clone()
            .unwrap_or_else(|| self.workspace.clone())
    }

    /// The far end's own `machine-settings.json`.
    ///
    /// The settings a *machine* has, as opposed to the ones a person has: what
    /// a session connected to that machine picks up as its `remote` layer.
    /// Written into the scenario's directory and named to the server on its
    /// command line, because the server is a separate process and a test cannot
    /// change its environment without changing every other test's too.
    pub fn remote_machine_settings(mut self, json: &str) -> Self {
        let path = self.root.join("remote-machine-settings.json");
        write_config(&path, json);
        self.machine_settings = Some(path);
        self
    }

    /// deco's own `keybindings.json`.
    pub fn user_keybindings(self, json: &str) -> Self {
        let path = self.deco_paths().keybindings;
        write_config(&path, json);
        self
    }

    /// VS Code's `settings.json`, which deco reads when it has none of its own.
    pub fn vscode_settings(mut self, json: &str) -> Self {
        self.vscode_settings = Some(json.to_owned());
        self
    }

    /// VS Code's `keybindings.json`, read under the same rule.
    pub fn vscode_keybindings(self, json: &str) -> Self {
        let path = self.vscode_paths().keybindings;
        write_config(&path, json);
        self
    }

    /// The workspace's `.vscode/settings.json`.
    pub fn workspace_settings(self, json: &str) -> Self {
        let path = self.workspace.join(".vscode").join("settings.json");
        write_config(&path, json);
        self
    }

    /// The workspace's `.deco/settings.json`, which shadows `.vscode`.
    pub fn deco_workspace_settings(self, json: &str) -> Self {
        let path = self.workspace.join(".deco").join("settings.json");
        write_config(&path, json);
        self
    }

    /// Installs a theme extension into deco's extensions directory, the way a
    /// marketplace download would land there.
    ///
    /// `id` is the directory name, which is `publisher.name-version` for anything
    /// installed by VS Code. `label` is the theme's name as the picker will show
    /// it, and `theme` is the theme file's own JSON.
    pub fn theme_extension(self, id: &str, label: &str, theme: &str) -> Self {
        let directory = self.deco_paths().extensions.join(id);
        std::fs::create_dir_all(&directory).expect("an extension directory");
        let manifest = format!(
            r#"{{
  "name": "{id}",
  "version": "1.0.0",
  "engines": {{ "vscode": "^1.0.0" }},
  "contributes": {{
    "themes": [
      {{ "label": "{label}", "uiTheme": "vs-dark", "path": "./themes/theme.json" }}
    ]
  }}
}}"#
        );
        std::fs::write(directory.join("package.json"), manifest).expect("a manifest");
        std::fs::create_dir_all(directory.join("themes")).expect("a themes directory");
        std::fs::write(directory.join("themes").join("theme.json"), theme).expect("a theme");
        self
    }

    // ---- starting it ------------------------------------------------------

    /// Starts deco with `args`, exactly as they would be typed after the program
    /// name. Relative paths are taken against the workspace, which is where the
    /// shell would have been.
    ///
    /// Panics if the arguments do not parse: a scenario that mistypes a flag is a
    /// broken test rather than a finding, and [`Scenario::usage_error`] is how a
    /// scenario asserts about a command line that should be refused.
    pub fn launch(&self, args: &[&str]) -> Editor {
        self.write_user_settings();
        let cli = match deco::cli::parse(args.iter().map(|arg| arg.to_string())) {
            Ok(deco::cli::Outcome::Run(cli)) => *cli,
            Ok(other) => panic!("`deco {}` did not ask to run: {other:?}", args.join(" ")),
            Err(error) => panic!("`deco {}` did not parse: {error}", args.join(" ")),
        };
        Editor::start(self, cli)
            .unwrap_or_else(|error| panic!("`deco {}` did not start: {error:#}", args.join(" ")))
    }

    /// The same, with the workspace served by a real `deco --server` process.
    ///
    /// The one substitution: `ssh host` is not in front of the server command.
    /// What that leaves out is an argument vector, tested where it is built; what
    /// it keeps is everything a remote session actually depends on — a second
    /// process, a framed protocol over its stdio, documents keyed by paths
    /// relative to the far end's workspace, and a server that refuses anything
    /// outside it.
    ///
    /// The authority is a real one so that language servers resolve the way they
    /// would in a session: they are wrapped in a transport, and a scenario with
    /// no `docker` on it sees that reported rather than silently running one
    /// here.
    /// `server_binary` is a `deco` to run as the far end. It is a parameter
    /// rather than something this works out for itself because `CARGO_BIN_EXE_*`
    /// is only defined for tests of the package that builds the binary, and a
    /// harness guessing at a path under `target/` would be a different kind of
    /// wrong.
    pub fn launch_remote(&self, args: &[&str], server_binary: &std::path::Path) -> Editor {
        self.write_user_settings();
        let cli = match deco::cli::parse(args.iter().map(|arg| arg.to_string())) {
            Ok(deco::cli::Outcome::Run(cli)) => *cli,
            Ok(other) => panic!("`deco {}` did not ask to run: {other:?}", args.join(" ")),
            Err(error) => panic!("`deco {}` did not parse: {error}", args.join(" ")),
        };

        let command = deco_remote::transport::Command {
            program: server_binary.display().to_string(),
            args: {
                let mut args = vec![
                    "--server".to_owned(),
                    "--stdio".to_owned(),
                    "--workspace".to_owned(),
                    self.served_workspace().display().to_string(),
                ];
                if let Some(path) = &self.machine_settings {
                    args.push("--machine-settings".to_owned());
                    args.push(path.display().to_string());
                }
                args
            },
        };
        let mut client = deco_remote::Client::start(&command)
            .unwrap_or_else(|error| panic!("the server should start: {error}"));
        let hello = client
            .handshake()
            .unwrap_or_else(|error| panic!("the server should answer: {error}"));
        let mut scm = deco_remote::Client::start(&command)
            .unwrap_or_else(|error| panic!("the SCM server should start: {error}"));
        scm.handshake()
            .unwrap_or_else(|error| panic!("the SCM server should answer: {error}"));

        let remote = deco_tui::RemoteSession {
            client,
            scm: Some(scm),
            location: deco_tui::lsp::Location::Remote {
                authority: deco_remote::Authority::parse("attached-container+scenario")
                    .expect("an authority"),
                options: deco_remote::TransportOptions::default(),
                // As the *server* spells it, which is what every path in the
                // session is relative to.
                workspace: std::path::PathBuf::from(hello.workspace),
            },
        };
        Editor::start_with(self, cli, Some(remote))
            .unwrap_or_else(|error| panic!("`deco {}` did not start: {error:#}", args.join(" ")))
    }

    /// Why `deco` refused to start.
    ///
    /// Startup can fail for reasons a command line cannot be blamed for — a file
    /// that is really a directory, a file the account cannot read — and what the
    /// person in the terminal sees then is this message and no editor.
    pub fn startup_error(&self, args: &[&str]) -> String {
        self.write_user_settings();
        let cli = match deco::cli::parse(args.iter().map(|arg| arg.to_string())) {
            Ok(deco::cli::Outcome::Run(cli)) => *cli,
            Ok(other) => panic!("`deco {}` did not ask to run: {other:?}", args.join(" ")),
            Err(error) => return error.to_string(),
        };
        match Editor::start(self, cli) {
            Err(error) => format!("{error:#}"),
            Ok(_) => panic!("`deco {}` started", args.join(" ")),
        }
    }

    /// Writes the `settings.json` a launch will read.
    ///
    /// The harness has two keys of its own, and they go into whichever file the
    /// scenario is exercising: deco's if the scenario wrote one, VS Code's if it
    /// wrote only that. Putting them in deco's file unconditionally would create
    /// the very file a scenario about reading VS Code's is asserting is absent.
    ///
    /// They go in first, so that anything the scenario wrote comes later and
    /// therefore wins — the rule the JSONC layer already applies to a repeated
    /// key.
    fn write_user_settings(&self) {
        let mut defaults: Vec<&str> = Vec::new();
        if !self.language_servers {
            defaults.push(
                "    // deco-e2e: this machine has no language servers installed.\n    \"deco.lsp.enabled\": false",
            );
        }
        // Deliberately *not* pinning `files.eol` here, tempting as it is. A new
        // file's ending depends on the platform, so a scenario asserting the bytes
        // of a file it created has to say which ending it expects — but setting
        // the key is not a way to make that go away, because in deco an explicit
        // `files.eol` also converts the ending of every *existing* file that is
        // opened. A harness default would silently change what those scenarios
        // were testing. Each scenario that creates a file says so for itself.

        match (&self.user_settings, &self.vscode_settings) {
            (Some(own), vscode) => {
                write_config(
                    &self.deco_paths().settings,
                    &with_defaults(&defaults, Some(own)),
                );
                if let Some(vscode) = vscode {
                    write_config(&self.vscode_paths().settings, vscode);
                }
            }
            (None, Some(vscode)) => {
                write_config(
                    &self.vscode_paths().settings,
                    &with_defaults(&defaults, Some(vscode)),
                );
            }
            (None, None) => {
                write_config(&self.deco_paths().settings, &with_defaults(&defaults, None));
            }
        }
    }

    /// What `deco` says about a command line it refuses.
    pub fn usage_error(&self, args: &[&str]) -> String {
        match deco::cli::parse(args.iter().map(|arg| arg.to_string())) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("`deco {}` was accepted", args.join(" ")),
        }
    }

    /// What `deco --print-config` would print for this machine.
    pub fn print_config(&self, args: &[&str]) -> String {
        let mut with_flag: Vec<&str> = args.to_vec();
        with_flag.push("--print-config");
        let editor = self.launch(&with_flag);
        deco::startup::config_report(editor.session())
    }

    // ---- what the harness needs to know about it --------------------------

    /// The machine as [`deco::startup`] sees it.
    pub(crate) fn boot(&self) -> deco::startup::Boot {
        deco::startup::Boot {
            env: Env {
                home: Some(self.home.clone()),
                xdg_config_home: None,
                appdata: None,
            },
            layout: self.layout,
            platform: self.platform,
            // The directory the shell would have been in when deco was started.
            cwd: Some(self.workspace.clone()),
        }
    }

    /// Where installed extensions live, for both deco and VS Code.
    pub(crate) fn extension_roots(&self) -> Vec<PathBuf> {
        vec![self.deco_paths().extensions, self.vscode_paths().extensions]
    }

    /// The workspace directory.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The home directory.
    pub fn home(&self) -> &Path {
        &self.home
    }

    pub(crate) fn terminal_size(&self) -> (u16, u16) {
        self.size
    }

    fn deco_paths(&self) -> ConfigPaths {
        ConfigPaths::deco(&self.boot().env, self.layout).expect("a config directory")
    }

    fn vscode_paths(&self) -> ConfigPaths {
        ConfigPaths::vscode(&self.boot().env, self.layout).expect("a config directory")
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Left where it is, and said out loud: the first question about a
            // failing scenario is what was on disk when it failed, and an
            // assertion message cannot carry a directory.
            eprintln!(
                "deco-e2e: the scenario that just failed is still at {}",
                self.root.display()
            );
            return;
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A settings object holding the harness's keys, then the scenario's own.
///
/// Spliced textually rather than parsed and merged, because a scenario's JSON is
/// JSONC — it has comments in it, on purpose, because a real `settings.json`
/// does — and a merge through a JSON value would throw them away.
fn with_defaults(defaults: &[&str], own: Option<&str>) -> String {
    if defaults.is_empty() {
        // Nothing of the harness's to add, so the scenario's own text goes to
        // disk exactly as it was written — and an object with only a leading
        // comma in it, which is what splicing nothing in front would produce, is
        // not valid JSON.
        return own.unwrap_or("{}").to_owned();
    }
    let mut json = String::from("{\n");
    json.push_str(&defaults.join(",\n"));
    match own {
        Some(settings) => {
            let rest = settings.trim();
            let inner = rest
                .strip_prefix('{')
                .unwrap_or_else(|| panic!("settings must be a JSON object, got {rest}"));
            if inner.trim_start().starts_with('}') {
                json.push_str("\n}");
            } else {
                json.push(',');
                json.push_str(inner);
            }
        }
        None => json.push_str("\n}"),
    }
    json
}

/// The `language_server` example, built alongside the tests that use it.
///
/// `cargo test` puts examples in `target/<profile>/examples/` and the test
/// binary in `target/<profile>/deps/`, so it is two levels up and across. There
/// is no `CARGO_BIN_EXE_*` for an example, which is why this is derived rather
/// than looked up.
fn fake_server() -> PathBuf {
    let test_binary = std::env::current_exe().expect("the test binary's own path");
    let profile = test_binary
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target/<profile>/deps/<binary>");
    let path = profile
        .join("examples")
        .join(format!("language_server{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "the language_server example was not built at {}.\n\
         `cargo test -p deco-e2e` builds it; a bare `cargo test --test <name>` may not.",
        path.display()
    );
    path
}

/// One JSON object's keys in front of another's.
///
/// Textual rather than parsed and merged, for the reason `with_defaults` is: a
/// scenario's settings are JSONC with comments in them, and a merge through a
/// JSON value would throw those away. `first` wins only where `second` does not
/// repeat the key, since a repeated key takes its last value.
fn splice(first: &str, second: &str) -> String {
    let first = first.trim().trim_end_matches('}').trim_end();
    let second = second.trim();
    let rest = second
        .strip_prefix('{')
        .unwrap_or_else(|| panic!("settings must be a JSON object, got {second}"));
    if rest.trim_start().starts_with('}') {
        return format!("{first}\n}}");
    }
    format!("{first},{rest}")
}

/// Writes a configuration file, creating its directory.
fn write_config(path: &Path, json: &str) {
    let parent = path.parent().expect("a configuration file has a directory");
    std::fs::create_dir_all(parent).expect("a configuration directory");
    std::fs::write(path, json).expect("a configuration file");
}
