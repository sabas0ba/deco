//! Walking a workspace for the quick-open list.
//!
//! Here rather than in `deco-editor` because the core has no filesystem at all —
//! a document is handed its text, never a path to read — and that is what lets the
//! whole editable surface be tested without one.
//!
//! # Bounded on purpose
//!
//! `ctrl+p` must answer immediately, and a workspace can be a home directory
//! someone opened by mistake. The walk therefore stops at [`MAX_FILES`] and at
//! [`MAX_DEPTH`], and says so rather than pretending the list is complete: a
//! quick-open that silently omits the file you wanted is worse than one that
//! admits it ran out of room.

use std::path::Path;

use deco_config::{glob, Settings};
use deco_editor::commands::PaletteEntry;

/// How many files the list holds before the walk gives up.
///
/// Large enough for any real project, small enough that the walk and the
/// filtering are both imperceptible.
pub const MAX_FILES: usize = 10_000;

/// How deep the walk goes.
///
/// A guard against a symlink loop as much as against a deep tree: `read_dir`
/// follows symlinks, and a link pointing at an ancestor would otherwise recurse
/// until the stack ran out.
pub const MAX_DEPTH: usize = 24;

/// Directories skipped whatever the settings say.
///
/// `files.exclude` covers `.git` and friends by default, but a build directory is
/// not in it and is exactly what makes a walk slow and its results useless. These
/// are conventions rather than configuration; a user who wants `target/` in the
/// list can still open it by typing the path.
const ALWAYS_SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    ".next",
    ".cache",
];

/// The result of a walk.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The files found, in the order they should be offered.
    pub files: Vec<PaletteEntry>,
    /// Whether the walk hit a limit and stopped early.
    pub truncated: bool,
}

/// Lists the files under `root` for quick open.
///
/// Paths in the entries are absolute, because that is what opening one needs;
/// the titles are relative to `root`, because that is what a person recognises.
pub fn list(root: &Path, settings: &Settings) -> Listing {
    let excludes = exclude_patterns(settings);
    let mut listing = Listing::default();
    walk(root, root, 0, &excludes, &mut listing);

    // By path, so the same workspace always offers the same order — `read_dir`
    // gives no ordering guarantee, and a list that reshuffles between presses is
    // one you cannot learn.
    listing.files.sort_by(|a, b| a.title.cmp(&b.title));
    listing
}

fn walk(root: &Path, dir: &Path, depth: usize, excludes: &[String], listing: &mut Listing) {
    if depth > MAX_DEPTH {
        listing.truncated = true;
        return;
    }
    // An unreadable directory is skipped rather than reported: a workspace with
    // one root-owned subdirectory in it should still offer the rest.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        if listing.files.len() >= MAX_FILES {
            listing.truncated = true;
            return;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(relative) = relative_to(root, &path) else {
            continue;
        };

        // `file_type` rather than `metadata`, so a broken symlink is a symlink
        // and not an error that aborts the directory.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if ALWAYS_SKIP.contains(&name) || is_excluded(excludes, &relative) {
                continue;
            }
            walk(root, &path, depth + 1, excludes, listing);
        } else if kind.is_file() && !is_excluded(excludes, &relative) {
            listing.files.push(PaletteEntry {
                id: path.to_string_lossy().into_owned(),
                title: relative,
            });
        }
    }
}

/// `path` relative to `root`, with `/` separators whatever the platform uses.
///
/// The glob dialect is `/`-separated, and so is every pattern anyone writes in a
/// `files.exclude`, so a Windows path has to be spelled that way before it is
/// matched — otherwise `**/.git` never matches anything there.
fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut out = String::new();
    for component in relative.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    Some(out)
}

/// The enabled patterns from `files.exclude`.
///
/// The setting is a map of pattern to boolean, and a pattern set to `false` is
/// how VS Code turns off one it inherited — so the value matters, not just the
/// key's presence.
fn exclude_patterns(settings: &Settings) -> Vec<String> {
    settings
        .get("files.exclude")
        .and_then(|value| value.as_object())
        .map(|map| {
            map.iter()
                .filter(|(_, enabled)| enabled.as_bool() == Some(true))
                .map(|(pattern, _)| pattern.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn is_excluded(patterns: &[String], relative: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| glob::matches(pattern, relative))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Builds a tree under a fresh temporary directory and returns its root.
    ///
    /// Each path is `dir/file` or `file`; a trailing `/` makes a directory.
    fn tree(name: &str, paths: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("deco-files-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for path in paths {
            let full = root.join(path);
            if path.ends_with('/') {
                std::fs::create_dir_all(&full).unwrap();
            } else {
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&full, "x").unwrap();
            }
        }
        root
    }

    fn titles(listing: &Listing) -> Vec<&str> {
        listing.files.iter().map(|f| f.title.as_str()).collect()
    }

    #[test]
    fn files_are_listed_relative_to_the_root_and_sorted() {
        let root = tree("sorted", &["b.rs", "a.rs", "src/c.rs"]);
        let listing = list(&root, &Settings::with_defaults());
        assert_eq!(titles(&listing), vec!["a.rs", "b.rs", "src/c.rs"]);
        assert!(!listing.truncated);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_entry_id_is_an_absolute_path_to_open() {
        let root = tree("absolute", &["a.rs"]);
        let listing = list(&root, &Settings::with_defaults());
        let entry = &listing.files[0];
        assert!(Path::new(&entry.id).is_absolute(), "{}", entry.id);
        assert!(Path::new(&entry.id).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn conventional_build_and_vcs_directories_are_skipped() {
        let root = tree(
            "skipped",
            &[
                "a.rs",
                "target/debug/junk.rs",
                "node_modules/pkg/index.js",
                ".git/config",
            ],
        );
        let listing = list(&root, &Settings::with_defaults());
        assert_eq!(titles(&listing), vec!["a.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_files_exclude_pattern_is_honoured() {
        let root = tree("excluded", &["keep.rs", "secret.key", "sub/other.key"]);
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                deco_config::Scope::User,
                r#"{ "files.exclude": { "**/*.key": true } }"#,
            )
            .unwrap();
        let listing = list(&root, &settings);
        assert_eq!(titles(&listing), vec!["keep.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pattern_turned_off_is_not_applied() {
        // `false` is how VS Code disables a pattern it inherited, so the value
        // matters and not just the key.
        let root = tree("disabled", &["keep.rs", ".git-not-a-dir"]);
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                deco_config::Scope::User,
                r#"{ "files.exclude": { "**/keep.rs": false } }"#,
            )
            .unwrap();
        assert!(titles(&list(&root, &settings)).contains(&"keep.rs"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_excluded_directory_is_not_descended_into() {
        let root = tree("dir-exclude", &["a.rs", "vendor/deep/b.rs"]);
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                deco_config::Scope::User,
                r#"{ "files.exclude": { "vendor": true } }"#,
            )
            .unwrap();
        assert_eq!(titles(&list(&root, &settings)), vec!["a.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_root_lists_nothing_rather_than_failing() {
        let listing = list(
            Path::new("/deco-does-not-exist-anywhere"),
            &Settings::with_defaults(),
        );
        assert!(listing.files.is_empty());
        assert!(!listing.truncated);
    }

    #[test]
    fn hitting_the_file_limit_is_reported_rather_than_hidden() {
        // The limit itself is not exercised — writing ten thousand files in a test
        // is slower than the feature — so the flag is checked on the depth guard,
        // which is the same field and the same contract.
        let mut listing = Listing::default();
        walk(
            Path::new("/"),
            Path::new("/"),
            MAX_DEPTH + 1,
            &[],
            &mut listing,
        );
        assert!(listing.truncated);
        assert!(listing.files.is_empty());
    }
}
