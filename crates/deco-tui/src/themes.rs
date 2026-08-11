//! Finding the colour themes that are installed.
//!
//! Here rather than in `deco-editor` for the same reason the file walk is: the
//! core has no filesystem, and a theme that lives in an extension directory has
//! to be read from one.
//!
//! # What counts as installed
//!
//! The two themes deco ships with, plus every `contributes.themes` entry of every
//! extension under deco's own extensions directory and VS Code's. A theme
//! extension has no `main`, never starts a host process and needs no capability —
//! which is why one from the marketplace works here at all.
//!
//! Nothing is *loaded* while listing. A picker over forty themes would otherwise
//! parse forty JSON files, and thirty-nine of them for nothing.

use std::path::{Path, PathBuf};

use deco_editor::commands::PaletteEntry;

/// How many extension directories are examined before the walk gives up.
///
/// A marketplace-managed directory holds tens of extensions, not thousands; a
/// number this size only ever stops something pathological.
pub const MAX_EXTENSIONS: usize = 2_000;

/// A theme that can be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    /// What to call it — the extension's own `label`, which is what a
    /// `workbench.colorTheme` setting has to name.
    pub label: String,
    /// The file to read, or `None` for one compiled in.
    pub path: Option<PathBuf>,
    /// `dark`, `light` or `high contrast`, from the contribution's `uiTheme`.
    ///
    /// Worth showing: it is the part of the choice a label often does not say, and
    /// it is what tells you whether the screen is about to go white.
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

    // Built-ins stay at the top, because they are the ones that always work, and
    // the rest go in the order the picker lists them.
    all[builtins..].sort_by(|a, b| a.label.cmp(&b.label));

    // One label can be contributed twice — the same extension installed under two
    // versions is the common way. The first wins, which is a built-in when a
    // marketplace theme happens to share a name with one.
    all.dedup_by(|a, b| a.label == b.label);
    all
}

/// The themes one extension directory contributes.
fn contributed(root: &Path) -> Vec<Available> {
    let Ok(source) = std::fs::read_to_string(root.join("package.json")) else {
        // A directory that is not an extension. Nothing to report: an extensions
        // directory routinely holds `.obsolete` and other bookkeeping.
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
                // `vs-dark` and anything unrecognised. Dark is VS Code's own
                // default for a contribution that does not say.
                _ => "dark",
            },
        })
        .collect()
}

/// The available themes as picker rows.
///
/// The identifier is the path to read, empty for a built-in — which is what
/// `Session::accept_prompt` hands back to the frontend to load.
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
        // VS Code's own default for a contribution that does not say.
        assert_eq!(kind("Unstated"), "dark");
    }

    #[test]
    fn a_directory_that_is_not_an_extension_is_skipped_quietly() {
        // An extensions directory routinely holds `.obsolete` and other
        // bookkeeping, which is not a problem worth reporting.
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
    fn contributed_themes_sort_by_label_below_the_builtins() {
        // The built-ins are the ones that always work, so they stay reachable at
        // the top rather than being buried by whatever is installed.
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
