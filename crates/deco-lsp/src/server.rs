//! Which language server to run for a language, and how.
//!
//! A server is a program the editor launches on the user's behalf, with the
//! user's privileges, named by a configuration file that may have arrived with
//! a cloned repository. Two consequences run through this module:
//!
//! - **A command is an argument vector, never a shell string.** The same rule
//!   `deco-remote` follows, for the same reason: `sh -c "$cmd"` with a `cmd`
//!   somebody else wrote is remote code execution wearing a configuration
//!   file's clothes. There is no field here that a shell ever sees.
//! - **A workspace cannot silently introduce one.** Settings layer as they do
//!   everywhere in deco, but a server definition arriving from workspace scope
//!   is marked [`Trust::Workspace`] so the editor can require confirmation
//!   before running it. Nothing in this module launches anything; it decides
//!   what *would* be launched, and says where the instruction came from.

/// A program and its arguments.
///
/// One element per argument. Never a shell string — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The executable to run. Looked up on `PATH` if it has no separator.
    pub program: String,
    /// Its arguments, already split.
    pub args: Vec<String>,
}

/// Where a server definition came from, and therefore how much it is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trust {
    /// Shipped with deco.
    BuiltIn,
    /// From the user's own settings. They wrote it; it is theirs.
    User,
    /// From `.vscode/settings.json` or similar inside the project.
    ///
    /// Cloning a repository must not be enough to run a program. The editor is
    /// expected to ask before launching one of these.
    Workspace,
}

impl Trust {
    /// Whether the editor should confirm with the user before launching.
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Self::Workspace)
    }
}

/// How to run a language server, and what it applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// An identifier for this server, e.g. `rust-analyzer`. Unique per registry.
    pub id: String,
    /// The VS Code language identifiers this server handles, e.g. `["rust"]`.
    pub language_ids: Vec<String>,
    /// What to run.
    pub command: Command,
    /// Extra environment variables for the child process.
    pub env: Vec<(String, String)>,
    /// The `initializationOptions` member of the `initialize` request.
    pub initialization_options: Option<serde_json::Value>,
    /// Where this definition came from.
    pub trust: Trust,
}

/// Why a server definition was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// No `command`, or an empty one.
    #[error("server `{id}` has no command to run")]
    NoCommand {
        /// The offending server id.
        id: String,
    },
    /// No `languages`, so nothing would ever start it.
    #[error("server `{id}` lists no languages, so nothing would start it")]
    NoLanguages {
        /// The offending server id.
        id: String,
    },
    /// A program name or argument contained a NUL or a newline.
    ///
    /// Rejected rather than escaped. No legitimate program name contains
    /// either, and both are exactly what an injection attempt looks like when
    /// the value is later written to a file, a log or another process's stdin.
    #[error("server `{id}` has a {field} containing a control character")]
    ControlCharacter {
        /// The offending server id.
        id: String,
        /// Which field.
        field: &'static str,
    },
    /// Two servers claimed the same id.
    #[error("server `{id}` is defined twice")]
    Duplicate {
        /// The offending server id.
        id: String,
    },
}

/// Rejects values that cannot legitimately appear in a program name or argument.
fn check(id: &str, field: &'static str, value: &str) -> Result<(), ConfigError> {
    if value.contains('\0') || value.contains('\n') || value.contains('\r') {
        return Err(ConfigError::ControlCharacter {
            id: id.to_owned(),
            field,
        });
    }
    Ok(())
}

impl ServerConfig {
    /// Reads one definition from settings.
    ///
    /// The shape mirrors what editors already use:
    ///
    /// ```jsonc
    /// {
    ///   "languages": ["rust"],
    ///   "command": "rust-analyzer",
    ///   "args": ["--log-file", "/tmp/ra.log"],
    ///   "env": { "RA_LOG": "info" },
    ///   "initializationOptions": { "cargo": { "features": "all" } }
    /// }
    /// ```
    pub fn from_json(
        id: impl Into<String>,
        value: &serde_json::Value,
        trust: Trust,
    ) -> Result<Self, ConfigError> {
        let id = id.into();

        let program = value
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        if program.trim().is_empty() {
            return Err(ConfigError::NoCommand { id });
        }
        check(&id, "command", &program)?;

        let args: Vec<String> = value
            .get("args")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        for arg in &args {
            check(&id, "argument", arg)?;
        }

        let language_ids: Vec<String> = value
            .get("languages")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .filter(|item| !item.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if language_ids.is_empty() {
            return Err(ConfigError::NoLanguages { id });
        }

        let mut env: Vec<(String, String)> = Vec::new();
        if let Some(object) = value.get("env").and_then(|v| v.as_object()) {
            for (name, value) in object {
                let Some(value) = value.as_str() else {
                    continue;
                };
                check(&id, "environment variable", name)?;
                check(&id, "environment variable", value)?;
                env.push((name.clone(), value.to_owned()));
            }
            // Object iteration order is already sorted for a serde_json map,
            // but pinning it here keeps the child's environment reproducible
            // regardless of the map implementation underneath.
            env.sort();
        }

        Ok(Self {
            id,
            language_ids,
            command: Command { program, args },
            env,
            initialization_options: value.get("initializationOptions").cloned(),
            trust,
        })
    }

    /// Whether this server handles a language.
    pub fn handles(&self, language_id: &str) -> bool {
        self.language_ids.iter().any(|id| id == language_id)
    }
}

/// The servers available, indexed by language.
#[derive(Debug, Clone, Default)]
pub struct ServerRegistry {
    servers: Vec<ServerConfig>,
}

impl ServerRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a definition, refusing a duplicate id.
    pub fn insert(&mut self, config: ServerConfig) -> Result<(), ConfigError> {
        if self.servers.iter().any(|s| s.id == config.id) {
            return Err(ConfigError::Duplicate { id: config.id });
        }
        self.servers.push(config);
        Ok(())
    }

    /// Reads a `{ "<id>": { … } }` map from settings.
    ///
    /// Returns the registry alongside the definitions it refused, so a single
    /// bad entry costs its own server rather than every server. A configuration
    /// file that silently disables language support is worse than one that
    /// reports what it could not read.
    pub fn from_json(value: &serde_json::Value, trust: Trust) -> (Self, Vec<ConfigError>) {
        let mut registry = Self::new();
        let mut problems = Vec::new();

        let Some(object) = value.as_object() else {
            return (registry, problems);
        };

        for (id, definition) in object {
            match ServerConfig::from_json(id.clone(), definition, trust) {
                Ok(config) => {
                    if let Err(error) = registry.insert(config) {
                        problems.push(error);
                    }
                }
                Err(error) => problems.push(error),
            }
        }

        (registry, problems)
    }

    /// Merges another registry over this one.
    ///
    /// Later definitions win, matching how every other setting in deco layers.
    /// The replacement carries the incoming definition's trust, so a workspace
    /// override of a user server still requires confirmation — otherwise
    /// shadowing a familiar id would be a way to launder an untrusted command.
    pub fn merge(&mut self, other: Self) {
        for config in other.servers {
            match self.servers.iter_mut().find(|s| s.id == config.id) {
                Some(existing) => *existing = config,
                None => self.servers.push(config),
            }
        }
    }

    /// The servers that handle a language, most preferred first.
    ///
    /// Anything the user configured comes before a built-in, because a built-in
    /// is a guess and a configuration is an instruction — someone who defines
    /// their own Rust server means it to be used instead of the bundled
    /// `rust-analyzer` entry, not alongside it.
    ///
    /// Within each group the order is the order of definition, which is settings
    /// order: user before workspace. That matters because a workspace-defined
    /// server needs confirmation before it runs, and if it came first a cloned
    /// repository could push the user's own working server out of the way
    /// simply by defining a competing one.
    pub fn for_language(&self, language_id: &str) -> Vec<&ServerConfig> {
        let matching = self.servers.iter().filter(|s| s.handles(language_id));
        let (configured, built_in): (Vec<_>, Vec<_>) =
            matching.partition(|s| s.trust != Trust::BuiltIn);
        configured.into_iter().chain(built_in).collect()
    }

    /// A server by id.
    pub fn get(&self, id: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|server| server.id == id)
    }

    /// Every server.
    pub fn iter(&self) -> impl Iterator<Item = &ServerConfig> {
        self.servers.iter()
    }

    /// How many servers are defined.
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    /// Whether no servers are defined.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Every language any server handles, sorted and deduplicated.
    pub fn languages(&self) -> Vec<&str> {
        let mut languages: Vec<&str> = self
            .servers
            .iter()
            .flat_map(|s| s.language_ids.iter().map(String::as_str))
            .collect();
        languages.sort_unstable();
        languages.dedup();
        languages
    }
}

/// Definitions deco ships with.
///
/// Deliberately short. Each entry assumes only that the program is on `PATH`,
/// which is the one thing deco can neither install nor verify — so a missing
/// server has to be an ordinary "not found" at launch rather than something
/// this table pretends to know.
pub fn built_in() -> ServerRegistry {
    let mut registry = ServerRegistry::new();
    for (id, languages, program, args) in [
        ("rust-analyzer", &["rust"][..], "rust-analyzer", &[][..]),
        (
            "typescript-language-server",
            &[
                "typescript",
                "typescriptreact",
                "javascript",
                "javascriptreact",
            ][..],
            "typescript-language-server",
            &["--stdio"][..],
        ),
        ("gopls", &["go"][..], "gopls", &[][..]),
        (
            "pyright",
            &["python"][..],
            "pyright-langserver",
            &["--stdio"][..],
        ),
    ] {
        registry
            .insert(ServerConfig {
                id: id.to_owned(),
                language_ids: languages.iter().map(|s| (*s).to_owned()).collect(),
                command: Command {
                    program: program.to_owned(),
                    args: args.iter().map(|s| (*s).to_owned()).collect(),
                },
                env: Vec::new(),
                initialization_options: None,
                trust: Trust::BuiltIn,
            })
            .expect("the built-in table has no duplicate ids");
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(value: serde_json::Value) -> Result<ServerConfig, ConfigError> {
        ServerConfig::from_json("test", &value, Trust::User)
    }

    #[test]
    fn a_definition_is_read_from_settings() {
        let server = config(json!({
            "languages": ["rust"],
            "command": "rust-analyzer",
            "args": ["--log-file", "/tmp/ra.log"],
            "env": {"RA_LOG": "info"},
            "initializationOptions": {"cargo": {"features": "all"}},
        }))
        .unwrap();

        assert_eq!(server.command.program, "rust-analyzer");
        assert_eq!(server.command.args, vec!["--log-file", "/tmp/ra.log"]);
        assert_eq!(server.env, vec![("RA_LOG".into(), "info".into())]);
        assert_eq!(
            server.initialization_options,
            Some(json!({"cargo": {"features": "all"}}))
        );
        assert!(server.handles("rust"));
        assert!(!server.handles("go"));
    }

    #[test]
    fn arguments_stay_separate_and_are_never_joined() {
        // The whole point of the argv representation: an argument containing a
        // space is one argument, not two, and no shell ever re-splits it.
        let server = config(json!({
            "languages": ["rust"],
            "command": "my server",
            "args": ["--path", "/a b/c", "; rm -rf ~"],
        }))
        .unwrap();
        assert_eq!(server.command.program, "my server");
        assert_eq!(server.command.args.len(), 3);
        assert_eq!(server.command.args[2], "; rm -rf ~");
    }

    #[test]
    fn a_missing_or_empty_command_is_refused() {
        for value in [
            json!({"languages": ["rust"]}),
            json!({"languages": ["rust"], "command": ""}),
            json!({"languages": ["rust"], "command": "   "}),
            json!({"languages": ["rust"], "command": 42}),
        ] {
            assert!(
                matches!(config(value.clone()), Err(ConfigError::NoCommand { .. })),
                "{value} should be refused"
            );
        }
    }

    #[test]
    fn a_definition_with_no_languages_is_refused() {
        // Nothing would ever start it, so accepting it just hides a typo.
        for value in [
            json!({"command": "x"}),
            json!({"command": "x", "languages": []}),
            json!({"command": "x", "languages": [""]}),
        ] {
            assert!(
                matches!(config(value.clone()), Err(ConfigError::NoLanguages { .. })),
                "{value} should be refused"
            );
        }
    }

    #[test]
    fn a_control_character_is_refused_rather_than_escaped() {
        // A newline in a program name is not a quoting problem to solve; it is
        // what an injection attempt looks like.
        for value in [
            json!({"languages": ["rust"], "command": "ra\nevil"}),
            json!({"languages": ["rust"], "command": "ra", "args": ["a\0b"]}),
            json!({"languages": ["rust"], "command": "ra", "env": {"A\nB": "x"}}),
            json!({"languages": ["rust"], "command": "ra", "env": {"A": "x\ny"}}),
        ] {
            assert!(
                matches!(
                    config(value.clone()),
                    Err(ConfigError::ControlCharacter { .. })
                ),
                "{value} should be refused"
            );
        }
    }

    #[test]
    fn a_workspace_definition_is_marked_for_confirmation() {
        // Cloning a repository must not by itself be enough to run a program.
        let server = ServerConfig::from_json(
            "x",
            &json!({"languages": ["rust"], "command": "ra"}),
            Trust::Workspace,
        )
        .unwrap();
        assert!(server.trust.needs_confirmation());
        assert!(!Trust::User.needs_confirmation());
        assert!(!Trust::BuiltIn.needs_confirmation());
    }

    #[test]
    fn a_registry_is_read_from_a_map_of_definitions() {
        let (registry, problems) = ServerRegistry::from_json(
            &json!({
                "rust-analyzer": {"languages": ["rust"], "command": "rust-analyzer"},
                "gopls": {"languages": ["go"], "command": "gopls"},
            }),
            Trust::User,
        );

        assert!(problems.is_empty());
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.languages(), vec!["go", "rust"]);
    }

    #[test]
    fn one_bad_definition_does_not_cost_the_others() {
        // A configuration file that silently disables every language because of
        // one typo is worse than one that reports what it could not read.
        let (registry, problems) = ServerRegistry::from_json(
            &json!({
                "good": {"languages": ["rust"], "command": "ra"},
                "bad": {"languages": ["go"]},
            }),
            Trust::User,
        );

        assert_eq!(registry.len(), 1);
        assert!(registry.get("good").is_some());
        assert_eq!(problems.len(), 1);
        assert!(matches!(problems[0], ConfigError::NoCommand { .. }));
    }

    #[test]
    fn a_non_object_is_read_as_no_servers_rather_than_failing() {
        let (registry, problems) = ServerRegistry::from_json(&json!([1, 2]), Trust::User);
        assert!(registry.is_empty());
        assert!(problems.is_empty());
    }

    #[test]
    fn a_duplicate_id_is_refused() {
        let mut registry = ServerRegistry::new();
        let server = config(json!({"languages": ["rust"], "command": "ra"})).unwrap();
        registry.insert(server.clone()).unwrap();
        assert_eq!(
            registry.insert(server),
            Err(ConfigError::Duplicate { id: "test".into() })
        );
    }

    #[test]
    fn several_servers_can_handle_one_language() {
        // A linter alongside a language server is an ordinary setup.
        let (registry, _) = ServerRegistry::from_json(
            &json!({
                "a": {"languages": ["rust"], "command": "ra"},
                "b": {"languages": ["rust"], "command": "linter"},
            }),
            Trust::User,
        );
        assert_eq!(registry.for_language("rust").len(), 2);
        assert!(registry.for_language("go").is_empty());
    }

    #[test]
    fn merging_lets_a_later_layer_replace_a_definition() {
        let (mut base, _) = ServerRegistry::from_json(
            &json!({"ra": {"languages": ["rust"], "command": "rust-analyzer"}}),
            Trust::User,
        );
        let (over, _) = ServerRegistry::from_json(
            &json!({"ra": {"languages": ["rust"], "command": "./my-ra"}}),
            Trust::Workspace,
        );
        base.merge(over);

        assert_eq!(base.len(), 1, "replaced, not appended");
        assert_eq!(base.get("ra").unwrap().command.program, "./my-ra");
    }

    #[test]
    fn a_workspace_override_of_a_user_server_still_needs_confirmation() {
        // Otherwise shadowing a familiar id would launder an untrusted command
        // into a trusted slot.
        let (mut base, _) = ServerRegistry::from_json(
            &json!({"ra": {"languages": ["rust"], "command": "rust-analyzer"}}),
            Trust::User,
        );
        let (over, _) = ServerRegistry::from_json(
            &json!({"ra": {"languages": ["rust"], "command": "./evil"}}),
            Trust::Workspace,
        );
        base.merge(over);

        assert!(base.get("ra").unwrap().trust.needs_confirmation());
    }

    #[test]
    fn merging_keeps_definitions_the_later_layer_did_not_mention() {
        let (mut base, _) = ServerRegistry::from_json(
            &json!({
                "ra": {"languages": ["rust"], "command": "rust-analyzer"},
                "gopls": {"languages": ["go"], "command": "gopls"},
            }),
            Trust::User,
        );
        let (over, _) = ServerRegistry::from_json(
            &json!({"ra": {"languages": ["rust"], "command": "./my-ra"}}),
            Trust::Workspace,
        );
        base.merge(over);
        assert_eq!(base.len(), 2);
        assert!(base.get("gopls").is_some());
    }

    #[test]
    fn the_built_in_table_is_valid_and_trusted() {
        let registry = built_in();
        assert!(!registry.is_empty());
        for server in registry.iter() {
            assert_eq!(server.trust, Trust::BuiltIn);
            assert!(!server.command.program.is_empty(), "{}", server.id);
            assert!(!server.language_ids.is_empty(), "{}", server.id);
        }
        assert_eq!(
            registry.for_language("rust").len(),
            1,
            "rust maps to exactly one built-in server"
        );
        assert_eq!(registry.for_language("typescript").len(), 1);
    }

    #[test]
    fn env_ordering_is_stable() {
        // A child process's environment should not depend on map iteration
        // order, or a bug will reproduce only sometimes.
        let server = config(json!({
            "languages": ["rust"],
            "command": "ra",
            "env": {"Z": "1", "A": "2", "M": "3"},
        }))
        .unwrap();
        let names: Vec<&str> = server.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["A", "M", "Z"]);
    }
}
