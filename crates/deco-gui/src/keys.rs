//! Translating window-system key events into deco chords.
//!
//! A window system reports far more than a terminal does — real modifier state,
//! physical keys, press and release — so this conversion loses much less than
//! the terminal one. It is kept separate from the render loop so it can be
//! tested without opening a window.

use deco_keymap::keys::{Chord, Key, Modifiers, NamedKey};
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey as WinitNamed};

/// Converts a window key event into a chord.
///
/// Returns `None` for releases, for modifier keys pressed alone, and for keys
/// deco has no name for.
pub fn chord_from_event(event: &KeyEvent, modifiers: ModifiersState) -> Option<Chord> {
    chord_from_parts(&event.logical_key, event.state, modifiers)
}

/// The conversion itself, taking only the fields it needs.
///
/// `KeyEvent` carries a platform-specific field with no public constructor, so
/// it cannot be built in a test. Taking the parts separately is what lets every
/// case below be covered without opening a window.
pub fn chord_from_parts(
    logical_key: &WinitKey,
    state: ElementState,
    modifiers: ModifiersState,
) -> Option<Chord> {
    if state != ElementState::Pressed {
        return None;
    }

    let mods = Modifiers {
        ctrl: modifiers.control_key(),
        shift: modifiers.shift_key(),
        alt: modifiers.alt_key(),
        meta: modifiers.super_key(),
    };

    let key = match logical_key {
        WinitKey::Character(text) => {
            let c = text.chars().next()?;
            Key::Char(c.to_lowercase().next().unwrap_or(c))
        }
        WinitKey::Named(named) => Key::Named(match named {
            WinitNamed::Enter => NamedKey::Enter,
            WinitNamed::Tab => NamedKey::Tab,
            WinitNamed::Space => NamedKey::Space,
            WinitNamed::Backspace => NamedKey::Backspace,
            WinitNamed::Delete => NamedKey::Delete,
            WinitNamed::Insert => NamedKey::Insert,
            WinitNamed::Escape => NamedKey::Escape,
            WinitNamed::ArrowLeft => NamedKey::Left,
            WinitNamed::ArrowRight => NamedKey::Right,
            WinitNamed::ArrowUp => NamedKey::Up,
            WinitNamed::ArrowDown => NamedKey::Down,
            WinitNamed::Home => NamedKey::Home,
            WinitNamed::End => NamedKey::End,
            WinitNamed::PageUp => NamedKey::PageUp,
            WinitNamed::PageDown => NamedKey::PageDown,
            WinitNamed::CapsLock => NamedKey::CapsLock,
            WinitNamed::NumLock => NamedKey::NumLock,
            WinitNamed::ScrollLock => NamedKey::ScrollLock,
            WinitNamed::ContextMenu => NamedKey::ContextMenu,
            WinitNamed::Pause => NamedKey::PauseBreak,
            WinitNamed::F1 => NamedKey::F(1),
            WinitNamed::F2 => NamedKey::F(2),
            WinitNamed::F3 => NamedKey::F(3),
            WinitNamed::F4 => NamedKey::F(4),
            WinitNamed::F5 => NamedKey::F(5),
            WinitNamed::F6 => NamedKey::F(6),
            WinitNamed::F7 => NamedKey::F(7),
            WinitNamed::F8 => NamedKey::F(8),
            WinitNamed::F9 => NamedKey::F(9),
            WinitNamed::F10 => NamedKey::F(10),
            WinitNamed::F11 => NamedKey::F(11),
            WinitNamed::F12 => NamedKey::F(12),
            // Modifier keys arrive as their own events; treating them as chords
            // would fire a binding every time the user reached for Shift.
            _ => return None,
        }),
        // Dead keys and unidentified keys carry nothing to bind to.
        _ => return None,
    };

    Some(Chord::new(mods, key))
}

/// Whether a chord should be allowed to type a character.
///
/// The window system already delivers composed text for dead keys and IME
/// input, so this only has to reject chords that were reaching for a command.
pub fn types_text(chord: &Chord) -> bool {
    matches!(chord.key, Key::Char(_))
        && !chord.modifiers.ctrl
        && !chord.modifiers.alt
        && !chord.modifiers.meta
}

#[cfg(test)]
mod tests {
    use super::*;
    fn character(text: &str) -> WinitKey {
        WinitKey::Character(text.into())
    }

    fn press(logical: WinitKey, modifiers: ModifiersState) -> Option<Chord> {
        chord_from_parts(&logical, ElementState::Pressed, modifiers)
    }

    #[test]
    fn a_plain_letter_becomes_a_plain_chord() {
        assert_eq!(
            press(character("a"), ModifiersState::empty()).unwrap(),
            Chord::parse("a").unwrap()
        );
    }

    #[test]
    fn an_uppercase_character_is_lowercased_with_shift_from_the_modifier_state() {
        // Unlike a terminal, the window system tells us Shift is down, so the
        // character itself does not have to be inspected.
        assert_eq!(
            press(character("A"), ModifiersState::SHIFT).unwrap(),
            Chord::parse("shift+a").unwrap()
        );
    }

    #[test]
    fn modifiers_come_from_the_modifier_state() {
        assert_eq!(
            press(
                character("s"),
                ModifiersState::CONTROL | ModifiersState::SHIFT
            )
            .unwrap(),
            Chord::parse("ctrl+shift+s").unwrap()
        );
        assert_eq!(
            press(character("k"), ModifiersState::SUPER).unwrap(),
            Chord::parse("cmd+k").unwrap()
        );
    }

    #[test]
    fn named_keys_are_mapped() {
        for (named, expected) in [
            (WinitNamed::Enter, "enter"),
            (WinitNamed::Escape, "escape"),
            (WinitNamed::ArrowLeft, "left"),
            (WinitNamed::PageDown, "pagedown"),
            (WinitNamed::Backspace, "backspace"),
            (WinitNamed::F5, "f5"),
            (WinitNamed::Space, "space"),
        ] {
            assert_eq!(
                press(WinitKey::Named(named), ModifiersState::empty()).unwrap(),
                Chord::parse(expected).unwrap(),
                "{expected}"
            );
        }
    }

    #[test]
    fn releases_produce_no_chord() {
        assert_eq!(
            chord_from_parts(
                &character("a"),
                ElementState::Released,
                ModifiersState::empty()
            ),
            None
        );
    }

    #[test]
    fn modifier_keys_alone_produce_no_chord() {
        for named in [
            WinitNamed::Shift,
            WinitNamed::Control,
            WinitNamed::Alt,
            WinitNamed::Super,
        ] {
            assert_eq!(
                press(WinitKey::Named(named), ModifiersState::empty()),
                None,
                "{named:?} should not be a chord on its own"
            );
        }
    }

    #[test]
    fn non_ascii_characters_survive() {
        assert_eq!(
            press(character("あ"), ModifiersState::empty()).unwrap().key,
            Key::Char('あ')
        );
    }

    #[test]
    fn only_unmodified_characters_type_text() {
        assert!(types_text(&Chord::parse("a").unwrap()));
        assert!(types_text(&Chord::parse("shift+a").unwrap()));
        assert!(!types_text(&Chord::parse("ctrl+a").unwrap()));
        assert!(!types_text(&Chord::parse("alt+a").unwrap()));
        assert!(!types_text(&Chord::parse("cmd+a").unwrap()));
        assert!(!types_text(&Chord::parse("enter").unwrap()));
    }
}
