//! Installed extensions, and which of them may start running.
//!
//! This module decides *which* installed extensions start a host process, and
//! *when*. It performs no I/O. The frontend walks the filesystem, as it does
//! for themes, because the core has no filesystem access.
//!
//! # Only code extensions activate
//!
//! An extension with no `main` never starts a process. Themes and grammars are
//! read as data, which is why marketplace themes work in deco. They are in the
//! catalogue because their commands and contributions still exist, but
//! [`Catalogue::to_activate`] never returns one. A test checks this.
//!
//! # Activation is a security control
//!
//! An extension that has not activated has no process, so it cannot make any
//! capability request. Narrow activation events are therefore a simple
//! mitigation, and accidentally broadening them is costly. An event in
//! `activationEvents` that deco does not recognise never fires, instead of
//! being treated as `*`. See [`crate::activation`].

use std::path::{Path, PathBuf};

use crate::activation::{any_fires, parse_all, ActivationEvent, Trigger};
use crate::manifest::Manifest;

/// A command an extension contributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contributed {
    /// The command id, as `commands.registerCommand` will name it.
    pub command: String,
    /// The label for the palette.
    pub title: String,
    /// A grouping prefix, shown before the title as VS Code shows it.
    pub category: Option<String>,
}

impl Contributed {
    /// The command's label in a palette: `Category: Title`, or just the title.
    pub fn label(&self) -> String {
        match &self.category {
            Some(category) if !category.is_empty() => format!("{category}: {}", self.title),
            _ => self.title.clone(),
        }
    }
}

/// One installed extension, as far as starting it is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// `publisher.name`, or `local.name` for an unpublished one.
    pub id: String,
    /// The name shown in the UI.
    pub label: String,
    /// The directory it was found in.
    pub root: PathBuf,
    /// The version from its manifest.
    ///
    /// Kept because a remembered permission applies to *this* code. A new
    /// version is new code, and a grant must not carry across an update that
    /// the user has not reviewed.
    pub version: String,
    /// Its entry point, relative to `root`. `None` for a declarative extension,
    /// which never runs.
    pub main: Option<String>,
    /// Its parsed `activationEvents`.
    pub activation: Vec<ActivationEvent>,
    /// The commands it contributes.
    pub commands: Vec<Contributed>,
}

impl Installed {
    /// Whether this extension has code to run at all.
    pub fn runnable(&self) -> bool {
        self.main.is_some()
    }

    /// Whether it contributes `command`.
    pub fn contributes(&self, command: &str) -> bool {
        self.commands.iter().any(|c| c.command == command)
    }
}

/// Everything installed, with the collisions found on the way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogue {
    /// The extensions, in the order they were found.
    pub extensions: Vec<Installed>,
    /// Problems found while building the catalogue, as messages for the user.
    ///
    /// Collected instead of logged, and never a reason to fail: one broken
    /// extension directory must not stop the others from working. The frontend
    /// shows these the same way it shows settings problems.
    pub problems: Vec<String>,
}

impl Catalogue {
    /// Builds a catalogue from manifests already parsed and paired with their
    /// directories. On a conflict, the earlier entry takes precedence.
    pub fn build(entries: impl IntoIterator<Item = (PathBuf, Manifest)>) -> Self {
        let mut catalogue = Self::default();
        for (root, manifest) in entries {
            let id = manifest.identifier();
            if let Some(first) = catalogue.by_id(&id) {
                // The usual cause is the same extension installed at two
                // versions. The first one is used, as for themes.
                catalogue.problems.push(format!(
                    "extension {id} is installed twice; using {} and ignoring {}",
                    first.root.display(),
                    root.display()
                ));
                continue;
            }

            let mut commands = Vec::new();
            for contribution in &manifest.contributes.commands {
                if contribution.command.trim().is_empty() {
                    catalogue.problems.push(format!(
                        "extension {id} contributes a command with no id, which nothing could invoke"
                    ));
                    continue;
                }
                if let Some(owner) = catalogue.owner_of(&contribution.command) {
                    // Two extensions contribute the same command id. Reported
                    // because the second extension appears broken, and the
                    // reason is not shown anywhere else.
                    catalogue.problems.push(format!(
                        "command {} is contributed by both {} and {id}; {} keeps it",
                        contribution.command, owner.id, owner.id
                    ));
                    continue;
                }
                commands.push(Contributed {
                    command: contribution.command.clone(),
                    title: contribution.title.clone(),
                    category: contribution.category.clone(),
                });
            }

            catalogue.extensions.push(Installed {
                label: manifest.label().to_owned(),
                version: manifest.version.clone(),
                id,
                root,
                main: manifest.main.clone().filter(|main| !main.trim().is_empty()),
                activation: parse_all(&manifest.activation_events),
                commands,
            });
        }
        catalogue
    }

    /// The extension with this identifier.
    pub fn by_id(&self, id: &str) -> Option<&Installed> {
        self.extensions.iter().find(|e| e.id == id)
    }

    /// The extension that contributes `command`, if any.
    ///
    /// `None` for deco's own commands, which are most of the palette.
    pub fn owner_of(&self, command: &str) -> Option<&Installed> {
        self.extensions.iter().find(|e| e.contributes(command))
    }

    /// Every extension that has code to run.
    pub fn code_extensions(&self) -> impl Iterator<Item = &Installed> {
        self.extensions.iter().filter(|e| e.runnable())
    }

    /// Every contributed command, with the extension that contributed it.
    ///
    /// Used by the palette, which lists a command whether or not its extension
    /// has started. Invoking the command starts the extension.
    pub fn contributed_commands(&self) -> Vec<(&Installed, &Contributed)> {
        self.extensions
            .iter()
            .flat_map(|e| e.commands.iter().map(move |c| (e, c)))
            .collect()
    }

    /// Which extensions this trigger should start, in catalogue order.
    ///
    /// Only extensions with code are returned; a theme has nothing to activate.
    /// An invoked command activates the extension that contributes it even
    /// without an `onCommand:` event, as in VS Code 1.74 and later. Otherwise
    /// the palette entry would do nothing.
    pub fn to_activate(&self, trigger: &Trigger<'_>) -> Vec<&Installed> {
        self.extensions
            .iter()
            .filter(|extension| extension.runnable())
            .filter(|extension| {
                any_fires(&extension.activation, trigger)
                    || matches!(trigger, Trigger::Command(command) if extension.contributes(command))
            })
            .collect()
    }

    /// The directories of all runnable extensions, used for a container's
    /// read-only mounts. A host needs no other paths on the machine.
    pub fn roots(&self) -> Vec<&Path> {
        self.code_extensions().map(|e| e.root.as_path()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(source: &str) -> Manifest {
        Manifest::parse(source).expect("a manifest")
    }

    /// An extension directory, as an absolute path for this platform.
    fn root(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\ext\\{name}"))
        } else {
            PathBuf::from(format!("/ext/{name}"))
        }
    }

    fn code_extension() -> (PathBuf, Manifest) {
        (
            root("acme.tools"),
            manifest(
                r#"{
  "name": "tools",
  "publisher": "acme",
  "displayName": "Acme Tools",
  "main": "./out/extension.js",
  "activationEvents": ["onLanguage:rust", "onCommand:acme.doThing"],
  "contributes": {
    "commands": [
      { "command": "acme.doThing", "title": "Do The Thing", "category": "Acme" },
      { "command": "acme.other", "title": "Something Else" }
    ]
  }
}"#,
            ),
        )
    }

    fn theme_extension() -> (PathBuf, Manifest) {
        (
            root("someone.theme"),
            manifest(
                r#"{
  "name": "theme",
  "publisher": "someone",
  "activationEvents": ["*"],
  "contributes": { "themes": [{ "label": "Midnight", "uiTheme": "vs-dark", "path": "./t.json" }] }
}"#,
            ),
        )
    }

    #[test]
    fn a_manifest_becomes_what_starting_it_needs() {
        let catalogue = Catalogue::build([code_extension()]);
        assert!(catalogue.problems.is_empty(), "{:?}", catalogue.problems);
        let extension = catalogue.by_id("acme.tools").expect("by id");
        assert_eq!(extension.label, "Acme Tools");
        assert_eq!(extension.main.as_deref(), Some("./out/extension.js"));
        assert!(extension.runnable());
        assert_eq!(extension.commands.len(), 2);
        assert_eq!(extension.commands[0].label(), "Acme: Do The Thing");
        assert_eq!(extension.commands[1].label(), "Something Else");
    }

    #[test]
    fn a_declarative_extension_is_catalogued_and_never_activated() {
        // `*` fires at startup for any extension with code. A theme has no code,
        // so starting a sandboxed process for it would serve no purpose.
        let catalogue = Catalogue::build([theme_extension()]);
        let theme = catalogue.by_id("someone.theme").expect("by id");
        assert!(!theme.runnable());
        assert!(catalogue.to_activate(&Trigger::StartupFinished).is_empty());
        assert_eq!(catalogue.code_extensions().count(), 0);
        assert!(catalogue.roots().is_empty());
    }

    #[test]
    fn an_unpublished_extension_keeps_the_local_prefix() {
        let catalogue = Catalogue::build([(
            root("mine"),
            manifest(r#"{ "name": "mine", "main": "./m.js" }"#),
        )]);
        assert!(catalogue.by_id("local.mine").is_some());
    }

    #[test]
    fn the_trigger_decides_and_only_code_answers() {
        let catalogue = Catalogue::build([code_extension(), theme_extension()]);

        let started = catalogue.to_activate(&Trigger::Language("rust"));
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].id, "acme.tools");

        assert!(catalogue.to_activate(&Trigger::Language("go")).is_empty());
        // `*` belongs to the theme, which cannot run.
        assert!(catalogue.to_activate(&Trigger::StartupFinished).is_empty());
    }

    #[test]
    fn a_contributed_command_activates_its_extension_without_an_on_command_event() {
        // VS Code stopped requiring the event in 1.74. deco does the same because
        // otherwise the palette entry would do nothing when the user invokes it.
        let catalogue = Catalogue::build([(
            root("acme.quiet"),
            manifest(
                r#"{
  "name": "quiet",
  "publisher": "acme",
  "main": "./m.js",
  "activationEvents": [],
  "contributes": { "commands": [{ "command": "acme.quiet.go", "title": "Go" }] }
}"#,
            ),
        )]);
        let started = catalogue.to_activate(&Trigger::Command("acme.quiet.go"));
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].id, "acme.quiet");

        // No other trigger activates it. An empty `activationEvents` is not a
        // wildcard.
        assert!(catalogue.to_activate(&Trigger::StartupFinished).is_empty());
        assert!(catalogue.to_activate(&Trigger::Language("rust")).is_empty());
    }

    #[test]
    fn decos_own_commands_belong_to_nobody() {
        let catalogue = Catalogue::build([code_extension()]);
        assert!(catalogue.owner_of("editor.action.commentLine").is_none());
        assert!(catalogue
            .to_activate(&Trigger::Command("editor.action.commentLine"))
            .is_empty());
        assert_eq!(
            catalogue.owner_of("acme.doThing").map(|e| e.id.as_str()),
            Some("acme.tools")
        );
    }

    #[test]
    fn an_unknown_activation_event_starts_nothing() {
        let catalogue = Catalogue::build([(
            root("acme.future"),
            manifest(
                r#"{
  "name": "future",
  "publisher": "acme",
  "main": "./m.js",
  "activationEvents": ["onSomethingNewVSCodeAdded:x"]
}"#,
            ),
        )]);
        for trigger in [
            Trigger::StartupFinished,
            Trigger::Language("rust"),
            Trigger::Command("x"),
            Trigger::Uri,
            Trigger::View("x"),
        ] {
            assert!(
                catalogue.to_activate(&trigger).is_empty(),
                "an event deco does not model fired for {trigger:?}"
            );
        }
    }

    #[test]
    fn the_same_extension_installed_twice_keeps_the_first_and_says_so() {
        let (_, manifest_one) = code_extension();
        let (_, manifest_two) = code_extension();
        let catalogue = Catalogue::build([
            (root("acme.tools-1.0.0"), manifest_one),
            (root("acme.tools-2.0.0"), manifest_two),
        ]);
        assert_eq!(catalogue.extensions.len(), 1);
        assert_eq!(catalogue.extensions[0].root, root("acme.tools-1.0.0"));
        assert_eq!(catalogue.problems.len(), 1);
        assert!(catalogue.problems[0].contains("installed twice"));
        assert!(catalogue.problems[0].contains("acme.tools-2.0.0"));
    }

    #[test]
    fn two_extensions_cannot_both_own_one_command() {
        // Without a reported problem, the second extension would appear broken:
        // the command is in the palette but activates the other extension, and
        // no message explains why.
        let catalogue = Catalogue::build([
            code_extension(),
            (
                root("rival.tools"),
                manifest(
                    r#"{
  "name": "tools",
  "publisher": "rival",
  "main": "./m.js",
  "contributes": { "commands": [{ "command": "acme.doThing", "title": "Mine Now" }] }
}"#,
                ),
            ),
        ]);
        assert_eq!(
            catalogue.owner_of("acme.doThing").map(|e| e.id.as_str()),
            Some("acme.tools")
        );
        let rival = catalogue.by_id("rival.tools").expect("still catalogued");
        assert!(rival.commands.is_empty(), "{:?}", rival.commands);
        assert_eq!(catalogue.problems.len(), 1);
        assert!(
            catalogue.problems[0].contains("acme.doThing"),
            "{:?}",
            catalogue.problems
        );
    }

    #[test]
    fn a_command_with_no_id_is_refused_rather_than_offered() {
        let catalogue = Catalogue::build([(
            root("acme.broken"),
            manifest(
                r#"{
  "name": "broken",
  "publisher": "acme",
  "main": "./m.js",
  "contributes": { "commands": [{ "command": "  ", "title": "Nothing" }] }
}"#,
            ),
        )]);
        assert!(catalogue.contributed_commands().is_empty());
        assert_eq!(catalogue.problems.len(), 1);
        assert!(catalogue.problems[0].contains("no id"));
    }

    #[test]
    fn an_empty_main_is_the_same_as_none() {
        // Otherwise `"main": ""` would resolve to the extension directory itself,
        // and the host would try to run it as JavaScript.
        let catalogue = Catalogue::build([(
            root("acme.blank"),
            manifest(
                r#"{ "name": "blank", "publisher": "acme", "main": "", "activationEvents": ["*"] }"#,
            ),
        )]);
        assert!(!catalogue.extensions[0].runnable());
        assert!(catalogue.to_activate(&Trigger::StartupFinished).is_empty());
    }

    #[test]
    fn the_palette_gets_every_contributed_command_with_its_extension() {
        let catalogue = Catalogue::build([code_extension(), theme_extension()]);
        let listed: Vec<(&str, String)> = catalogue
            .contributed_commands()
            .into_iter()
            .map(|(extension, command)| (extension.id.as_str(), command.label()))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("acme.tools", "Acme: Do The Thing".to_owned()),
                ("acme.tools", "Something Else".to_owned()),
            ]
        );
    }

    #[test]
    fn only_the_directories_of_things_that_run_are_mountable() {
        let catalogue = Catalogue::build([code_extension(), theme_extension()]);
        assert_eq!(catalogue.roots(), vec![root("acme.tools").as_path()]);
    }

    #[test]
    fn nothing_installed_is_not_a_problem() {
        let catalogue = Catalogue::build([]);
        assert!(catalogue.problems.is_empty());
        assert!(catalogue.to_activate(&Trigger::StartupFinished).is_empty());
        assert!(catalogue.contributed_commands().is_empty());
    }
}
