//! Finding the colour themes that are installed.
//!
//! This module is in this crate rather than in `deco-editor` for the same reason
//! as the file walk: the core has no filesystem access, and a theme in an
//! extension directory has to be read from the filesystem.
//!
//! # What counts as installed
//!
//! The two themes deco ships with, plus every `contributes.themes` entry of every
//! extension under deco's own extensions directory and VS Code's. A theme
//! extension has no `main`, never starts a host process and needs no capability.
//! This is why marketplace themes work in deco.
//!
//! Listing does not *load* any theme, so opening a picker over many themes does
//! not parse their JSON files.

use std::path::{Path, PathBuf};

use deco_editor::commands::PaletteEntry;

/// How many extension directories are examined before the walk gives up.
///
/// A marketplace-managed directory holds tens of extensions, not thousands. The
/// limit only applies to abnormal directories.
pub const MAX_EXTENSIONS: usize = 2_000;

/// A theme that can be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    /// The display name: the extension's `label`. A `workbench.colorTheme`
    /// setting refers to a theme by this name.
    pub label: String,
    /// The file to read, or `None` for a built-in theme.
    pub path: Option<PathBuf>,
    /// `dark`, `light` or `high contrast`, from the contribution's `uiTheme`.
    ///
    /// Shown in the picker because the label often does not indicate whether the
    /// theme is light or dark.
    pub kind: &'static str,
}

/// Every theme deco could switch to, built-ins first and then by label.
///
/// `roots` are extension directories; missing ones are skipped rather than
/// reported, because not having installed any extensions is the normal case.
pub fn list(roots: &[PathBuf]) -> Vec<Available> {
    let mut all: Vec<Available> = deco_theme::defaults::BUILTIN_THEME_NAMES
        .iter()
        .map(|name| Available {
            label: (*name).to_owned(),
            path: None,
            kind: if name.contains("Light") {
                "light"
            } else {
                "dark"
            },
        })
        .collect();
    let builtins = all.len();

    let mut examined = 0usize;
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if examined >= MAX_EXTENSIONS {
                break;
            }
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            examined += 1;
            all.extend(contributed(&entry.path()));
        }
    }

    // Built-ins stay at the top because they always work. Contributed themes are
    // sorted by label, then by path, because `read_dir` returns entries in no
    // particular order.
    all[builtins..].sort_by(|a, b| (&a.label, &a.path).cmp(&(&b.label, &b.path)));

    // One label can be contributed twice, usually by the same extension installed
    // in two versions. The first entry is kept, which is a built-in when a
    // marketplace theme has the same name as one. Duplicates need not be
    // adjacent, because the built-ins are not sorted with the rest.
    let mut seen = std::collections::HashSet::new();
    all.retain(|theme| seen.insert(theme.label.clone()));
    all
}

/// The themes one extension directory contributes.
fn contributed(root: &Path) -> Vec<Available> {
    let Ok(source) = std::fs::read_to_string(root.join("package.json")) else {
        // A directory that is not an extension. This is not reported: an
        // extensions directory usually contains `.obsolete` and other metadata.
        return Vec::new();
    };
    let Ok(manifest) = deco_ext::Manifest::parse(&source) else {
        return Vec::new();
    };
    manifest
        .contributes
        .themes
        .iter()
        .filter(|theme| !theme.label.is_empty())
        .map(|theme| Available {
            label: theme.label.clone(),
            // Joined onto the extension root, since the manifest's path is
            // relative to it.
            path: Some(root.join(&theme.path)),
            kind: match theme.ui_theme.as_deref() {
                Some("vs") => "light",
                Some("hc-black") | Some("hc-light") => "high contrast",
                // `vs-dark` and anything unrecognised. VS Code also defaults to
                // dark when a contribution has no `uiTheme`.
                _ => "dark",
            },
        })
        .collect()
}

/// The available themes as picker rows.
///
/// The identifier is the path to read, or empty for a built-in.
/// `Session::accept_prompt` returns this identifier to the frontend to load.
pub fn rows(available: &[Available]) -> Vec<PaletteEntry> {
    available
        .iter()
        .map(|theme| {
            let id = theme
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            PaletteEntry::new(&id, &theme.label).with_detail(theme.kind)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An extension directory holding `manifest`, and optionally a theme file.
    fn extension(root: &Path, name: &str, manifest: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), manifest).unwrap();
        dir
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deco-themes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_builtin_themes_are_always_offered() {
        let found = list(&[]);
        let labels: Vec<&str> = found.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["Default Dark Modern", "Default Light Modern"]);
        assert!(found.iter().all(|t| t.path.is_none()));
        assert_eq!(found[0].kind, "dark");
        assert_eq!(found[1].kind, "light");
    }

    #[test]
    fn a_contributed_theme_is_offered_with_its_path_joined_to_the_extension() {
        let root = temp("contributed");
        extension(
            &root,
            "someone.night",
            r#"{ "name": "night", "contributes": { "themes": [
                 { "label": "Night Owl", "uiTheme": "vs-dark", "path": "./themes/owl.json" }
               ] } }"#,
        );

        let found = list(std::slice::from_ref(&root));
        let owl = found
            .iter()
            .find(|t| t.label == "Night Owl")
            .expect("the contribution should be offered");
        assert_eq!(
            owl.path.as_deref(),
            Some(
                root.join("someone.night")
                    .join("./themes/owl.json")
                    .as_path()
            )
        );
        assert_eq!(owl.kind, "dark");
    }

    #[test]
    fn ui_theme_says_what_the_label_often_does_not() {
        let root = temp("kinds");
        extension(
            &root,
            "someone.pack",
            r#"{ "name": "pack", "contributes": { "themes": [
                 { "label": "Paper", "uiTheme": "vs", "path": "./p.json" },
                 { "label": "Contrast", "uiTheme": "hc-black", "path": "./c.json" },
                 { "label": "Unstated", "path": "./u.json" }
               ] } }"#,
        );
        let found = list(&[root]);
        let kind = |label: &str| {
            found
                .iter()
                .find(|t| t.label == label)
                .map(|t| t.kind)
                .unwrap()
        };
        assert_eq!(kind("Paper"), "light");
        assert_eq!(kind("Contrast"), "high contrast");
        // VS Code's default for a contribution without `uiTheme`.
        assert_eq!(kind("Unstated"), "dark");
    }

    #[test]
    fn a_directory_that_is_not_an_extension_is_skipped_quietly() {
        // An extensions directory usually contains `.obsolete` and other
        // metadata. These are not errors.
        let root = temp("not-an-extension");
        std::fs::create_dir_all(root.join(".obsolete")).unwrap();
        extension(&root, "broken", "{ not json");
        extension(&root, "no-themes", r#"{ "name": "plain" }"#);
        assert_eq!(list(&[root]).len(), 2, "just the built-ins");
    }

    #[test]
    fn a_missing_extensions_directory_is_not_an_error() {
        // Having installed no extensions is the normal case.
        let found = list(&[PathBuf::from("/nowhere/at/all")]);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn one_label_is_offered_once() {
        // The same extension installed under two versions is the usual cause.
        let root = temp("duplicates");
        for version in ["someone.night-1.0.0", "someone.night-1.1.0"] {
            extension(
                &root,
                version,
                r#"{ "name": "night", "contributes": { "themes": [
                     { "label": "Night Owl", "path": "./o.json" }
                   ] } }"#,
            );
        }
        let found = list(&[root]);
        assert_eq!(
            found.iter().filter(|t| t.label == "Night Owl").count(),
            1,
            "{found:?}"
        );
    }

    #[test]
    fn a_builtin_label_is_offered_once_as_the_builtin() {
        let root = temp("builtin-dark");
        extension(
            &root,
            "someone.modern",
            r#"{ "name": "modern", "contributes": { "themes": [
                 { "label": "Default Dark Modern", "path": "./d.json" }
               ] } }"#,
        );
        let found = list(&[root]);
        let matching: Vec<&Available> = found
            .iter()
            .filter(|t| t.label == "Default Dark Modern")
            .collect();
        assert_eq!(matching.len(), 1, "{found:?}");
        assert_eq!(matching[0].path, None, "the built-in should be kept");
    }

    #[test]
    fn a_builtin_label_is_offered_once_among_other_contributions() {
        // "Aardvark" sorts between the built-ins and the duplicate, so the two
        // entries with the same label are not adjacent.
        let root = temp("builtin-light");
        extension(
            &root,
            "someone.modern",
            r#"{ "name": "modern", "contributes": { "themes": [
                 { "label": "Default Light Modern", "uiTheme": "vs", "path": "./l.json" },
                 { "label": "Aardvark", "path": "./a.json" }
               ] } }"#,
        );
        let found = list(&[root]);
        let labels: Vec<&str> = found.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Default Dark Modern", "Default Light Modern", "Aardvark"]
        );
        assert_eq!(found[1].path, None, "the built-in should be kept");
    }

    #[test]
    fn contributed_themes_sort_by_label_below_the_builtins() {
        // The built-ins always work, so they stay at the top above installed
        // themes.
        let root = temp("order");
        extension(
            &root,
            "someone.pack",
            r#"{ "name": "pack", "contributes": { "themes": [
                 { "label": "Zebra", "path": "./z.json" },
                 { "label": "Aardvark", "path": "./a.json" }
               ] } }"#,
        );
        let labels: Vec<String> = list(&[root]).into_iter().map(|t| t.label).collect();
        assert_eq!(
            labels,
            [
                "Default Dark Modern",
                "Default Light Modern",
                "Aardvark",
                "Zebra"
            ]
        );
    }

    #[test]
    fn a_row_carries_the_path_to_read_and_the_kind_to_show() {
        let available = vec![
            Available {
                label: "Default Dark Modern".to_owned(),
                path: None,
                kind: "dark",
            },
            Available {
                label: "Night Owl".to_owned(),
                path: Some(PathBuf::from("/ext/owl.json")),
                kind: "dark",
            },
        ];
        let rows = rows(&available);
        assert_eq!(rows[0].id, "", "a built-in has no file");
        assert_eq!(rows[0].title, "Default Dark Modern");
        assert_eq!(rows[0].detail.as_deref(), Some("dark"));
        assert_eq!(rows[1].id, "/ext/owl.json");
    }
}
