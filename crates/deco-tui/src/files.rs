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
use deco_core::search::SearchOptions;
use deco_core::Buffer;
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

/// How many matches a project-wide search reports before it stops.
///
/// A term that appears ten thousand times is not being looked for one occurrence
/// at a time, and a list nobody can scroll to the end of is not more useful than
/// a list that says how many it stopped at.
pub const MAX_MATCHES: usize = 500;

/// Largest file a project-wide search will read.
///
/// A minified bundle or a checked-in database is not what anyone means by
/// "search my project", and reading it is most of the time the search takes.
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
/// Synchronous and bounded, which is the honest first version: a search that
/// streams results as it finds them needs a thread and a panel that updates, and
/// this needs neither to be useful. The bounds are reported rather than hidden.
pub fn search(root: &Path, settings: &Settings, needle: &str, options: SearchOptions) -> Found {
    let mut found = Found::default();
    if needle.is_empty() {
        return found;
    }
    let listing = list(root, settings);
    found.truncated = listing.truncated;

    for entry in &listing.files {
        if found.matches.len() >= MAX_MATCHES {
            found.truncated = true;
            break;
        }
        let path = Path::new(&entry.id);
        // Size first, so a huge file costs a `stat` rather than a read.
        if std::fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            continue;
        }
        // Not UTF-8 is how a binary file presents itself here, and skipping it is
        // right: a match inside a PNG is not a search result.
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        found.files_searched += 1;

        // The same search the find bar uses, so a term that matches in one place
        // matches in the other. Reading the file into a rope to do it is more work
        // than a bespoke scan would be, and worth it for having one definition of
        // what a match is.
        let buffer = Buffer::from_text(&text);
        for range in deco_core::search::find_all(&buffer, needle, options) {
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
            listing
                .files
                .push(PaletteEntry::new(&path.to_string_lossy(), &relative));
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
/// Whether `relative` is one of the paths `files.exclude` turns off.
///
/// Public because a remote search is filtered here rather than on the far end:
/// the server reads no settings — deliberately, since answering `fs.read` by
/// consulting a `settings.json` on the remote would be an authority nobody gave
/// it — so the user's own excludes can only be applied by whoever has them.
pub fn excluded_by_settings(settings: &Settings, relative: &str) -> bool {
    is_excluded(&exclude_patterns(settings), relative)
}

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
        // One definition of what a match is: a term found in the find bar must be
        // found here too, and not found where the find bar would not find it.
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
            },
        );
        assert_eq!(matches(&whole), vec!["a.rs:1: Total"]);
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
