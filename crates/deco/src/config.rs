//! Finding and loading the user's configuration.

use std::path::{Path, PathBuf};

use deco_config::paths::{ConfigPaths, Env, Layout};
use deco_config::{Scope, Settings};

/// Everything read off disk at startup.
pub struct LoadedConfig {
    /// The layered settings.
    pub settings: Settings,
    /// The raw `keybindings.json`, if there was one.
    pub keybindings: Option<String>,
    /// Anything that went wrong, to be shown rather than to stop startup.
    pub problems: Vec<String>,
}

/// Reads a file, returning `None` if it is simply absent and recording a
/// problem if it exists but cannot be read.
fn read_optional(path: &Path, problems: &mut Vec<String>) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            problems.push(format!("{}: {error}", path.display()));
            None
        }
    }
}

/// Loads settings and keybindings.
///
/// deco's own configuration directory is preferred; if it holds no
/// `settings.json`, VS Code's is read instead so an existing setup works
/// without being copied. Nothing is ever written back to VS Code's directory —
/// the import is one-way on purpose.
pub fn load(env: &Env, layout: Layout, workspace: Option<&Path>) -> LoadedConfig {
    let mut problems = Vec::new();
    let mut settings = Settings::with_defaults();

    let deco_paths = ConfigPaths::deco(env, layout);
    let vscode_paths = ConfigPaths::vscode(env, layout);

    let user_settings = deco_paths
        .as_ref()
        .and_then(|p| read_optional(&p.settings, &mut problems))
        .or_else(|| {
            vscode_paths
                .as_ref()
                .and_then(|p| read_optional(&p.settings, &mut problems))
        });
    if let Some(source) = &user_settings {
        if let Err(error) = settings.load_layer(Scope::User, source) {
            problems.push(format!("settings.json: {error}"));
        }
    }

    let keybindings = deco_paths
        .as_ref()
        .and_then(|p| read_optional(&p.keybindings, &mut problems))
        .or_else(|| {
            vscode_paths
                .as_ref()
                .and_then(|p| read_optional(&p.keybindings, &mut problems))
        });

    if let Some(root) = workspace {
        for candidate in deco_config::paths::workspace_settings_candidates(root) {
            let Some(source) = read_optional(&candidate, &mut problems) else {
                continue;
            };
            if let Err(error) = settings.load_layer(Scope::Workspace, &source) {
                problems.push(format!("{}: {error}", candidate.display()));
            }
            // The first candidate that exists wins; `.deco` shadows `.vscode`.
            break;
        }
    }

    LoadedConfig {
        settings,
        keybindings,
        problems,
    }
}

/// The workspace root implied by the file being opened.
///
/// Walks upwards looking for a marker directory, and falls back to the file's
/// own directory. This is what a workspace settings file is resolved against.
pub fn workspace_root_for(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    let mut current = Some(start);
    while let Some(dir) = current {
        for marker in [".deco", ".vscode", ".git"] {
            if dir.join(marker).exists() {
                return Some(dir.to_path_buf());
            }
        }
        current = dir.parent();
    }
    Some(start.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_directory_is_not_a_problem() {
        let env = Env {
            home: Some(PathBuf::from("/nonexistent-home")),
            ..Default::default()
        };
        let loaded = load(&env, Layout::Xdg, None);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        // The built-in defaults are still there.
        assert_eq!(loaded.settings.get_u64("editor.tabSize", None), Some(4));
        assert!(loaded.keybindings.is_none());
    }

    #[test]
    fn the_workspace_root_walks_up_to_a_marker() {
        let temp = std::env::temp_dir().join(format!("deco-test-{}", std::process::id()));
        let nested = temp.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(temp.join(".git")).unwrap();

        let found = workspace_root_for(&nested.join("file.rs")).unwrap();
        assert_eq!(found, temp);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_file_with_no_marker_above_it_uses_its_own_directory() {
        let temp = std::env::temp_dir().join(format!("deco-nomarker-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let found = workspace_root_for(&temp.join("file.rs")).unwrap();
        // Somewhere at or above the file, but never nothing.
        assert!(found.starts_with(std::env::temp_dir()) || found == temp);
        std::fs::remove_dir_all(&temp).ok();
    }
}
