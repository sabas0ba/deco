//! Finding and loading the user's configuration.

use std::path::{Path, PathBuf};

use deco_config::paths::{ConfigPaths, Env, Layout};
use deco_config::snippets::{FileKind, UserSnippet};
use deco_config::{Scope, Settings};

/// Everything read off disk at startup.
pub struct LoadedConfig {
    /// The layered settings.
    pub settings: Settings,
    /// The raw `keybindings.json`, if there was one.
    pub keybindings: Option<String>,
    /// The user's snippets, then the workspace's.
    pub snippets: Vec<UserSnippet>,
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
/// without being copied. Nothing is ever written back to VS Code's directory;
/// the import is intentionally one-way.
///
/// `remote_settings` is the connected machine's own `machine-settings.json`,
/// already fetched over the connection because this machine cannot read it.
/// It becomes the [`Scope::Remote`] layer, above the user's and below the
/// workspace's, as in VS Code. The layer is **untrusted**: a language server
/// defined there still has to be confirmed, and the extension sandbox ignores
/// its settings.
pub fn load(
    env: &Env,
    layout: Layout,
    workspace: Option<&Path>,
    remote_settings: Option<&str>,
) -> LoadedConfig {
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

    // Between the user's and the workspace's, which is the order VS Code uses
    // and the order `Scope` already declares. Loaded before the workspace layer
    // below so that a project can still override a machine-wide setting.
    if let Some(source) = remote_settings {
        if let Err(error) = settings.load_layer(Scope::Remote, source) {
            problems.push(format!("the remote's machine-settings.json: {error}"));
        }
    }

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

    // deco's own snippets directory is preferred, and VS Code's is read when
    // deco has none, as for `settings.json`.
    let mut snippets = Vec::new();
    let user_snippets = deco_paths
        .as_ref()
        .map(|p| p.snippets.clone())
        .filter(|dir| dir.is_dir())
        .or_else(|| vscode_paths.as_ref().map(|p| p.snippets.clone()));
    if let Some(dir) = user_snippets {
        load_snippets(&dir, false, &mut snippets, &mut problems);
    }
    if let Some(root) = workspace {
        for dir in [root.join(".deco"), root.join(".vscode")] {
            load_snippets(&dir, true, &mut snippets, &mut problems);
        }
    }

    LoadedConfig {
        settings,
        keybindings,
        snippets,
        problems,
    }
}

/// Reads every snippet file in `dir`, in file-name order.
///
/// A workspace may only hold `*.code-snippets` files, as in VS Code, because
/// its `.vscode` directory also holds `settings.json` and other JSON files
/// that are not snippets. A directory that does not exist holds no snippets.
fn load_snippets(
    dir: &Path,
    workspace: bool,
    snippets: &mut Vec<UserSnippet>,
    problems: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, FileKind)> = entries
        .flatten()
        .filter_map(|entry| {
            let kind = FileKind::of(&entry.file_name().to_string_lossy())?;
            let usable = !workspace || kind == FileKind::Global;
            (usable && entry.path().is_file()).then(|| (entry.path(), kind))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, kind) in files {
        let Some(text) = read_optional(&path, problems) else {
            continue;
        };
        let (found, file_problems) = deco_config::snippets::parse(&kind, &text);
        snippets.extend(found);
        problems.extend(
            file_problems
                .into_iter()
                .map(|problem| format!("{}: {problem}", path.display())),
        );
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
        let loaded = load(&env, Layout::Xdg, None, None);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        // The built-in defaults are still there.
        assert_eq!(loaded.settings.get_u64("editor.tabSize", None), Some(4));
        assert!(loaded.keybindings.is_none());
        assert!(loaded.snippets.is_empty());
    }

    #[test]
    fn snippets_come_from_the_user_and_the_workspace() {
        let temp = std::env::temp_dir().join(format!("deco-snippets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let env = Env {
            home: Some(temp.join("home")),
            xdg_config_home: Some(temp.join("config")),
            ..Default::default()
        };
        let user = temp.join("config").join("deco").join("snippets");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            user.join("rust.json"),
            r#"{ "fn": { "prefix": "fn", "body": "fn $1() {}" } }"#,
        )
        .unwrap();
        std::fs::write(user.join("notes.txt"), "not a snippet file").unwrap();
        let workspace = temp.join("project");
        std::fs::create_dir_all(workspace.join(".vscode")).unwrap();
        std::fs::write(
            workspace.join(".vscode").join("team.code-snippets"),
            r#"{ "todo": { "prefix": "todo", "body": "TODO($1)" } }"#,
        )
        .unwrap();
        // A language file in a workspace is not read: `.vscode` holds other JSON.
        std::fs::write(workspace.join(".vscode").join("settings.json"), "{}").unwrap();

        let loaded = load(&env, Layout::Xdg, Some(&workspace), None);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        let names: Vec<&str> = loaded.snippets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["fn", "todo"]);
        std::fs::remove_dir_all(&temp).ok();
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
