//! The environment a scenario runs in: a home directory, a workspace, and the
//! files in both.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use deco_config::paths::{ConfigPaths, Env, Layout};
use deco_keymap::binding::Platform;

use crate::editor::Editor;

/// Distinguishes scenarios built in the same process, so that tests running on
/// different threads do not use the same directory.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A simulated machine for deco to start on.
///
/// Everything is under one temporary directory: `home/` holds the configuration
/// deco and VS Code would read, and `work/` contains the workspace files. The
/// directory is removed when the scenario is dropped. If the test failed, the
/// directory is kept and its path is printed so the files can be inspected.
pub struct Scenario {
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    layout: Layout,
    platform: Platform,
    size: (u16, u16),
    /// Written at launch rather than when set, because the harness inserts a
    /// key of its own before it. See [`Scenario::language_servers`].
    user_settings: Option<String>,
    /// Also written at launch. In a scenario about deco reading VS Code's file,
    /// the harness's keys must go into VS Code's file. Writing them to deco's
    /// file would create the file whose absence is being tested.
    vscode_settings: Option<String>,
    language_servers: bool,
    /// A separate workspace for the remote side, set once a scenario adds a file
    /// to it. See [`Scenario::remote_file`].
    remote: Option<PathBuf>,
    /// The file the remote side serves as its machine settings, if a scenario
    /// set one. See [`Scenario::remote_machine_settings`].
    machine_settings: Option<PathBuf>,
}

impl Scenario {
    /// A new, empty machine. `name` only needs to be recognisable in a
    /// directory listing.
    pub fn new(name: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("deco-e2e-{name}-{}-{unique}", std::process::id()));
        // A previous run that was killed rather than dropped leaves its
        // directory behind. Remove it so the scenario does not start with files
        // it did not write.
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let workspace = root.join("work");
        std::fs::create_dir_all(&home).expect("a home directory");
        std::fs::create_dir_all(&workspace).expect("a workspace");
        Self {
            root,
            home,
            workspace,
            // Not `Layout::host()`. Otherwise a scenario about where settings are
            // read from would behave differently on each platform. All three
            // layouts can be selected on any platform.
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

    /// Which platform's keybindings apply: `key` or `mac`, and the
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

    /// Writes a file as raw bytes, for contents that are inconvenient or
    /// impossible in a `&str`: a UTF-8 BOM, a lone CR, or invalid UTF-8.
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
    /// Off by default. A scenario must behave the same on every machine, but a
    /// machine with `rust-analyzer` installed behaves differently: the status
    /// line shows a server's message, a completion list opens on `.`, and a
    /// scenario about saving a file can fail for an unrelated reason. By default
    /// the scenario therefore disables language servers with the user setting
    /// `"deco.lsp.enabled": false` in `settings.json`.
    ///
    /// Turn it on for a scenario about language servers, and configure a server
    /// that can run on the machine through `deco.lsp.servers`.
    pub fn language_servers(mut self, enabled: bool) -> Self {
        self.language_servers = enabled;
        self
    }

    /// Installs a language server for `language` that this scenario can rely on.
    ///
    /// The server is [`examples/language_server.rs`], a separate program that
    /// speaks LSP over a pipe, not a stub passed to the editor. `role` is
    /// `argv[1]` and selects the features it offers; `"full"` enables all of
    /// them.
    ///
    /// The definition is written into `deco.lsp.servers` as a user would write
    /// it, so the configuration path is also tested. This also enables
    /// [`Scenario::language_servers`].
    ///
    /// [`examples/language_server.rs`]: https://github.com/sabas0ba/deco/blob/main/crates/deco-e2e/examples/language_server.rs
    pub fn language_server(mut self, language: &str, role: &str) -> Self {
        self.language_servers = true;
        let program = fake_server();
        // Through `serde_json` rather than `format!`, because backslashes in a
        // Windows path must be escaped to be read back correctly as JSON.
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
        // Combined with any settings the scenario already set, so a scenario can
        // have both a server and its own settings.
        self.user_settings = Some(match self.user_settings.take() {
            Some(existing) => splice(&text, &existing),
            None => text,
        });
        self
    }

    /// Writes a file on the remote side, in a directory separate from the local
    /// workspace.
    ///
    /// Without this, [`Scenario::launch_remote`] serves the scenario's own
    /// workspace, so the remote and local sides are the same directory. A
    /// scenario then cannot distinguish a file read through the connection from
    /// one read from the local disk, or a write to the server from a local write.
    ///
    /// With this method the two sides are different directories.
    /// [`Editor::on_disk`] then reads the remote directory, because a remote
    /// session's files are there.
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

    /// The directory the remote side serves: the remote workspace if a scenario
    /// created one, otherwise the local workspace.
    pub(crate) fn served_workspace(&self) -> PathBuf {
        self.remote
            .clone()
            .unwrap_or_else(|| self.workspace.clone())
    }

    /// The remote side's `machine-settings.json`.
    ///
    /// These are per-machine settings, as opposed to user settings. A session
    /// connected to that machine uses them as its `remote` layer. The file is
    /// written into the scenario's directory and passed to the server on its
    /// command line. The server is a separate process, and changing the
    /// environment in a test would affect every other test.
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

    /// VS Code's `keybindings.json`, read under the same condition.
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

    /// Installs a theme extension into deco's extensions directory, in the same
    /// form as a marketplace download.
    ///
    /// `id` is the directory name, which is `publisher.name-version` for
    /// extensions installed by VS Code. `label` is the theme name shown in the
    /// picker, and `theme` is the JSON content of the theme file.
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

    /// Starts deco with `args`, as they would be typed after the program name.
    /// Relative paths are resolved against the workspace, which is the shell's
    /// working directory.
    ///
    /// Panics if the arguments do not parse, because a mistyped flag is an error
    /// in the test. Use [`Scenario::usage_error`] to assert that a command line
    /// is rejected.
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

    /// Like [`Scenario::launch`], with the workspace served by a real
    /// `deco --server` process.
    ///
    /// The only difference from a real session is that the server command is
    /// not prefixed with `ssh host`. That omits only an argument vector, which
    /// is tested where it is built. Everything a remote session depends on is
    /// kept: a second process, a framed protocol over its stdio, documents keyed
    /// by paths relative to the remote workspace, and a server that rejects
    /// anything outside it.
    ///
    /// The authority is real so that language servers resolve as in a real
    /// session: they are wrapped in a transport. A scenario without `docker`
    /// reports that error instead of running the server locally.
    /// `server_binary` is the `deco` binary to run as the remote side. It is a
    /// parameter because `CARGO_BIN_EXE_*` is only defined for tests of the
    /// package that builds the binary, and guessing a path under `target/` is
    /// unreliable.
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
                // The path as the server reports it. Every path in the session
                // is relative to it.
                workspace: std::path::PathBuf::from(hello.workspace),
            },
        };
        Editor::start_with(self, cli, Some(remote))
            .unwrap_or_else(|error| panic!("`deco {}` did not start: {error:#}", args.join(" ")))
    }

    /// The error message when `deco` fails to start.
    ///
    /// Startup can fail with a valid command line, for example when a file is a
    /// directory or cannot be read by the account. The user then sees this
    /// message and no editor.
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
    /// The harness has one key of its own, `deco.lsp.enabled: false`, added
    /// when the machine has no language servers. It goes into the file the
    /// scenario tests: deco's if the scenario wrote one, VS Code's if it wrote
    /// only that. Always writing it to deco's file would create the file that a
    /// scenario about reading VS Code's settings expects to be absent.
    ///
    /// It is written first, so the scenario's own keys come later and take
    /// precedence. The JSONC layer uses the last value of a repeated key.
    fn write_user_settings(&self) {
        let mut defaults: Vec<&str> = Vec::new();
        if !self.language_servers {
            defaults.push(
                "    // deco-e2e: this machine has no language servers installed.\n    \"deco.lsp.enabled\": false",
            );
        }
        // `files.eol` is intentionally not set here. It decides the ending of a
        // new file, an untitled buffer and an existing file with no line
        // terminator; an existing file with an ending keeps its own. Without
        // the key the default `auto` applies, which is the platform's ending,
        // and some scenarios test that default (for example `untitled_line` in
        // `tests/files.rs`). A harness default would replace it. A scenario
        // that checks the bytes of a file it created sets the key itself.

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

    /// The error message `deco` prints for a rejected command line.
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

    /// The machine as passed to [`deco::startup`].
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
            // Keep the directory and print its path, so the files on disk at the
            // time of the failure can be inspected.
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
/// Combined as text rather than parsed and merged. A scenario's settings are
/// JSONC and contain comments, as a real `settings.json` does, and merging
/// through a JSON value would drop them.
fn with_defaults(defaults: &[&str], own: Option<&str>) -> String {
    if defaults.is_empty() {
        // No harness keys, so the scenario's text is written unchanged.
        // Splicing an empty list would leave a leading comma, which is not
        // valid JSON.
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
/// binary in `target/<profile>/deps/`. The path is derived from the test
/// binary's path because there is no `CARGO_BIN_EXE_*` for an example.
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

/// Places one JSON object's keys before another's.
///
/// Combined as text for the same reason as `with_defaults`: a scenario's
/// settings are JSONC with comments, and merging through a JSON value would drop
/// them. A key in `first` applies only if `second` does not repeat it, because a
/// repeated key takes its last value.
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
