//! deco's built-in keymap.
//!
//! Command identifiers are VS Code's, verbatim. That is the whole point: a
//! user's `keybindings.json` refers to commands like
//! `editor.action.commentLine`, and rebinding one has to reach the same command
//! deco runs by default. Where VS Code's own default differs per platform the
//! entry carries a `mac` field, exactly as `keybindings.json` allows.

use crate::binding::{parse, Platform, Rule, Source};

/// The default keymap, in `keybindings.json` form.
pub const DEFAULT_KEYBINDINGS_JSONC: &str = r#"[
    // ---- Cursor movement ------------------------------------------------
    { "key": "left",             "command": "cursorLeft",           "when": "textInputFocus" },
    { "key": "right",            "command": "cursorRight",          "when": "textInputFocus" },
    { "key": "up",               "command": "cursorUp",             "when": "textInputFocus" },
    { "key": "down",             "command": "cursorDown",           "when": "textInputFocus" },
    { "key": "shift+left",       "command": "cursorLeftSelect",     "when": "textInputFocus" },
    { "key": "shift+right",      "command": "cursorRightSelect",    "when": "textInputFocus" },
    { "key": "shift+up",         "command": "cursorUpSelect",       "when": "textInputFocus" },
    { "key": "shift+down",       "command": "cursorDownSelect",     "when": "textInputFocus" },

    { "key": "ctrl+left",  "mac": "alt+left",        "command": "cursorWordLeft",        "when": "textInputFocus" },
    { "key": "ctrl+right", "mac": "alt+right",       "command": "cursorWordEndRight",    "when": "textInputFocus" },
    { "key": "ctrl+shift+left",  "mac": "alt+shift+left",  "command": "cursorWordLeftSelect",     "when": "textInputFocus" },
    { "key": "ctrl+shift+right", "mac": "alt+shift+right", "command": "cursorWordEndRightSelect", "when": "textInputFocus" },

    { "key": "home",       "mac": "cmd+left",        "command": "cursorHome",       "when": "textInputFocus" },
    { "key": "end",        "mac": "cmd+right",       "command": "cursorEnd",        "when": "textInputFocus" },
    { "key": "shift+home", "mac": "cmd+shift+left",  "command": "cursorHomeSelect", "when": "textInputFocus" },
    { "key": "shift+end",  "mac": "cmd+shift+right", "command": "cursorEndSelect",  "when": "textInputFocus" },

    { "key": "ctrl+home",       "mac": "cmd+up",         "command": "cursorTop",          "when": "textInputFocus" },
    { "key": "ctrl+end",        "mac": "cmd+down",       "command": "cursorBottom",       "when": "textInputFocus" },
    { "key": "ctrl+shift+home", "mac": "cmd+shift+up",   "command": "cursorTopSelect",    "when": "textInputFocus" },
    { "key": "ctrl+shift+end",  "mac": "cmd+shift+down", "command": "cursorBottomSelect", "when": "textInputFocus" },

    { "key": "pageup",         "command": "cursorPageUp",         "when": "textInputFocus" },
    { "key": "pagedown",       "command": "cursorPageDown",       "when": "textInputFocus" },
    { "key": "shift+pageup",   "command": "cursorPageUpSelect",   "when": "textInputFocus" },
    { "key": "shift+pagedown", "command": "cursorPageDownSelect", "when": "textInputFocus" },

    // ---- Selection ------------------------------------------------------
    { "key": "ctrl+a", "mac": "cmd+a", "command": "editor.action.selectAll", "when": "textInputFocus" },
    { "key": "ctrl+l", "mac": "cmd+l", "command": "expandLineSelection",     "when": "textInputFocus" },
    { "key": "escape", "command": "removeSecondaryCursors", "when": "editorHasMultipleSelections" },
    { "key": "escape", "command": "cancelSelection",        "when": "editorHasSelection && !editorHasMultipleSelections" },

    // ---- Multi-cursor ---------------------------------------------------
    { "key": "ctrl+alt+down", "mac": "cmd+alt+down", "command": "editor.action.insertCursorBelow", "when": "editorTextFocus" },
    { "key": "ctrl+alt+up",   "mac": "cmd+alt+up",   "command": "editor.action.insertCursorAbove", "when": "editorTextFocus" },
    { "key": "ctrl+d",        "mac": "cmd+d",        "command": "editor.action.addSelectionToNextFindMatch", "when": "editorFocus" },
    { "key": "ctrl+shift+l",  "mac": "cmd+shift+l",  "command": "editor.action.selectHighlights",  "when": "editorFocus" },
    { "key": "ctrl+k ctrl+d", "mac": "cmd+k cmd+d",  "command": "editor.action.moveSelectionToNextFindMatch", "when": "editorFocus" },

    // ---- Text editing ---------------------------------------------------
    { "key": "ctrl+z",       "mac": "cmd+z",       "command": "undo", "when": "textInputFocus && !editorReadonly" },
    { "key": "ctrl+shift+z", "mac": "cmd+shift+z", "command": "redo", "when": "textInputFocus && !editorReadonly" },
    { "key": "ctrl+y",                             "command": "redo", "when": "textInputFocus && !editorReadonly && isWindows" },

    { "key": "ctrl+x", "mac": "cmd+x", "command": "editor.action.clipboardCutAction",   "when": "textInputFocus && !editorReadonly" },
    { "key": "ctrl+c", "mac": "cmd+c", "command": "editor.action.clipboardCopyAction",  "when": "textInputFocus" },
    { "key": "ctrl+v", "mac": "cmd+v", "command": "editor.action.clipboardPasteAction", "when": "textInputFocus && !editorReadonly" },

    { "key": "backspace",      "command": "deleteLeft",  "when": "textInputFocus && !editorReadonly" },
    { "key": "delete",         "command": "deleteRight", "when": "textInputFocus && !editorReadonly" },
    { "key": "ctrl+backspace", "mac": "alt+backspace", "command": "deleteWordLeft",  "when": "textInputFocus && !editorReadonly" },
    { "key": "ctrl+delete",    "mac": "alt+delete",    "command": "deleteWordRight", "when": "textInputFocus && !editorReadonly" },

    { "key": "enter", "command": "type", "args": { "text": "\n" }, "when": "textInputFocus && !editorReadonly && !suggestWidgetVisible" },
    { "key": "tab",   "command": "tab",      "when": "editorTextFocus && !editorReadonly && !suggestWidgetVisible" },
    { "key": "shift+tab", "command": "outdent", "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+]", "mac": "cmd+]", "command": "editor.action.indentLines",  "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+[", "mac": "cmd+[", "command": "editor.action.outdentLines", "when": "editorTextFocus && !editorReadonly" },

    { "key": "alt+up",         "command": "editor.action.moveLinesUpAction",   "when": "editorTextFocus && !editorReadonly" },
    { "key": "alt+down",       "command": "editor.action.moveLinesDownAction", "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+shift+alt+up",   "mac": "cmd+shift+alt+up",   "command": "editor.action.copyLinesUpAction",   "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+shift+alt+down", "mac": "cmd+shift+alt+down", "command": "editor.action.copyLinesDownAction", "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+shift+k",   "mac": "cmd+shift+k", "command": "editor.action.deleteLines", "when": "textInputFocus && !editorReadonly" },
    { "key": "ctrl+enter",     "mac": "cmd+enter",   "command": "editor.action.insertLineAfter",  "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+shift+enter", "mac": "cmd+shift+enter", "command": "editor.action.insertLineBefore", "when": "editorTextFocus && !editorReadonly" },

    { "key": "ctrl+/",       "mac": "cmd+/",       "command": "editor.action.commentLine",      "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+shift+a", "mac": "cmd+shift+a", "command": "editor.action.blockComment",     "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+k ctrl+c", "mac": "cmd+k cmd+c", "command": "editor.action.addCommentLine",    "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+k ctrl+u", "mac": "cmd+k cmd+u", "command": "editor.action.removeCommentLine", "when": "editorTextFocus && !editorReadonly" },
    { "key": "ctrl+shift+i", "mac": "cmd+shift+i", "command": "editor.action.formatDocument",  "when": "editorHasDocumentFormattingProvider && editorTextFocus && !editorReadonly" },
    { "key": "ctrl+k ctrl+f", "mac": "cmd+k cmd+f", "command": "editor.action.formatSelection", "when": "editorHasDocumentFormattingProvider && editorHasSelection && editorTextFocus && !editorReadonly" },

    // ---- Files ----------------------------------------------------------
    { "key": "ctrl+n",       "mac": "cmd+n",       "command": "workbench.action.files.newUntitledFile" },
    { "key": "ctrl+o",       "mac": "cmd+o",       "command": "workbench.action.files.openFile" },
    { "key": "ctrl+s",       "mac": "cmd+s",       "command": "workbench.action.files.save" },
    { "key": "ctrl+shift+s", "mac": "cmd+shift+s", "command": "workbench.action.files.saveAs" },
    { "key": "ctrl+k s",     "mac": "cmd+k s",     "command": "workbench.action.files.saveAll" },
    { "key": "ctrl+w",       "mac": "cmd+w",       "command": "workbench.action.closeActiveEditor" },
    { "key": "ctrl+k ctrl+o", "mac": "cmd+k cmd+o", "command": "workbench.action.files.openFolder" },
    { "key": "ctrl+q",       "command": "workbench.action.quit", "when": "!isMac" },
    { "key": "cmd+q",        "command": "workbench.action.quit", "when": "isMac" },

    // ---- Search ---------------------------------------------------------
    { "key": "ctrl+f",       "mac": "cmd+f",       "command": "actions.find",                  "when": "editorFocus" },
    { "key": "ctrl+h",       "mac": "cmd+alt+f",   "command": "editor.action.startFindReplaceAction", "when": "editorFocus" },
    { "key": "f3",           "mac": "cmd+g",       "command": "editor.action.nextMatchFindAction",     "when": "editorFocus" },
    { "key": "shift+f3",     "mac": "cmd+shift+g", "command": "editor.action.previousMatchFindAction", "when": "editorFocus" },
    { "key": "ctrl+shift+f", "mac": "cmd+shift+f", "command": "workbench.action.findInFiles" },
    { "key": "ctrl+shift+h", "mac": "cmd+shift+h", "command": "workbench.action.replaceInFiles" },
    { "key": "escape",       "command": "closeFindWidget", "when": "editorFocus && findWidgetVisible" },
    // After the `enter` binding above, so that these win while the find input has
    // the keyboard: a later rule takes precedence, as it does in VS Code.
    { "key": "enter",        "command": "editor.action.nextMatchFindAction",     "when": "findInputFocussed" },
    { "key": "shift+enter",  "command": "editor.action.previousMatchFindAction", "when": "findInputFocussed" },
    { "key": "alt+c",        "command": "toggleFindCaseSensitive", "when": "findWidgetVisible" },
    { "key": "alt+w",        "command": "toggleFindWholeWord",     "when": "findWidgetVisible" },
    // The same keys while a project search is being typed, where they toggle that
    // search's own options rather than the find bar's.
    { "key": "alt+c",        "command": "toggleFindCaseSensitive", "when": "searchViewletVisible" },
    { "key": "alt+w",        "command": "toggleFindWholeWord",     "when": "searchViewletVisible" },
    { "key": "alt+r",        "command": "toggleFindRegex",         "when": "findWidgetVisible" },
    { "key": "enter",        "command": "editor.action.replaceOne", "when": "replaceInputFocussed" },
    { "key": "ctrl+alt+enter", "mac": "cmd+alt+enter", "command": "editor.action.replaceAll", "when": "findWidgetVisible" },
    // VS Code moves between the two inputs with the browser's own focus
    // traversal, so there is no command identifier of its to be faithful to. A
    // `deco.` one says as much.
    { "key": "tab",          "command": "deco.find.toggleField", "when": "findWidgetVisible" },
    { "key": "shift+tab",    "command": "deco.find.toggleField", "when": "findWidgetVisible" },

    // ---- Navigation -----------------------------------------------------
    { "key": "ctrl+p",       "mac": "cmd+p",       "command": "workbench.action.quickOpen" },
    { "key": "ctrl+shift+p", "mac": "cmd+shift+p", "command": "workbench.action.showCommands" },
    { "key": "f1",           "command": "workbench.action.showCommands" },
    { "key": "ctrl+g",       "command": "workbench.action.gotoLine" },
    { "key": "ctrl+k ctrl+i", "mac": "cmd+k cmd+i", "command": "editor.action.showHover", "when": "editorTextFocus" },
    { "key": "f12",          "command": "editor.action.revealDefinition", "when": "editorHasDefinitionProvider && editorTextFocus" },
    { "key": "shift+f12",    "command": "editor.action.goToReferences",   "when": "editorHasReferenceProvider && editorTextFocus" },
    { "key": "ctrl+shift+o", "mac": "cmd+shift+o", "command": "workbench.action.gotoSymbol", "when": "editorHasDocumentSymbolProvider && editorTextFocus" },
    { "key": "f2",           "command": "editor.action.rename",           "when": "editorHasRenameProvider && editorTextFocus && !editorReadonly" },
    { "key": "f8",           "command": "editor.action.marker.next",     "when": "editorFocus" },
    { "key": "shift+f8",     "command": "editor.action.marker.prev",     "when": "editorFocus" },
    { "key": "ctrl+.",       "mac": "cmd+.",       "command": "editor.action.quickFix", "when": "editorHasCodeActionsProvider && editorTextFocus && !editorReadonly" },
    { "key": "ctrl+space",   "mac": "ctrl+space",  "command": "editor.action.triggerSuggest", "when": "editorTextFocus && !editorReadonly" },
    { "key": "escape",       "command": "closeHoverWidget", "when": "editorHoverVisible && textInputFocus" },
    { "key": "escape",       "command": "hideSuggestWidget", "when": "suggestWidgetVisible && textInputFocus" },
    { "key": "down",         "command": "selectNextSuggestion",     "when": "suggestWidgetVisible && textInputFocus" },
    { "key": "up",           "command": "selectPrevSuggestion",     "when": "suggestWidgetVisible && textInputFocus" },
    { "key": "tab",          "command": "acceptSelectedSuggestion", "when": "suggestWidgetVisible && textInputFocus" },
    { "key": "enter",        "command": "acceptSelectedSuggestion", "when": "suggestWidgetVisible && textInputFocus" },

    // ---- The file tree ---------------------------------------------------
    // Gated on the side bar having the keyboard, so every one of these still
    // means what it always meant in the text. VS Code's `list.*` identifiers,
    // because a tree is a list and these are what its explorer binds.
    //
    // *Before* the quick-open block below, so that a prompt's keys win while one
    // is open: the tree keeps focus while it asks for a name, and `enter` then
    // has to mean "accept the name" rather than "open the selected row".
    { "key": "down",         "command": "list.focusDown",   "when": "sideBarFocus" },
    { "key": "up",           "command": "list.focusUp",     "when": "sideBarFocus" },
    { "key": "right",        "command": "list.expand",      "when": "sideBarFocus" },
    { "key": "left",         "command": "list.collapse",    "when": "sideBarFocus" },
    { "key": "home",         "command": "list.focusFirst",  "when": "sideBarFocus" },
    { "key": "end",          "command": "list.focusLast",   "when": "sideBarFocus" },
    { "key": "enter",        "command": "list.select",      "when": "sideBarFocus" },
    { "key": "escape",       "command": "workbench.action.focusActiveEditorGroup", "when": "sideBarFocus" },
    // Changing the files themselves. VS Code's identifiers, and its keys — `F2`
    // and `delete` mean something else in the text, which is why both are gated.
    { "key": "f2",           "command": "renameFile",           "when": "filesExplorerFocus" },
    // `cmd+backspace` on macOS, where the key labelled Delete reports as
    // Backspace and forward-delete is the awkward one. VS Code's explorer binds
    // the same pair.
    { "key": "delete",       "mac": "cmd+backspace", "command": "deleteFile",  "when": "filesExplorerFocus" },
    // `mac` overrides on both, or the general `cmd+n` rule above wins on macOS
    // and the platform-standard key opens an untitled editor instead of the
    // tree's prompt. A focus-specific rule only overrides a general one when it
    // is bound for the same platform.
    { "key": "ctrl+n",       "mac": "cmd+n",       "command": "explorer.newFile",   "when": "filesExplorerFocus" },
    { "key": "ctrl+shift+n", "mac": "cmd+shift+n", "command": "explorer.newFolder", "when": "filesExplorerFocus" },
    // Every one of these is gated on `filesExplorerFocus` rather than
    // `sideBarFocus`. The side bar has two tenants, and `sideBarFocus` is true
    // for both — so on the wider key these would reach the *hidden* tree's
    // selection while the source-control view is on screen. `ctrl+z` is the
    // one that shows why it matters: it would take back a file operation
    // nothing on screen mentions.
    //
    // The tree's own undo. The general `ctrl+z` above is gated on
    // `textInputFocus`, which is false while a region has the keyboard — so
    // without this the key does not resolve at all in the tree.
    { "key": "ctrl+z",       "mac": "cmd+z",       "command": "undo", "when": "filesExplorerFocus" },
    { "key": "ctrl+shift+e", "mac": "cmd+shift+e", "command": "workbench.files.action.focusFilesExplorer" },
    // The side bar's other tenant. Both of these open the container, switch to
    // the view and give it the keyboard, which is what VS Code's
    // `workbench.view.*` do — one key to reach a thing you mean to act on.
    { "key": "ctrl+shift+g", "mac": "cmd+shift+g", "command": "workbench.view.scm" },
    // The repository's own keys, gated on the source-control view having the
    // keyboard: `enter` opens a file there and means something else entirely in
    // the text, and `ctrl+enter` is VS Code's own commit key.
    { "key": "ctrl+enter",   "mac": "cmd+enter",   "command": "git.commit", "when": "deco.sourceControlFocus" },

    // ---- Quick open -----------------------------------------------------
    // Last, so these win whenever a prompt holds the keyboard: a later rule takes
    // precedence, as in VS Code, and every key here is also bound to something in
    // the editor or to another widget.
    { "key": "escape",       "command": "workbench.action.closeQuickOpen",              "when": "inQuickOpen" },
    { "key": "enter",        "command": "workbench.action.acceptSelectedQuickOpenItem",  "when": "inQuickOpen" },
    { "key": "down",         "command": "workbench.action.quickOpenSelectNext",          "when": "inQuickOpen" },
    { "key": "up",           "command": "workbench.action.quickOpenSelectPrevious",      "when": "inQuickOpen" },
    { "key": "tab",          "command": "workbench.action.quickOpenSelectNext",          "when": "inQuickOpen" },
    { "key": "shift+tab",    "command": "workbench.action.quickOpenSelectPrevious",      "when": "inQuickOpen" },

    // ---- Editors and tabs -----------------------------------------------
    { "key": "ctrl+tab",       "command": "workbench.action.nextEditor" },
    { "key": "ctrl+shift+tab", "command": "workbench.action.previousEditor" },
    { "key": "ctrl+1", "mac": "cmd+1", "command": "workbench.action.focusFirstEditorGroup" },
    { "key": "ctrl+2", "mac": "cmd+2", "command": "workbench.action.focusSecondEditorGroup" },
    { "key": "ctrl+3", "mac": "cmd+3", "command": "workbench.action.focusThirdEditorGroup" },
    { "key": "ctrl+\\", "mac": "cmd+\\", "command": "workbench.action.splitEditor" },

    // ---- View -----------------------------------------------------------
    { "key": "alt+z",        "command": "editor.action.toggleWordWrap" },
    { "key": "ctrl+b",       "mac": "cmd+b",       "command": "workbench.action.toggleSidebarVisibility" },
    { "key": "ctrl+j",       "mac": "cmd+j",       "command": "workbench.action.togglePanel" },
    { "key": "ctrl+`",       "command": "workbench.action.terminal.toggleTerminal" },
    { "key": "ctrl+k z",     "mac": "cmd+k z",     "command": "workbench.action.toggleZenMode" },
    { "key": "ctrl+=",       "mac": "cmd+=",       "command": "workbench.action.zoomIn" },
    { "key": "ctrl+-",       "mac": "cmd+-",       "command": "workbench.action.zoomOut" },
    { "key": "ctrl+numpad0", "mac": "cmd+numpad0", "command": "workbench.action.zoomReset" },

    // ---- Preferences ----------------------------------------------------
    { "key": "ctrl+,",        "mac": "cmd+,",        "command": "workbench.action.openSettings" },
    { "key": "ctrl+k ctrl+s", "mac": "cmd+k cmd+s",  "command": "workbench.action.openGlobalKeybindings" },
    { "key": "ctrl+k ctrl+t", "mac": "cmd+k cmd+t",  "command": "workbench.action.selectTheme" },
    { "key": "ctrl+k m",      "mac": "cmd+k m",      "command": "workbench.action.editor.changeLanguageMode" },

    // ---- Remote ---------------------------------------------------------
    { "key": "ctrl+alt+o", "mac": "cmd+alt+o", "command": "deco.remote.showMenu" }
]"#;

/// Parses the built-in keymap for `platform`.
///
/// Any entry that fails to parse is dropped silently here — the constant is
/// covered by a test that asserts it parses cleanly, so a failure at runtime
/// would be a deco bug rather than something the user can act on.
pub fn default_rules(platform: Platform) -> Vec<Rule> {
    parse(DEFAULT_KEYBINDINGS_JSONC, platform, Source::Default)
        .map(|parsed| parsed.rules)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Chord;
    use crate::resolver::{ChordState, Keymap, Resolution};
    use crate::when::ContextKeys;

    fn parsed(platform: Platform) -> crate::binding::ParsedKeybindings {
        parse(DEFAULT_KEYBINDINGS_JSONC, platform, Source::Default).unwrap()
    }

    #[test]
    fn the_default_keymap_parses_cleanly_on_every_platform() {
        for platform in [Platform::Linux, Platform::Mac, Platform::Windows] {
            let parsed = parsed(platform);
            assert!(
                parsed.problems.is_empty(),
                "{platform:?} produced problems: {:?}",
                parsed.problems
            );
            assert!(!parsed.rules.is_empty());
        }
    }

    #[test]
    fn every_platform_gets_the_same_number_of_bindings() {
        let linux = parsed(Platform::Linux).rules.len();
        let mac = parsed(Platform::Mac).rules.len();
        let windows = parsed(Platform::Windows).rules.len();
        assert_eq!((linux, mac), (linux, windows));
        assert_eq!(linux, mac);
    }

    #[test]
    fn mac_uses_command_where_other_platforms_use_control() {
        let mac = Keymap::from_rules(default_rules(Platform::Mac));
        let save = mac.keys_for_command("workbench.action.files.save");
        assert_eq!(
            save.first().map(|k| k.to_string()),
            Some("cmd+s".to_owned())
        );

        let linux = Keymap::from_rules(default_rules(Platform::Linux));
        let save = linux.keys_for_command("workbench.action.files.save");
        assert_eq!(
            save.first().map(|k| k.to_string()),
            Some("ctrl+s".to_owned())
        );
    }

    #[test]
    fn the_defaults_resolve_the_keys_users_expect() {
        let km = Keymap::from_rules(default_rules(Platform::Linux));
        let mut ctx = ContextKeys::with_platform_defaults();
        ctx.set("textInputFocus", true);
        ctx.set("editorTextFocus", true);
        ctx.set("editorFocus", true);

        for (key, command) in [
            ("ctrl+s", "workbench.action.files.save"),
            ("ctrl+shift+p", "workbench.action.showCommands"),
            ("ctrl+p", "workbench.action.quickOpen"),
            ("ctrl+z", "undo"),
            ("ctrl+/", "editor.action.commentLine"),
            ("ctrl+b", "workbench.action.toggleSidebarVisibility"),
            ("alt+up", "editor.action.moveLinesUpAction"),
        ] {
            let mut state = ChordState::new();
            let resolved = km.resolve(&mut state, Chord::parse(key).unwrap(), &ctx);
            assert_eq!(
                resolved,
                Resolution::Match {
                    command: command.to_owned(),
                    args: None
                },
                "{key} did not resolve to {command}"
            );
        }
    }

    #[test]
    fn the_comment_chord_resolves_in_two_presses() {
        let km = Keymap::from_rules(default_rules(Platform::Linux));
        let mut ctx = ContextKeys::with_platform_defaults();
        ctx.set("editorTextFocus", true);
        let mut state = ChordState::new();

        let first = km.resolve(&mut state, Chord::parse("ctrl+k").unwrap(), &ctx);
        assert!(matches!(first, Resolution::Pending { .. }));
        let second = km.resolve(&mut state, Chord::parse("ctrl+c").unwrap(), &ctx);
        assert_eq!(
            second,
            Resolution::Match {
                command: "editor.action.addCommentLine".into(),
                args: None
            }
        );
    }

    #[test]
    fn go_to_symbol_needs_a_server_that_offers_symbols() {
        // Ungated, `ctrl+shift+o` would resolve to a command that can only report
        // that the server cannot answer — a key that looks broken rather than
        // one that is simply not available here.
        let km = Keymap::from_rules(default_rules(Platform::Linux));
        let mut ctx = ContextKeys::with_platform_defaults();
        ctx.set("textInputFocus", true);
        ctx.set("editorTextFocus", true);

        let mut state = ChordState::new();
        assert_eq!(
            km.resolve(&mut state, Chord::parse("ctrl+shift+o").unwrap(), &ctx),
            Resolution::NoMatch
        );

        ctx.set("editorHasDocumentSymbolProvider", true);
        let mut state = ChordState::new();
        assert_eq!(
            km.resolve(&mut state, Chord::parse("ctrl+shift+o").unwrap(), &ctx),
            Resolution::Match {
                command: "workbench.action.gotoSymbol".into(),
                args: None
            }
        );
    }

    #[test]
    fn readonly_editors_do_not_resolve_mutating_commands() {
        let km = Keymap::from_rules(default_rules(Platform::Linux));
        let mut ctx = ContextKeys::with_platform_defaults();
        ctx.set("textInputFocus", true);
        ctx.set("editorReadonly", true);

        let mut state = ChordState::new();
        assert_eq!(
            km.resolve(&mut state, Chord::parse("ctrl+z").unwrap(), &ctx),
            Resolution::NoMatch
        );
        // Copying is still allowed.
        let mut state = ChordState::new();
        assert_eq!(
            km.resolve(&mut state, Chord::parse("ctrl+c").unwrap(), &ctx),
            Resolution::Match {
                command: "editor.action.clipboardCopyAction".into(),
                args: None
            }
        );
    }

    #[test]
    fn escape_picks_the_most_specific_binding_available() {
        let km = Keymap::from_rules(default_rules(Platform::Linux));
        let mut ctx = ContextKeys::with_platform_defaults();
        ctx.set("editorFocus", true);
        ctx.set("textInputFocus", true);

        // With the suggest widget open, Escape closes it rather than clearing
        // the selection.
        ctx.set("suggestWidgetVisible", true);
        ctx.set("editorHasSelection", true);
        let mut state = ChordState::new();
        assert_eq!(
            km.resolve(&mut state, Chord::parse("escape").unwrap(), &ctx),
            Resolution::Match {
                command: "hideSuggestWidget".into(),
                args: None
            }
        );
    }

    #[test]
    fn quit_is_bound_once_per_platform() {
        let linux = Keymap::from_rules(default_rules(Platform::Linux));
        let mut ctx = ContextKeys::new();
        ctx.set("isMac", false);
        let mut state = ChordState::new();
        assert_eq!(
            linux.resolve(&mut state, Chord::parse("ctrl+q").unwrap(), &ctx),
            Resolution::Match {
                command: "workbench.action.quit".into(),
                args: None
            }
        );

        let mut ctx = ContextKeys::new();
        ctx.set("isMac", true);
        let mut state = ChordState::new();
        assert_eq!(
            linux.resolve(&mut state, Chord::parse("ctrl+q").unwrap(), &ctx),
            Resolution::NoMatch
        );
    }

    #[test]
    fn no_two_defaults_share_a_key_and_when_clause() {
        // An exact duplicate means one of them can never fire, which is always
        // a mistake in the defaults rather than a deliberate override.
        let rules = default_rules(Platform::Linux);
        let mut seen: Vec<(String, Option<String>)> = Vec::new();
        for rule in &rules {
            let b = rule.binding();
            let entry = (b.key.to_string(), b.when.as_ref().map(|w| w.to_string()));
            assert!(
                !seen.contains(&entry),
                "duplicate default binding for {} when {:?}",
                entry.0,
                entry.1
            );
            seen.push(entry);
        }
    }
}
