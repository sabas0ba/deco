//! Walking a workspace for the quick-open list.
//!
//! This module is in this crate rather than in `deco-editor` because the core has
//! no filesystem access. A document receives its text, never a path to read, so
//! all editing behaviour can be tested without a filesystem.
//!
//! # Bounded on purpose
//!
//! `ctrl+p` must respond immediately, and a workspace can be a home directory
//! opened by mistake. The walk therefore stops at [`MAX_FILES`] and at
//! [`MAX_DEPTH`], and reports that the list is incomplete when it stops early.

use std::path::Path;

use deco_config::{glob, Settings};
use deco_core::search::SearchOptions;
use deco_core::Buffer;
use deco_editor::commands::PaletteEntry;

/// How many files the list holds before the walk stops.
///
/// Large enough for typical projects, and small enough that the walk and the
/// filtering have no noticeable delay.
pub const MAX_FILES: usize = 10_000;

/// How deep the walk goes.
///
/// Bounds the recursion of the walk in very deep trees. Symlink loops cannot occur: the walk classifies entries with
/// `DirEntry::file_type`, which does not follow symlinks, so a symlink to a
/// directory is neither walked nor listed, and a symlink to a file is not listed.
pub const MAX_DEPTH: usize = 24;

/// Directories skipped regardless of settings.
///
/// `files.exclude` covers `.git` and similar directories by default, but not build
/// directories, which make the walk slow and fill the results with generated
/// files. These names are conventions rather than configuration. A file in
/// `target/` can still be opened by typing its path.
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
/// Entry paths are absolute because opening a file needs an absolute path.
/// Titles are relative to `root` because they are easier to read.
pub fn list(root: &Path, settings: &Settings) -> Listing {
    let excludes = exclude_patterns(settings);
    let mut listing = Listing::default();
    walk(root, root, 0, &excludes, &mut listing);

    // Sort by path so the same workspace always has the same order. `read_dir`
    // does not guarantee any ordering.
    listing.files.sort_by(|a, b| a.title.cmp(&b.title));
    listing
}

/// How many matches a project-wide search reports before it stops.
///
/// Very large result lists are not useful to scroll through. The search reports
/// that it stopped at the limit instead.
pub const MAX_MATCHES: usize = 500;

/// Largest file a project-wide search will read.
///
/// Large files such as minified bundles or checked-in databases are rarely search
/// targets, and reading them would take most of the search time.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// What a project-wide search found.
#[derive(Debug, Default)]
pub struct Found {
    /// One entry per match: the file as its `id`, `path:line: text` as its title,
    /// and the position to land on.
    pub matches: Vec<PaletteEntry>,
    /// Whether a limit stopped the search early.
    pub truncated: bool,
    /// How many files were read.
    pub files_searched: usize,
}

/// Searches every file under `root` for `needle`.
///
/// The search is synchronous and bounded. Streaming results would require a
/// background thread and a panel that updates while results arrive. When a bound
/// is reached, the result reports it.
pub fn search(root: &Path, settings: &Settings, needle: &str, options: SearchOptions) -> Found {
    let mut found = Found::default();
    if needle.is_empty() {
        return found;
    }
    // Compiled once for every file. The session rejects an invalid pattern before
    // asking for a search, so an error here finds nothing rather than panicking.
    let Ok(pattern) = deco_core::search::Pattern::new(needle, options) else {
        return found;
    };
    let listing = list(root, settings);
    found.truncated = listing.truncated;

    for entry in &listing.files {
        if found.matches.len() >= MAX_MATCHES {
            found.truncated = true;
            break;
        }
        let path = Path::new(&entry.id);
        // Check the size first, so a huge file costs a `stat` rather than a read.
        if std::fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            continue;
        }
        // A file that is not valid UTF-8 is treated as binary and skipped.
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        found.files_searched += 1;

        // Use the same search as the find bar, so both report the same matches.
        // Loading the file into a rope costs more than a dedicated scan, but keeps
        // a single definition of a match.
        let buffer = Buffer::from_text(&text);
        for range in pattern.find_all(&buffer) {
            if found.matches.len() >= MAX_MATCHES {
                found.truncated = true;
                break;
            }
            let line = buffer
                .line_content(range.start.line as usize)
                .map(|line| line.to_string())
                .unwrap_or_default();
            found.matches.push(PaletteEntry::at(
                &entry.id,
                &format!(
                    "{}:{}: {}",
                    entry.title,
                    range.start.line + 1,
                    truncate(line.trim(), 120)
                ),
                range.start,
            ));
        }
    }
    found
}

/// `text` cut to `limit` characters, with an ellipsis when it was cut.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    text.chars().take(limit).collect::<String>() + "…"
}

fn walk(root: &Path, dir: &Path, depth: usize, excludes: &[String], listing: &mut Listing) {
    if depth > MAX_DEPTH {
        listing.truncated = true;
        return;
    }
    // An unreadable directory is skipped without an error, so a workspace with
    // one root-owned subdirectory still lists the other files.
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

        // Use `file_type` rather than `metadata`, so a broken symlink is reported
        // as a symlink instead of an error that aborts the directory.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if ALWAYS_SKIP.contains(&name) || is_excluded(excludes, &relative) {
                continue;
            }
            walk(root, &path, depth + 1, excludes, listing);
        } else if kind.is_file() && !is_excluded(excludes, &relative) {
            listing
                .files
                .push(PaletteEntry::new(&path.to_string_lossy(), &relative));
        }
    }
}

/// `path` relative to `root`, with `/` separators on every platform.
///
/// Glob patterns, including those in `files.exclude`, use `/` separators. A
/// Windows path has to be converted before matching, otherwise `**/.git` never
/// matches on Windows.
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

/// Whether `relative` is one of the paths `files.exclude` hides.
///
/// Public because remote search results are filtered on the local side, not in
/// the remote environment. The server intentionally reads no settings: a
/// `settings.json` on the remote machine must not affect how it handles
/// `fs.read`. The user's excludes can therefore only be applied locally.
pub fn excluded_by_settings(settings: &Settings, relative: &str) -> bool {
    is_excluded(&exclude_patterns(settings), relative)
}

/// Lists one directory for the file tree.
///
/// Lists one level only. The tree reads a directory when it is expanded, so
/// opening a workspace costs one `read_dir` regardless of its size. The tree
/// does not reuse the quick-open listing because that listing is capped at
/// [`MAX_FILES`] and flattened. A tree built from it would be truncated and could
/// not show empty directories.
///
/// Applies the same exclusions as quick open, so `files.exclude` hides the same
/// files from `ctrl+p` and from the tree.
pub fn list_dir(root: &Path, dir: &Path, settings: &Settings) -> Vec<deco_editor::explorer::Entry> {
    let excludes = exclude_patterns(settings);
    // An unreadable directory is returned as empty rather than as an error, so a
    // workspace with one root-owned subdirectory still shows the other entries.
    // The walk handles this case the same way.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(relative) = relative_to(root, &path) else {
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if ALWAYS_SKIP.contains(&name) || is_excluded(&excludes, &relative) {
                continue;
            }
            out.push(deco_editor::explorer::Entry::dir(name));
        } else if kind.is_file() && !is_excluded(&excludes, &relative) {
            out.push(deco_editor::explorer::Entry::file(name));
        }
    }
    out
}

/// The enabled patterns from `files.exclude`.
///
/// The setting is a map of pattern to boolean. VS Code uses `false` to disable an
/// inherited pattern, so the value is checked, not only the key.
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
    fn one_directory_is_listed_without_walking_into_it() {
        let root = tree(
            "one-level",
            &[
                "a.rs",
                "src/b.rs",
                "src/deep/c.rs",
                "target/junk.o",
                ".git/HEAD",
            ],
        );
        let mut names: Vec<String> = list_dir(&root, &root, &Settings::with_defaults())
            .into_iter()
            .map(|entry| format!("{}{}", entry.name, if entry.is_dir { "/" } else { "" }))
            .collect();
        names.sort();
        // `src/` appears as one row without its contents, and the conventionally
        // skipped directories are not listed.
        assert_eq!(names, ["a.rs", "src/"]);

        let mut inner: Vec<String> = list_dir(&root, &root.join("src"), &Settings::with_defaults())
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        inner.sort();
        assert_eq!(inner, ["b.rs", "deep"]);
    }

    #[test]
    fn a_directory_that_cannot_be_read_lists_as_empty() {
        let root = tree("unreadable", &["a.rs"]);
        assert!(list_dir(&root, &root.join("nope"), &Settings::with_defaults()).is_empty());
    }

    #[test]
    fn files_exclude_hides_the_same_things_from_the_tree_as_from_quick_open() {
        let root = tree("excluded-tree", &["keep.rs", "skip.log"]);
        let mut settings = Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            "files.exclude",
            serde_json::json!({ "**/*.log": true }),
        );
        let names: Vec<String> = list_dir(&root, &root, &settings)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["keep.rs"]);
        assert_eq!(titles(&list(&root, &settings)), ["keep.rs"]);
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
        // VS Code uses `false` to disable an inherited pattern, so the value is
        // checked, not only the key.
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

    // ---- Project-wide search ---------------------------------------------

    fn matches(found: &Found) -> Vec<&str> {
        found.matches.iter().map(|m| m.title.as_str()).collect()
    }

    #[test]
    fn a_term_is_found_across_files_with_its_line_and_text() {
        let root = tree("search", &["a.rs", "b.rs"]);
        std::fs::write(root.join("a.rs"), "fn one() {}\nlet total = 1;\n").unwrap();
        std::fs::write(root.join("b.rs"), "// total\n").unwrap();
        let found = search(
            &root,
            &Settings::with_defaults(),
            "total",
            SearchOptions::EXACT,
        );
        assert_eq!(
            matches(&found),
            vec!["a.rs:2: let total = 1;", "b.rs:1: // total"]
        );
        assert!(!found.truncated);
        assert_eq!(found.files_searched, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_match_carries_the_position_to_land_on() {
        let root = tree("search-pos", &["a.rs"]);
        std::fs::write(root.join("a.rs"), "one\ntwo total\n").unwrap();
        let found = search(
            &root,
            &Settings::with_defaults(),
            "total",
            SearchOptions::EXACT,
        );
        let at = found.matches[0].at.expect("a result is a position");
        assert_eq!((at.line, at.character), (1, 4));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_search_options_are_the_find_bars() {
        // Project search and the find bar must report exactly the same matches.
        let root = tree("search-options", &["a.rs"]);
        std::fs::write(root.join("a.rs"), "Total\ntotalise\n").unwrap();
        let settings = Settings::with_defaults();

        let sensitive = search(&root, &settings, "total", SearchOptions::EXACT);
        assert_eq!(matches(&sensitive), vec!["a.rs:2: totalise"]);

        let insensitive = search(&root, &settings, "total", SearchOptions::default());
        assert_eq!(insensitive.matches.len(), 2, "`Total` matches too");

        let whole = search(
            &root,
            &settings,
            "total",
            SearchOptions {
                case_sensitive: false,
                whole_word: true,
                regex: false,
            },
        );
        assert_eq!(matches(&whole), vec!["a.rs:1: Total"]);

        let regex = search(
            &root,
            &settings,
            "^tot.l$",
            SearchOptions {
                case_sensitive: false,
                whole_word: false,
                regex: true,
            },
        );
        assert_eq!(matches(&regex), vec!["a.rs:1: Total"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_needle_finds_nothing_rather_than_everything() {
        let root = tree("search-empty", &["a.rs"]);
        let found = search(&root, &Settings::with_defaults(), "", SearchOptions::EXACT);
        assert!(found.matches.is_empty());
        assert_eq!(found.files_searched, 0, "nothing was even read");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_is_not_text_is_skipped_rather_than_matched() {
        let root = tree("search-binary", &["a.rs"]);
        std::fs::write(root.join("blob.bin"), [0xff, 0xfe, b'h', b'i', 0x00]).unwrap();
        std::fs::write(root.join("a.rs"), "hi\n").unwrap();
        let found = search(
            &root,
            &Settings::with_defaults(),
            "hi",
            SearchOptions::EXACT,
        );
        assert_eq!(matches(&found), vec!["a.rs:1: hi"]);
        assert_eq!(found.files_searched, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_over_the_size_limit_is_not_read() {
        let root = tree("search-big", &["a.rs"]);
        std::fs::write(root.join("a.rs"), "needle\n").unwrap();
        let big = "x".repeat((MAX_FILE_BYTES + 1) as usize) + "needle";
        std::fs::write(root.join("big.rs"), big).unwrap();
        let found = search(
            &root,
            &Settings::with_defaults(),
            "needle",
            SearchOptions::EXACT,
        );
        assert_eq!(matches(&found), vec!["a.rs:1: needle"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hitting_the_match_limit_is_reported() {
        let root = tree("search-many", &["a.rs"]);
        std::fs::write(root.join("a.rs"), "x\n".repeat(MAX_MATCHES + 10)).unwrap();
        let found = search(&root, &Settings::with_defaults(), "x", SearchOptions::EXACT);
        assert_eq!(found.matches.len(), MAX_MATCHES);
        assert!(found.truncated, "the reader has to be told it stopped");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_excluded_file_is_not_searched() {
        let root = tree("search-excluded", &["keep.rs", "vendor/dep.rs"]);
        std::fs::write(root.join("keep.rs"), "needle\n").unwrap();
        std::fs::write(root.join("vendor/dep.rs"), "needle\n").unwrap();
        let mut settings = Settings::with_defaults();
        settings
            .load_layer(
                deco_config::Scope::User,
                r#"{ "files.exclude": { "vendor": true } }"#,
            )
            .unwrap();
        let found = search(&root, &settings, "needle", SearchOptions::EXACT);
        assert_eq!(matches(&found), vec!["keep.rs:1: needle"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_very_long_line_is_shortened_for_the_list() {
        let root = tree("search-long", &["a.rs"]);
        std::fs::write(root.join("a.rs"), format!("{} needle", "y".repeat(400))).unwrap();
        let found = search(
            &root,
            &Settings::with_defaults(),
            "needle",
            SearchOptions::EXACT,
        );
        let title = &found.matches[0].title;
        assert!(title.ends_with('…'), "{title}");
        assert!(title.chars().count() < 200, "{}", title.chars().count());
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
        // The file limit itself is not exercised because writing ten thousand
        // files would make the test slow. The flag is checked through the depth
        // limit, which sets the same field.
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
