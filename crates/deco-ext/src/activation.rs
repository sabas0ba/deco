//! Activation events: when an extension's code is allowed to start running.
//!
//! Activation is a security control as much as a performance one. An extension
//! that has not activated has no host process and therefore no capability
//! requests at all, so narrow activation events are the cheapest possible
//! mitigation for a compromised extension.

use deco_config::glob;

/// One entry of `activationEvents`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationEvent {
    /// `*` — activate at startup, unconditionally.
    Always,
    /// `onStartupFinished` — activate once the editor is idle.
    StartupFinished,
    /// `onLanguage:<id>`.
    Language(String),
    /// `onCommand:<id>`.
    Command(String),
    /// `workspaceContains:<glob>`.
    WorkspaceContains(String),
    /// `onFileSystem:<scheme>`.
    FileSystem(String),
    /// `onView:<id>`.
    View(String),
    /// `onUri`.
    Uri,
    /// Anything deco does not model. Never fires, which keeps an unknown event
    /// from being treated as `*` by accident.
    Unknown(String),
}

impl ActivationEvent {
    /// Parses one activation event string.
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        match text {
            "*" => return ActivationEvent::Always,
            "onStartupFinished" => return ActivationEvent::StartupFinished,
            "onUri" => return ActivationEvent::Uri,
            _ => {}
        }
        match text.split_once(':') {
            Some(("onLanguage", value)) => ActivationEvent::Language(value.to_owned()),
            Some(("onCommand", value)) => ActivationEvent::Command(value.to_owned()),
            Some(("workspaceContains", value)) => {
                ActivationEvent::WorkspaceContains(value.to_owned())
            }
            Some(("onFileSystem", value)) => ActivationEvent::FileSystem(value.to_owned()),
            Some(("onView", value)) => ActivationEvent::View(value.to_owned()),
            _ => ActivationEvent::Unknown(text.to_owned()),
        }
    }
}

/// Something that happened and might activate an extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger<'a> {
    /// The editor finished starting up.
    StartupFinished,
    /// A document of this language was opened.
    Language(&'a str),
    /// This command was invoked.
    Command(&'a str),
    /// The workspace contains these files, as `/`-separated relative paths.
    WorkspaceFiles(&'a [String]),
    /// A view was revealed.
    View(&'a str),
    /// A URI was handed to the editor.
    Uri,
}

/// Whether `event` fires for `trigger`.
pub fn fires(event: &ActivationEvent, trigger: &Trigger<'_>) -> bool {
    match (event, trigger) {
        // `*` activates on startup and nothing else; there is no separate
        // "activate on everything forever" state to be in.
        (ActivationEvent::Always, Trigger::StartupFinished) => true,
        (ActivationEvent::StartupFinished, Trigger::StartupFinished) => true,
        (ActivationEvent::Language(want), Trigger::Language(got)) => want == got,
        (ActivationEvent::Command(want), Trigger::Command(got)) => want == got,
        (ActivationEvent::View(want), Trigger::View(got)) => want == got,
        (ActivationEvent::Uri, Trigger::Uri) => true,
        (ActivationEvent::WorkspaceContains(pattern), Trigger::WorkspaceFiles(files)) => {
            files.iter().any(|file| glob::matches(pattern, file))
        }
        _ => false,
    }
}

/// Whether any of `events` fires for `trigger`.
pub fn any_fires(events: &[ActivationEvent], trigger: &Trigger<'_>) -> bool {
    events.iter().any(|event| fires(event, trigger))
}

/// Parses a manifest's `activationEvents`.
pub fn parse_all(events: &[String]) -> Vec<ActivationEvent> {
    events.iter().map(|e| ActivationEvent::parse(e)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_event_forms() {
        assert_eq!(ActivationEvent::parse("*"), ActivationEvent::Always);
        assert_eq!(
            ActivationEvent::parse("onStartupFinished"),
            ActivationEvent::StartupFinished
        );
        assert_eq!(
            ActivationEvent::parse("onLanguage:rust"),
            ActivationEvent::Language("rust".into())
        );
        assert_eq!(
            ActivationEvent::parse("onCommand:my.cmd"),
            ActivationEvent::Command("my.cmd".into())
        );
        assert_eq!(
            ActivationEvent::parse("workspaceContains:**/Cargo.toml"),
            ActivationEvent::WorkspaceContains("**/Cargo.toml".into())
        );
        assert_eq!(
            ActivationEvent::parse("onView:myView"),
            ActivationEvent::View("myView".into())
        );
        assert_eq!(ActivationEvent::parse("onUri"), ActivationEvent::Uri);
    }

    #[test]
    fn an_unrecognised_event_never_fires() {
        let event = ActivationEvent::parse("onSomethingNew:x");
        assert_eq!(event, ActivationEvent::Unknown("onSomethingNew:x".into()));
        for trigger in [
            Trigger::StartupFinished,
            Trigger::Language("rust"),
            Trigger::Command("x"),
            Trigger::Uri,
        ] {
            assert!(
                !fires(&event, &trigger),
                "unknown event fired for {trigger:?}"
            );
        }
    }

    #[test]
    fn language_events_are_exact() {
        let event = ActivationEvent::parse("onLanguage:rust");
        assert!(fires(&event, &Trigger::Language("rust")));
        assert!(!fires(&event, &Trigger::Language("rustdoc")));
        assert!(!fires(&event, &Trigger::Command("rust")));
    }

    #[test]
    fn star_activates_only_at_startup() {
        let event = ActivationEvent::parse("*");
        assert!(fires(&event, &Trigger::StartupFinished));
        assert!(!fires(&event, &Trigger::Language("rust")));
    }

    #[test]
    fn workspace_contains_uses_globs() {
        let event = ActivationEvent::parse("workspaceContains:**/Cargo.toml");
        let files = vec!["crates/deco/Cargo.toml".to_owned(), "README.md".to_owned()];
        assert!(fires(&event, &Trigger::WorkspaceFiles(&files)));

        let files = vec!["README.md".to_owned()];
        assert!(!fires(&event, &Trigger::WorkspaceFiles(&files)));
    }

    #[test]
    fn any_fires_scans_the_whole_list() {
        let events = parse_all(&["onLanguage:go".to_owned(), "onCommand:my.cmd".to_owned()]);
        assert!(any_fires(&events, &Trigger::Command("my.cmd")));
        assert!(!any_fires(&events, &Trigger::Command("other.cmd")));
        assert!(!any_fires(&events, &Trigger::StartupFinished));
    }

    #[test]
    fn an_extension_with_no_activation_events_never_activates() {
        assert!(!any_fires(&[], &Trigger::StartupFinished));
    }
}
