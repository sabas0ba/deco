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
    language_servers: bool,
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
            language_servers: false,
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

    /// deco's own `keybindings.json`.
    pub fn user_keybindings(self, json: &str) -> Self {
        let path = self.deco_paths().keybindings;
        write_config(&path, json);
        self
    }

    /// VS Code's `settings.json`, which deco reads when it has none of its own.
    pub fn vscode_settings(self, json: &str) -> Self {
        let path = self.vscode_paths().settings;
        write_config(&path, json);
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
    /// The harness's own key goes in first so that anything the scenario wrote
    /// comes later and therefore wins, which is the rule the JSONC layer already
    /// applies to a repeated key.
    fn write_user_settings(&self) {
        let own = self.user_settings.as_deref();
        if own.is_none() && self.language_servers {
            // Nothing to write: no scenario settings, and the machine is allowed
            // its language servers.
            return;
        }
        let mut json = String::new();
        if self.language_servers {
            json.push_str(own.unwrap_or("{}"));
        } else {
            json.push_str("{\n    // deco-e2e: this machine has no language servers installed.\n");
            json.push_str("    \"deco.lsp.enabled\": false");
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
        }
        write_config(&self.deco_paths().settings, &json);
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

/// Writes a configuration file, creating its directory.
fn write_config(path: &Path, json: &str) {
    let parent = path.parent().expect("a configuration file has a directory");
    std::fs::create_dir_all(parent).expect("a configuration directory");
    std::fs::write(path, json).expect("a configuration file");
}
