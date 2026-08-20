//! Translating terminal key events into deco chords.
//!
//! Terminals are lossy about modifiers. Most of them deliver `Ctrl+A` as the
//! control character `0x01` with no letter attached, and many cannot report
//! `Shift` on a printable key at all because the shifted character *is* the
//! report. The conversions here undo as much of that as the terminal allows,
//! and are kept apart from the event loop so every case can be tested without a
//! terminal attached.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use deco_keymap::keys::{Chord, Key, Modifiers, NamedKey};

/// Converts a terminal key event into a chord, or `None` for events that carry
/// no key (modifier presses, key releases).
pub fn chord_from_event(event: KeyEvent) -> Option<Chord> {
    // With the kitty keyboard protocol enabled a terminal reports releases and
    // repeats too; acting on a release would double every keystroke.
    if event.kind == KeyEventKind::Release {
        return None;
    }

    let mut modifiers = Modifiers {
        ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
        shift: event.modifiers.contains(KeyModifiers::SHIFT),
        alt: event.modifiers.contains(KeyModifiers::ALT),
        meta: event.modifiers.contains(KeyModifiers::SUPER)
            | event.modifiers.contains(KeyModifiers::META),
    };

    let key = match event.code {
        KeyCode::Char(c) => {
            // An uppercase letter means Shift was held whether or not the
            // terminal said so. Punctuation is left alone: `!` is Shift+1 on a
            // US layout and something else entirely elsewhere, so inferring
            // from the character would be wrong on most keyboards.
            if c.is_uppercase() {
                modifiers.shift = true;
            }
            Key::Char(c.to_lowercase().next().unwrap_or(c))
        }
        KeyCode::Enter => Key::Named(NamedKey::Enter),
        KeyCode::Tab => Key::Named(NamedKey::Tab),
        KeyCode::BackTab => {
            // A terminal reports Shift+Tab as its own code rather than as Tab
            // with a modifier.
            modifiers.shift = true;
            Key::Named(NamedKey::Tab)
        }
        KeyCode::Backspace => Key::Named(NamedKey::Backspace),
        KeyCode::Delete => Key::Named(NamedKey::Delete),
        KeyCode::Insert => Key::Named(NamedKey::Insert),
        KeyCode::Esc => Key::Named(NamedKey::Escape),
        KeyCode::Left => Key::Named(NamedKey::Left),
        KeyCode::Right => Key::Named(NamedKey::Right),
        KeyCode::Up => Key::Named(NamedKey::Up),
        KeyCode::Down => Key::Named(NamedKey::Down),
        KeyCode::Home => Key::Named(NamedKey::Home),
        KeyCode::End => Key::Named(NamedKey::End),
        KeyCode::PageUp => Key::Named(NamedKey::PageUp),
        KeyCode::PageDown => Key::Named(NamedKey::PageDown),
        KeyCode::F(n) if (1..=19).contains(&n) => Key::Named(NamedKey::F(n)),
        // Some terminals report Ctrl+Space as NUL under this code rather than as
        // `Char(' ')` with Control. Either way it is the space bar.
        KeyCode::Null => Key::Char(' '),
        _ => return None,
    };

    Some(Chord::new(modifiers, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn chord(code: KeyCode, modifiers: KeyModifiers) -> Chord {
        chord_from_event(event(code, modifiers)).expect("expected a chord")
    }

    #[test]
    fn a_plain_letter_becomes_a_plain_chord() {
        assert_eq!(
            chord(KeyCode::Char('a'), KeyModifiers::NONE),
            Chord::parse("a").unwrap()
        );
    }

    #[test]
    fn control_combinations_carry_their_modifier() {
        assert_eq!(
            chord(KeyCode::Char('s'), KeyModifiers::CONTROL),
            Chord::parse("ctrl+s").unwrap()
        );
        assert_eq!(
            chord(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            Chord::parse("ctrl+shift+p").unwrap()
        );
    }

    #[test]
    fn an_uppercase_letter_implies_shift() {
        // Most terminals report Shift+A as 'A' with no SHIFT modifier at all.
        assert_eq!(
            chord(KeyCode::Char('A'), KeyModifiers::NONE),
            Chord::parse("shift+a").unwrap()
        );
    }

    #[test]
    fn shifted_punctuation_is_not_second_guessed() {
        // `!` is Shift+1 on a US layout and something else elsewhere; adding
        // Shift here would break every non-US keyboard.
        let chord = chord(KeyCode::Char('!'), KeyModifiers::NONE);
        assert_eq!(chord.key, Key::Char('!'));
        assert!(!chord.modifiers.shift);
    }

    #[test]
    fn named_keys_are_mapped() {
        for (code, expected) in [
            (KeyCode::Enter, "enter"),
            (KeyCode::Esc, "escape"),
            (KeyCode::Left, "left"),
            (KeyCode::PageDown, "pagedown"),
            (KeyCode::Home, "home"),
            (KeyCode::Backspace, "backspace"),
            (KeyCode::Delete, "delete"),
            (KeyCode::F(5), "f5"),
        ] {
            assert_eq!(
                chord(code, KeyModifiers::NONE),
                Chord::parse(expected).unwrap(),
                "{expected}"
            );
        }
    }

    #[test]
    fn ctrl_space_reaches_the_binding_however_the_terminal_spells_it() {
        // crossterm's unix parser turns the NUL a terminal sends for Ctrl+Space
        // into `Char(' ')` with CONTROL; other paths use `Null`. Both have to
        // arrive as the chord `ctrl+space` resolves to, or the default binding
        // for Trigger Suggest is one nothing can press.
        for code in [KeyCode::Char(' '), KeyCode::Null] {
            assert_eq!(
                chord(code, KeyModifiers::CONTROL),
                Chord::parse("ctrl+space").unwrap(),
                "{code:?}"
            );
        }
    }

    #[test]
    fn a_plain_space_is_the_character_it_types() {
        assert_eq!(
            chord(KeyCode::Char(' '), KeyModifiers::NONE),
            Chord::parse("space").unwrap()
        );
        assert_eq!(
            chord(KeyCode::Char(' '), KeyModifiers::NONE).key,
            Key::Char(' ')
        );
    }

    #[test]
    fn back_tab_is_shift_tab() {
        assert_eq!(
            chord(KeyCode::BackTab, KeyModifiers::NONE),
            Chord::parse("shift+tab").unwrap()
        );
    }

    #[test]
    fn alt_and_super_are_carried_through() {
        assert_eq!(
            chord(KeyCode::Up, KeyModifiers::ALT),
            Chord::parse("alt+up").unwrap()
        );
        assert_eq!(
            chord(KeyCode::Char('k'), KeyModifiers::SUPER),
            Chord::parse("cmd+k").unwrap()
        );
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut release = event(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(chord_from_event(release), None);
    }

    #[test]
    fn key_repeats_are_treated_as_presses() {
        let mut repeat = event(KeyCode::Char('a'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(chord_from_event(repeat), Some(Chord::parse("a").unwrap()));
    }

    #[test]
    fn unsupported_codes_produce_no_chord() {
        assert_eq!(
            chord_from_event(event(KeyCode::F(30), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            chord_from_event(event(KeyCode::CapsLock, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn non_ascii_characters_survive() {
        let chord = chord(KeyCode::Char('あ'), KeyModifiers::NONE);
        assert_eq!(chord.key, Key::Char('あ'));
    }
}
