//! Turning keypresses into commands.

use serde_json::Value;

use crate::binding::{Keybinding, Rule, Source};
use crate::keys::{Chord, KeySequence};
use crate::when::{ContextKeys, WhenExpr};

/// What a keypress resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// Nothing matched; the frontend should handle the key itself (typing a
    /// character, for instance).
    NoMatch,
    /// The key began a chord. The frontend should show the pending prefix and
    /// send the next keypress back here.
    Pending {
        /// The chord already pressed.
        prefix: Chord,
    },
    /// A command should run.
    Match {
        /// The command identifier.
        command: String,
        /// Arguments to pass, if the binding supplied any.
        args: Option<Value>,
    },
}

/// Tracks whether a chord prefix is currently pending.
///
/// Kept separate from [`Keymap`] so that each view can own its own chord state
/// while sharing one keymap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChordState {
    pending: Option<Chord>,
}

impl ChordState {
    /// No chord in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// The pending prefix, if any.
    pub fn pending(&self) -> Option<Chord> {
        self.pending
    }

    /// Abandons any pending chord (Escape, or focus moving away).
    pub fn reset(&mut self) {
        self.pending = None;
    }
}

/// The set of active keybindings, in precedence order.
///
/// Later bindings win, which is how a user's `keybindings.json` overrides the
/// defaults without either side needing to know about the other.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: Vec<Keybinding>,
}

impl Keymap {
    /// An empty keymap.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a keymap by applying `rules` in order.
    pub fn from_rules(rules: impl IntoIterator<Item = Rule>) -> Self {
        let mut keymap = Self::new();
        keymap.apply(rules);
        keymap
    }

    /// Applies more rules on top of the existing ones.
    pub fn apply(&mut self, rules: impl IntoIterator<Item = Rule>) {
        for rule in rules {
            match rule {
                Rule::Bind(binding) => self.bindings.push(binding),
                Rule::Unbind(removal) => self.remove_matching(&removal),
            }
        }
    }

    /// Removes bindings targeted by a `-command` entry.
    ///
    /// A removal without a `when` clause removes every binding for that key and
    /// command; one *with* a clause only removes bindings carrying the same
    /// clause, so a user can drop a context-specific default while keeping the
    /// general one. This is VS Code's rule.
    fn remove_matching(&mut self, removal: &Keybinding) {
        self.bindings.retain(|existing| {
            let same_target = existing.command == removal.command && existing.key == removal.key;
            if !same_target {
                return true;
            }
            match &removal.when {
                None => false,
                Some(when) => existing.when.as_ref() != Some(when),
            }
        });
    }

    /// All bindings, lowest precedence first.
    pub fn bindings(&self) -> &[Keybinding] {
        &self.bindings
    }

    /// Number of bindings.
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether the keymap holds no bindings.
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// The key sequences bound to `command`, highest precedence first. Used to
    /// label menu items and the command palette.
    pub fn keys_for_command(&self, command: &str) -> Vec<&KeySequence> {
        self.bindings
            .iter()
            .rev()
            .filter(|b| b.command == command)
            .map(|b| &b.key)
            .collect()
    }

    /// Resolves `chord` against the current chord state and context.
    ///
    /// Advances `state`: a chord prefix is recorded, and any other outcome
    /// clears it.
    pub fn resolve(&self, state: &mut ChordState, chord: Chord, ctx: &ContextKeys) -> Resolution {
        match state.pending.take() {
            Some(prefix) => self.resolve_second(prefix, chord, ctx),
            None => {
                let resolution = self.resolve_first(chord, ctx);
                if let Resolution::Pending { prefix } = resolution {
                    state.pending = Some(prefix);
                }
                resolution
            }
        }
    }

    fn resolve_first(&self, chord: Chord, ctx: &ContextKeys) -> Resolution {
        // Scanning backwards means the highest-precedence binding is found
        // first, which is exactly "later definitions win".
        let winner = self
            .bindings
            .iter()
            .rev()
            .find(|b| b.key.first() == chord && when_matches(b.when.as_ref(), ctx));

        match winner {
            None => Resolution::NoMatch,
            Some(binding) if binding.key.is_chord() => Resolution::Pending { prefix: chord },
            Some(binding) => command_result(binding),
        }
    }

    fn resolve_second(&self, prefix: Chord, chord: Chord, ctx: &ContextKeys) -> Resolution {
        let winner = self
            .bindings
            .iter()
            .rev()
            .find(|b| b.key.chords() == [prefix, chord] && when_matches(b.when.as_ref(), ctx));
        match winner {
            None => Resolution::NoMatch,
            Some(binding) => command_result(binding),
        }
    }
}

/// An empty command string means "unbind this key", not "run a command named
/// the empty string".
fn command_result(binding: &Keybinding) -> Resolution {
    if binding.command.is_empty() {
        Resolution::NoMatch
    } else {
        Resolution::Match {
            command: binding.command.clone(),
            args: binding.args.clone(),
        }
    }
}

fn when_matches(when: Option<&WhenExpr>, ctx: &ContextKeys) -> bool {
    when.map(|expr| expr.evaluate(ctx)).unwrap_or(true)
}

/// Builds a keymap from the built-in defaults plus optional user bindings.
///
/// Returns the keymap along with any problems found in the user's file, so the
/// caller can surface them without blocking startup.
pub fn build(
    platform: crate::binding::Platform,
    user_keybindings: Option<&str>,
) -> (Keymap, Vec<crate::binding::Problem>) {
    let defaults = crate::defaults::default_rules(platform);
    let mut keymap = Keymap::from_rules(defaults);
    let mut problems = Vec::new();

    if let Some(source) = user_keybindings {
        match crate::binding::parse(source, platform, Source::User) {
            Ok(parsed) => {
                problems = parsed.problems;
                keymap.apply(parsed.rules);
            }
            Err(e) => problems.push(crate::binding::Problem {
                index: 0,
                message: e.to_string(),
            }),
        }
    }
    (keymap, problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::{parse, Platform};
    use serde_json::json;

    fn keymap(source: &str) -> Keymap {
        let parsed = parse(source, Platform::Linux, Source::User).unwrap();
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        Keymap::from_rules(parsed.rules)
    }

    fn press(keymap: &Keymap, state: &mut ChordState, key: &str, ctx: &ContextKeys) -> Resolution {
        keymap.resolve(state, Chord::parse(key).unwrap(), ctx)
    }

    fn matched(command: &str) -> Resolution {
        Resolution::Match {
            command: command.to_owned(),
            args: None,
        }
    }

    #[test]
    fn resolves_a_simple_binding() {
        let km = keymap(r#"[{"key": "ctrl+s", "command": "save"}]"#);
        let mut state = ChordState::new();
        assert_eq!(
            press(&km, &mut state, "ctrl+s", &ContextKeys::new()),
            matched("save")
        );
    }

    #[test]
    fn an_unbound_key_does_not_match() {
        let km = keymap(r#"[{"key": "ctrl+s", "command": "save"}]"#);
        let mut state = ChordState::new();
        assert_eq!(
            press(&km, &mut state, "ctrl+x", &ContextKeys::new()),
            Resolution::NoMatch
        );
    }

    #[test]
    fn later_bindings_win() {
        let km = keymap(
            r#"[
                {"key": "ctrl+s", "command": "first"},
                {"key": "ctrl+s", "command": "second"}
            ]"#,
        );
        let mut state = ChordState::new();
        assert_eq!(
            press(&km, &mut state, "ctrl+s", &ContextKeys::new()),
            matched("second")
        );
    }

    #[test]
    fn when_clauses_gate_bindings() {
        let km = keymap(
            r#"[
                {"key": "tab", "command": "indent"},
                {"key": "tab", "command": "acceptSuggestion", "when": "suggestWidgetVisible"}
            ]"#,
        );
        let mut state = ChordState::new();
        let mut ctx = ContextKeys::new();
        assert_eq!(press(&km, &mut state, "tab", &ctx), matched("indent"));
        ctx.set("suggestWidgetVisible", true);
        assert_eq!(
            press(&km, &mut state, "tab", &ctx),
            matched("acceptSuggestion")
        );
    }

    #[test]
    fn a_binding_whose_when_fails_falls_through_to_the_one_below() {
        let km = keymap(
            r#"[
                {"key": "escape", "command": "clearSelection"},
                {"key": "escape", "command": "closeWidget", "when": "widgetVisible"}
            ]"#,
        );
        let mut state = ChordState::new();
        assert_eq!(
            press(&km, &mut state, "escape", &ContextKeys::new()),
            matched("clearSelection")
        );
    }

    #[test]
    fn args_are_carried_through() {
        let km = keymap(r#"[{"key": "ctrl+k", "command": "type", "args": {"text": "x"}}]"#);
        let mut state = ChordState::new();
        assert_eq!(
            press(&km, &mut state, "ctrl+k", &ContextKeys::new()),
            Resolution::Match {
                command: "type".into(),
                args: Some(json!({"text": "x"}))
            }
        );
    }

    #[test]
    fn chords_need_a_second_keypress() {
        let km = keymap(r#"[{"key": "ctrl+k ctrl+c", "command": "addComment"}]"#);
        let mut state = ChordState::new();
        let ctx = ContextKeys::new();

        assert_eq!(
            press(&km, &mut state, "ctrl+k", &ctx),
            Resolution::Pending {
                prefix: Chord::parse("ctrl+k").unwrap()
            }
        );
        assert_eq!(state.pending(), Some(Chord::parse("ctrl+k").unwrap()));

        assert_eq!(
            press(&km, &mut state, "ctrl+c", &ctx),
            matched("addComment")
        );
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn an_unmatched_second_chord_clears_the_pending_state() {
        let km = keymap(r#"[{"key": "ctrl+k ctrl+c", "command": "addComment"}]"#);
        let mut state = ChordState::new();
        let ctx = ContextKeys::new();
        press(&km, &mut state, "ctrl+k", &ctx);
        assert_eq!(press(&km, &mut state, "ctrl+z", &ctx), Resolution::NoMatch);
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn a_second_chord_does_not_fall_back_to_single_key_bindings() {
        // While a chord is pending, `ctrl+c` must not run "copy".
        let km = keymap(
            r#"[
                {"key": "ctrl+c", "command": "copy"},
                {"key": "ctrl+k ctrl+u", "command": "removeComment"}
            ]"#,
        );
        let mut state = ChordState::new();
        let ctx = ContextKeys::new();
        press(&km, &mut state, "ctrl+k", &ctx);
        assert_eq!(press(&km, &mut state, "ctrl+c", &ctx), Resolution::NoMatch);
    }

    #[test]
    fn two_chords_sharing_a_prefix_both_resolve() {
        let km = keymap(
            r#"[
                {"key": "ctrl+k ctrl+c", "command": "addComment"},
                {"key": "ctrl+k ctrl+u", "command": "removeComment"}
            ]"#,
        );
        let ctx = ContextKeys::new();
        let mut state = ChordState::new();
        press(&km, &mut state, "ctrl+k", &ctx);
        assert_eq!(
            press(&km, &mut state, "ctrl+c", &ctx),
            matched("addComment")
        );
        press(&km, &mut state, "ctrl+k", &ctx);
        assert_eq!(
            press(&km, &mut state, "ctrl+u", &ctx),
            matched("removeComment")
        );
    }

    #[test]
    fn resetting_abandons_a_pending_chord() {
        let km = keymap(r#"[{"key": "ctrl+k ctrl+c", "command": "addComment"}]"#);
        let mut state = ChordState::new();
        let ctx = ContextKeys::new();
        press(&km, &mut state, "ctrl+k", &ctx);
        state.reset();
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn a_single_key_binding_defined_after_a_chord_takes_the_key() {
        let km = keymap(
            r#"[
                {"key": "ctrl+k ctrl+c", "command": "addComment"},
                {"key": "ctrl+k", "command": "killLine"}
            ]"#,
        );
        let mut state = ChordState::new();
        assert_eq!(
            press(&km, &mut state, "ctrl+k", &ContextKeys::new()),
            matched("killLine")
        );
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn removal_without_a_when_drops_every_matching_binding() {
        let km = keymap(
            r#"[
                {"key": "ctrl+s", "command": "save"},
                {"key": "ctrl+s", "command": "save", "when": "editorFocus"},
                {"key": "ctrl+s", "command": "-save"}
            ]"#,
        );
        assert!(km.is_empty());
    }

    #[test]
    fn removal_with_a_when_drops_only_the_matching_clause() {
        let km = keymap(
            r#"[
                {"key": "ctrl+s", "command": "save"},
                {"key": "ctrl+s", "command": "save", "when": "editorFocus"},
                {"key": "ctrl+s", "command": "-save", "when": "editorFocus"}
            ]"#,
        );
        assert_eq!(km.len(), 1);
        assert!(km.bindings()[0].when.is_none());
    }

    #[test]
    fn removal_only_affects_the_named_key() {
        let km = keymap(
            r#"[
                {"key": "ctrl+s", "command": "save"},
                {"key": "ctrl+alt+s", "command": "save"},
                {"key": "ctrl+s", "command": "-save"}
            ]"#,
        );
        assert_eq!(km.len(), 1);
        assert_eq!(km.bindings()[0].key.to_string(), "ctrl+alt+s");
    }

    #[test]
    fn removal_before_the_binding_it_names_has_no_effect() {
        // Rules apply in order, so a removal cannot reach forwards.
        let km = keymap(
            r#"[
                {"key": "ctrl+s", "command": "-save"},
                {"key": "ctrl+s", "command": "save"}
            ]"#,
        );
        assert_eq!(km.len(), 1);
    }

    #[test]
    fn an_empty_command_unbinds_the_key() {
        let km = keymap(
            r#"[
                {"key": "ctrl+s", "command": "save"},
                {"key": "ctrl+s", "command": ""}
            ]"#,
        );
        let mut state = ChordState::new();
        assert_eq!(
            press(&km, &mut state, "ctrl+s", &ContextKeys::new()),
            Resolution::NoMatch
        );
    }

    #[test]
    fn keys_for_command_lists_highest_precedence_first() {
        let km = keymap(
            r#"[
                {"key": "ctrl+s", "command": "save"},
                {"key": "ctrl+alt+s", "command": "save"}
            ]"#,
        );
        let keys: Vec<String> = km
            .keys_for_command("save")
            .iter()
            .map(|k| k.to_string())
            .collect();
        assert_eq!(keys, ["ctrl+alt+s", "ctrl+s"]);
        assert!(km.keys_for_command("nope").is_empty());
    }

    #[test]
    fn user_bindings_override_the_defaults() {
        let user = r#"[{"key": "ctrl+p", "command": "my.customCommand"}]"#;
        let (km, problems) = build(Platform::Linux, Some(user));
        assert!(problems.is_empty(), "{problems:?}");
        let mut state = ChordState::new();
        let mut ctx = ContextKeys::with_platform_defaults();
        ctx.set("editorTextFocus", true);
        assert_eq!(
            press(&km, &mut state, "ctrl+p", &ctx),
            matched("my.customCommand")
        );
    }

    #[test]
    fn a_broken_user_file_still_leaves_the_defaults_working() {
        let (km, problems) = build(Platform::Linux, Some("this is not json"));
        assert_eq!(problems.len(), 1);
        assert!(!km.is_empty());
    }
}
