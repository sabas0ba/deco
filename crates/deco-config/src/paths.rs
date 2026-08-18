//! Where deco looks for configuration, and where VS Code keeps its own.
//!
//! deco reads its own directory first and falls back to VS Code's, so a user
//! who already has `settings.json` and `keybindings.json` gets their editor
//! behaviour without copying anything. Nothing is ever written to VS Code's
//! directory — importing is one-way on purpose.

use std::path::{Path, PathBuf};

/// The host platform's configuration layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `$XDG_CONFIG_HOME` or `~/.config`.
    Xdg,
    /// `~/Library/Application Support`.
    MacOs,
    /// `%APPDATA%`.
    Windows,
}

impl Layout {
    /// The layout of the machine this binary is running on.
    pub const fn host() -> Self {
        if cfg!(target_os = "windows") {
            Layout::Windows
        } else if cfg!(target_os = "macos") {
            Layout::MacOs
        } else {
            Layout::Xdg
        }
    }
}

/// The environment inputs the path rules depend on.
///
/// Taking these as data rather than reading the process environment directly is
/// what makes the rules for all three platforms testable from any one of them.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// The user's home directory.
    pub home: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME`, if set.
    pub xdg_config_home: Option<PathBuf>,
    /// `%APPDATA%`, if set.
    pub appdata: Option<PathBuf>,
}

impl Env {
    /// Reads the relevant variables from the process environment.
    pub fn from_process() -> Self {
        Self {
            home: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            appdata: std::env::var_os("APPDATA").map(PathBuf::from),
        }
    }
}

/// The base configuration directory for an application named `app`.
pub fn config_dir_for(env: &Env, layout: Layout, app: &str) -> Option<PathBuf> {
    match layout {
        Layout::Xdg => {
            // An empty XDG_CONFIG_HOME is treated as unset, per the spec.
            let base = env
                .xdg_config_home
                .as_ref()
                .filter(|p| !p.as_os_str().is_empty())
                .cloned()
                .or_else(|| env.home.as_ref().map(|h| h.join(".config")))?;
            Some(base.join(app))
        }
        Layout::MacOs => Some(
            env.home
                .as_ref()?
                .join("Library")
                .join("Application Support")
                .join(app),
        ),
        Layout::Windows => {
            let base = env
                .appdata
                .clone()
                .or_else(|| env.home.as_ref().map(|h| h.join("AppData").join("Roaming")))?;
            Some(base.join(app))
        }
    }
}

/// The set of files and directories deco reads for a given installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    /// The base directory, e.g. `~/.config/deco`.
    pub root: PathBuf,
    /// `settings.json`.
    pub settings: PathBuf,
    /// `keybindings.json`.
    pub keybindings: PathBuf,
    /// Installed extensions.
    pub extensions: PathBuf,
    /// User snippets.
    pub snippets: PathBuf,
    /// `permissions.json`: what extensions have been allowed and refused.
    ///
    /// deco's own, with no VS Code equivalent — there a capability decision does
    /// not exist, because an extension has whatever Node has.
    pub permissions: PathBuf,
}

impl ConfigPaths {
    /// Derives the standard file layout under `root`.
    ///
    /// VS Code splits these between `Code/User/*.json` and `~/.vscode/extensions`;
    /// [`ConfigPaths::vscode`] applies that quirk, while deco keeps everything under one root.
    pub fn under(root: PathBuf) -> Self {
        Self {
            settings: root.join("settings.json"),
            keybindings: root.join("keybindings.json"),
            extensions: root.join("extensions"),
            snippets: root.join("snippets"),
            permissions: root.join("permissions.json"),
            root,
        }
    }

    /// deco's own configuration paths.
    pub fn deco(env: &Env, layout: Layout) -> Option<Self> {
        Some(Self::under(config_dir_for(env, layout, "deco")?))
    }

    /// VS Code's configuration paths, for one-way import.
    ///
    /// VS Code stores user JSON under `<config>/Code/User` but extensions in
    /// `~/.vscode/extensions` on every platform, which is why this cannot just
    /// call [`ConfigPaths::under`].
    pub fn vscode(env: &Env, layout: Layout) -> Option<Self> {
        let user = config_dir_for(env, layout, "Code")?.join("User");
        let extensions = env.home.as_ref()?.join(".vscode").join("extensions");
        Some(Self {
            settings: user.join("settings.json"),
            keybindings: user.join("keybindings.json"),
            snippets: user.join("snippets"),
            // VS Code has no equivalent, so there is nothing to import: this path
            // exists on the struct and is never read for a VS Code layout.
            permissions: user.join("permissions.json"),
            extensions,
            root: user,
        })
    }
}

/// Workspace-level settings files, in the order deco prefers them.
///
/// `.deco` wins so a project can hold deco-specific settings, but a project
/// that only has `.vscode` still works untouched.
pub fn workspace_settings_candidates(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join(".deco").join("settings.json"),
        root.join(".vscode").join("settings.json"),
    ]
}

/// Workspace-level keybinding files, in preference order.
pub fn workspace_keybindings_candidates(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join(".deco").join("keybindings.json"),
        root.join(".vscode").join("keybindings.json"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &str) -> Env {
        Env {
            home: Some(PathBuf::from(home)),
            ..Default::default()
        }
    }

    #[test]
    fn xdg_layout_prefers_xdg_config_home() {
        let mut e = env("/home/u");
        e.xdg_config_home = Some(PathBuf::from("/custom/cfg"));
        assert_eq!(
            config_dir_for(&e, Layout::Xdg, "deco").unwrap(),
            PathBuf::from("/custom/cfg/deco")
        );
    }

    #[test]
    fn xdg_layout_falls_back_to_dot_config() {
        assert_eq!(
            config_dir_for(&env("/home/u"), Layout::Xdg, "deco").unwrap(),
            PathBuf::from("/home/u/.config/deco")
        );
    }

    #[test]
    fn an_empty_xdg_config_home_is_treated_as_unset() {
        let mut e = env("/home/u");
        e.xdg_config_home = Some(PathBuf::new());
        assert_eq!(
            config_dir_for(&e, Layout::Xdg, "deco").unwrap(),
            PathBuf::from("/home/u/.config/deco")
        );
    }

    #[test]
    fn macos_layout_uses_application_support() {
        assert_eq!(
            config_dir_for(&env("/Users/u"), Layout::MacOs, "deco").unwrap(),
            PathBuf::from("/Users/u/Library/Application Support/deco")
        );
    }

    #[test]
    fn windows_layout_prefers_appdata() {
        let mut e = env("C:\\Users\\u");
        e.appdata = Some(PathBuf::from("C:\\Users\\u\\AppData\\Roaming"));
        assert_eq!(
            config_dir_for(&e, Layout::Windows, "deco").unwrap(),
            PathBuf::from("C:\\Users\\u\\AppData\\Roaming").join("deco")
        );
    }

    #[test]
    fn windows_layout_derives_appdata_from_home_when_unset() {
        let e = env("C:\\Users\\u");
        assert_eq!(
            config_dir_for(&e, Layout::Windows, "deco").unwrap(),
            PathBuf::from("C:\\Users\\u")
                .join("AppData")
                .join("Roaming")
                .join("deco")
        );
    }

    #[test]
    fn no_home_means_no_config_dir() {
        let e = Env::default();
        assert!(config_dir_for(&e, Layout::Xdg, "deco").is_none());
        assert!(config_dir_for(&e, Layout::MacOs, "deco").is_none());
        assert!(config_dir_for(&e, Layout::Windows, "deco").is_none());
    }

    #[test]
    fn deco_paths_sit_under_one_root() {
        let paths = ConfigPaths::deco(&env("/home/u"), Layout::Xdg).unwrap();
        assert_eq!(
            paths.settings,
            PathBuf::from("/home/u/.config/deco/settings.json")
        );
        assert_eq!(
            paths.keybindings,
            PathBuf::from("/home/u/.config/deco/keybindings.json")
        );
        assert_eq!(
            paths.extensions,
            PathBuf::from("/home/u/.config/deco/extensions")
        );
    }

    #[test]
    fn vscode_paths_split_user_json_from_extensions() {
        let paths = ConfigPaths::vscode(&env("/home/u"), Layout::Xdg).unwrap();
        assert_eq!(
            paths.settings,
            PathBuf::from("/home/u/.config/Code/User/settings.json")
        );
        // Extensions live outside the config directory on every platform.
        assert_eq!(
            paths.extensions,
            PathBuf::from("/home/u/.vscode/extensions")
        );
    }

    #[test]
    fn vscode_paths_follow_the_platform_layout() {
        let paths = ConfigPaths::vscode(&env("/Users/u"), Layout::MacOs).unwrap();
        assert_eq!(
            paths.settings,
            PathBuf::from("/Users/u/Library/Application Support/Code/User/settings.json")
        );
    }

    #[test]
    fn workspace_candidates_prefer_dot_deco() {
        let candidates = workspace_settings_candidates(Path::new("/w"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/w/.deco/settings.json"),
                PathBuf::from("/w/.vscode/settings.json")
            ]
        );
    }

    #[test]
    fn host_layout_matches_the_build_target() {
        let expected = if cfg!(target_os = "windows") {
            Layout::Windows
        } else if cfg!(target_os = "macos") {
            Layout::MacOs
        } else {
            Layout::Xdg
        };
        assert_eq!(Layout::host(), expected);
    }
}
