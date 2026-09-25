//! Reading the language-server registry out of layered settings.
//!
//! The key is `deco.lsp.servers`, in deco's own namespace. VS Code has no
//! equivalent setting because it gets servers from extensions. A definition
//! looks like:
//!
//! ```jsonc
//! {
//!   "deco.lsp.enabled": true,
//!   "deco.lsp.servers": {
//!     "rust-analyzer": {
//!       "languages": ["rust"],
//!       "command": "rust-analyzer",
//!       "args": [],
//!       "initializationOptions": { "cargo": { "features": "all" } }
//!     }
//!   }
//! }
//! ```
//!
//! # Why this is not just `settings.get("deco.lsp.servers")`
//!
//! Ordinary settings are resolved by precedence: the highest-priority layer
//! wins, and nothing depends on which layer the value came from. A server
//! definition is different because using it means **executing a program**. A
//! `.vscode/settings.json` can come from a cloned repository, so a definition
//! from workspace scope must keep that marking until the process is spawned.
//! Therefore each layer is read separately and tagged with its [`Trust`]
//! instead of being resolved into a single value.
//!
//! Layers are still applied in VS Code's order, so a workspace *can* override a
//! user-defined server. The override keeps the workspace's trust level and
//! does not become trusted: see [`crate::server::ServerRegistry::merge`].

use deco_config::{Scope, Settings};

use crate::server::{ConfigError, ServerRegistry, Trust};

/// The setting holding server definitions.
pub const SERVERS_KEY: &str = "deco.lsp.servers";

/// The setting that turns language-server support off entirely.
pub const ENABLED_KEY: &str = "deco.lsp.enabled";

/// Which [`Trust`] a settings scope confers.
fn trust_of(scope: Scope) -> Trust {
    match scope {
        // Defaults ship with deco. The user settings file is written by the user.
        Scope::Default => Trust::BuiltIn,
        Scope::User => Trust::User,
        // Untrusted, even though the user chose to connect.
        //
        // Connecting to a machine does not mean trusting every file on it. The
        // remote layer can be written by anyone with an account on that
        // machine, which is the same problem as a `.vscode/settings.json` in a
        // cloned repository. The rest of deco already treats this scope as
        // untrusted: `deco_ext::sandbox` does not let it choose the extension
        // sandbox. A definition here is a *program to execute*, so it requires
        // confirmation in the same way as a workspace definition.
        Scope::Remote => Trust::Workspace,
        // Both come with the project's files, so both require confirmation.
        Scope::Workspace | Scope::Folder => Trust::Workspace,
    }
}

/// Whether language-server support is on. Defaults to on.
pub fn enabled(settings: &Settings) -> bool {
    settings.get_bool(ENABLED_KEY, None).unwrap_or(true)
}

/// Builds the registry from every layer, plus whatever could not be read.
///
/// Problems are returned instead of causing a failure. A malformed definition
/// disables only its own server. Other servers, and opening the settings file
/// to fix it, are unaffected.
pub fn registry(settings: &Settings) -> (ServerRegistry, Vec<ConfigError>) {
    let mut registry = crate::server::built_in();
    let mut problems = Vec::new();

    // Lowest precedence first, so a later layer's definition replaces an
    // earlier one. The replacement keeps the later layer's trust.
    for scope in Scope::ALL {
        let Some(layer) = settings.layer(scope) else {
            continue;
        };
        let Some(value) = layer.get(SERVERS_KEY) else {
            continue;
        };
        let (layer_registry, layer_problems) = ServerRegistry::from_json(value, trust_of(scope));
        registry.merge(layer_registry);
        problems.extend(layer_problems);
    }

    (registry, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(layers: &[(Scope, &str)]) -> Settings {
        let mut settings = Settings::with_defaults();
        for (scope, source) in layers {
            settings
                .load_layer(*scope, source)
                .unwrap_or_else(|error| panic!("{scope:?} layer: {error}"));
        }
        settings
    }

    #[test]
    fn language_servers_are_on_unless_turned_off() {
        assert!(enabled(&Settings::with_defaults()));
        assert!(!enabled(&settings(&[(
            Scope::User,
            r#"{"deco.lsp.enabled": false}"#
        )])));
    }

    #[test]
    fn the_built_in_servers_are_present_without_any_configuration() {
        let (registry, problems) = registry(&Settings::with_defaults());
        assert!(problems.is_empty());
        assert!(!registry.is_empty());
        assert_eq!(registry.for_language("rust").len(), 1);
    }

    #[test]
    fn a_user_definition_is_trusted() {
        let (registry, problems) = registry(&settings(&[(
            Scope::User,
            r#"{"deco.lsp.servers": {"mine": {"languages": ["toml"], "command": "taplo"}}}"#,
        )]));
        assert!(problems.is_empty());
        let server = registry.get("mine").expect("defined");
        assert_eq!(server.trust, Trust::User);
        assert!(!server.trust.needs_confirmation());
    }

    #[test]
    fn a_workspace_definition_needs_confirmation() {
        // Layers are read separately for this case. A `.vscode/settings.json`
        // can come from a cloned repository, and cloning must not be enough to
        // run a program.
        let (registry, _) = registry(&settings(&[(
            Scope::Workspace,
            r#"{"deco.lsp.servers": {"theirs": {"languages": ["rust"], "command": "./ra"}}}"#,
        )]));
        assert!(registry.get("theirs").unwrap().trust.needs_confirmation());
    }

    #[test]
    fn a_folder_definition_needs_confirmation_too() {
        let (registry, _) = registry(&settings(&[(
            Scope::Folder,
            r#"{"deco.lsp.servers": {"theirs": {"languages": ["rust"], "command": "./ra"}}}"#,
        )]));
        assert!(registry.get("theirs").unwrap().trust.needs_confirmation());
    }

    #[test]
    fn a_remote_definition_needs_confirmation_like_a_workspaces() {
        // Connecting to a machine does not mean trusting every file on it. The
        // remote layer can be written by anyone with an account on that
        // machine. `deco_ext::sandbox` already does not let this scope choose
        // the extension sandbox. A server definition is a *program to
        // execute*, so its policy must be at least as strict.
        let (registry, _) = registry(&settings(&[(
            Scope::Remote,
            r#"{"deco.lsp.servers": {"theirs": {"languages": ["rust"], "command": "./ra"}}}"#,
        )]));
        assert!(registry.get("theirs").unwrap().trust.needs_confirmation());
    }

    #[test]
    fn a_remote_override_of_a_user_server_still_needs_confirmation() {
        // `remote` is the layer directly above the user layer. It wins on
        // precedence but must not inherit the trust of the entry it replaces.
        let (registry, _) = registry(&settings(&[
            (
                Scope::User,
                r#"{"deco.lsp.servers": {"ra": {"languages": ["rust"], "command": "rust-analyzer"}}}"#,
            ),
            (
                Scope::Remote,
                r#"{"deco.lsp.servers": {"ra": {"languages": ["rust"], "command": "./evil"}}}"#,
            ),
        ]));
        let server = registry.get("ra").expect("defined");
        assert_eq!(server.command.program, "./evil", "the override applies");
        assert!(
            server.trust.needs_confirmation(),
            "and it is still not trusted"
        );
    }

    #[test]
    fn a_workspace_override_of_a_user_server_still_needs_confirmation() {
        // Otherwise an untrusted command could reuse a familiar id, such as
        // `rust-analyzer`, and become trusted.
        let (registry, _) = registry(&settings(&[
            (
                Scope::User,
                r#"{"deco.lsp.servers": {"ra": {"languages": ["rust"], "command": "rust-analyzer"}}}"#,
            ),
            (
                Scope::Workspace,
                r#"{"deco.lsp.servers": {"ra": {"languages": ["rust"], "command": "./evil"}}}"#,
            ),
        ]));
        let server = registry.get("ra").expect("defined");
        assert_eq!(server.command.program, "./evil", "the override applies");
        assert!(
            server.trust.needs_confirmation(),
            "and it is still not trusted"
        );
    }

    #[test]
    fn a_workspace_override_of_a_built_in_still_needs_confirmation() {
        // A built-in id is a likely target because the user has seen it work.
        let (registry, _) = registry(&settings(&[(
            Scope::Workspace,
            r#"{"deco.lsp.servers": {"rust-analyzer": {"languages": ["rust"], "command": "./evil"}}}"#,
        )]));
        let server = registry.get("rust-analyzer").expect("defined");
        assert_eq!(server.command.program, "./evil");
        assert!(server.trust.needs_confirmation());
    }

    #[test]
    fn a_user_layer_can_replace_a_built_in() {
        let (registry, _) = registry(&settings(&[(
            Scope::User,
            r#"{"deco.lsp.servers": {"rust-analyzer": {"languages": ["rust"], "command": "/opt/ra"}}}"#,
        )]));
        assert_eq!(
            registry.get("rust-analyzer").unwrap().command.program,
            "/opt/ra"
        );
        assert_eq!(
            registry.for_language("rust").len(),
            1,
            "replaced, not duplicated"
        );
    }

    #[test]
    fn one_broken_definition_does_not_cost_the_others() {
        let (registry, problems) = registry(&settings(&[(
            Scope::User,
            r#"{"deco.lsp.servers": {
                "good": {"languages": ["toml"], "command": "taplo"},
                "nocommand": {"languages": ["go"]}
            }}"#,
        )]));
        assert!(registry.get("good").is_some());
        assert_eq!(problems.len(), 1);
        assert!(matches!(problems[0], ConfigError::NoCommand { .. }));
    }

    #[test]
    fn a_definition_that_is_not_an_object_is_ignored_quietly() {
        let (registry, problems) = registry(&settings(&[(
            Scope::User,
            r#"{"deco.lsp.servers": "not an object"}"#,
        )]));
        assert!(problems.is_empty());
        // The built-ins survive; only the malformed layer contributes nothing.
        assert!(!registry.is_empty());
    }

    #[test]
    fn a_control_character_in_a_command_is_reported() {
        let (_, problems) = registry(&settings(&[(
            Scope::User,
            "{\"deco.lsp.servers\": {\"bad\": {\"languages\": [\"rust\"], \"command\": \"ra\\nevil\"}}}",
        )]));
        assert!(
            matches!(problems[0], ConfigError::ControlCharacter { .. }),
            "{problems:?}"
        );
    }

    #[test]
    fn every_scope_maps_to_a_trust_level() {
        // Checks the intended classification of every scope, not only that the
        // match compiles. A new scope must be classified explicitly.
        for scope in Scope::ALL {
            let trust = trust_of(scope);
            // Every scope other than deco's defaults and the user's own file
            // requires confirmation. `Remote` is included because files on a
            // connected machine are not written by the user.
            let expected_confirmation =
                matches!(scope, Scope::Workspace | Scope::Folder | Scope::Remote);
            assert_eq!(
                trust.needs_confirmation(),
                expected_confirmation,
                "{scope:?} was classified as {trust:?}"
            );
        }
    }
}
