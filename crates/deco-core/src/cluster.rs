//! The unit text is laid out in: the extended grapheme cluster.
//!
//! A cluster is what a reader sees as one character: a letter with its
//! combining accents (`e` + U+0301), a consonant cluster in an Indic script,
//! a kana with a voicing mark, a flag made of two regional indicators, or an
//! emoji joined with U+200D. Its characters share the cells of the first one,
//! so measuring them one at a time, as if each took a column, puts every
//! following character on the row too far right.
//!
//! Caret motion already moves by cluster (see [`crate::movement`]). This
//! module gives layout the same unit: every display-width calculation in this
//! crate and in the frontends walks clusters and measures each with
//! [`width`].
//!
//! Surrogate pairs are a separate matter: a character outside the Basic
//! Multilingual Plane is two UTF-16 code units, which [`crate::position`]
//! converts, and is one `char` and usually one cluster here.

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// The clusters of `text`, each with its length in UTF-16 code units.
pub fn clusters(text: &str) -> impl Iterator<Item = (&str, u32)> {
    text.graphemes(true)
        .map(|cluster| (cluster, cluster.encode_utf16().count() as u32))
}

/// The columns `cluster` occupies, never fewer than one.
///
/// A character replaced by a stand-in when drawn (see
/// [`crate::stand_in`]) takes the stand-in's single column, whatever its own
/// width. Anything else takes the width of the whole sequence, so a combining
/// mark adds nothing to its base and an emoji sequence is as wide as one
/// emoji. One column at least, so a caret placed after a zero-width cluster
/// still moves.
///
/// A tab has no fixed width; callers expand it to the next tab stop before
/// asking.
pub fn width(cluster: &str) -> usize {
    let mut chars = cluster.chars();
    if let (Some(only), None) = (chars.next(), chars.next()) {
        if crate::stand_in::stand_in(only).is_some() {
            return 1;
        }
    }
    cluster.width().max(1)
}

/// The columns `text` occupies, with no tabs in it.
pub fn text_width(text: &str) -> usize {
    clusters(text).map(|(cluster, _)| width(cluster)).sum()
}

/// What is drawn for `cluster`: its stand-in when it is a single character
/// that has one, or `None` when it is drawn as itself.
///
/// A joiner or variation selector inside a longer cluster belongs to the
/// emoji or letter it modifies and is drawn with it; on its own it is
/// replaced, because then nothing would show it.
pub fn stand_in(cluster: &str) -> Option<char> {
    let mut chars = cluster.chars();
    match (chars.next(), chars.next()) {
        (Some(only), None) => crate::stand_in::stand_in(only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_combining_mark_adds_no_column_to_its_base() {
        assert_eq!(text_width("e\u{301}x"), 2);
        assert_eq!(text_width("か\u{3099}"), 2);
        assert_eq!(text_width("क\u{94d}ष"), 2);
    }

    #[test]
    fn an_emoji_sequence_is_as_wide_as_one_emoji() {
        assert_eq!(text_width("👨\u{200d}👩\u{200d}👧"), 2);
        assert_eq!(text_width("🇯🇵"), 2);
        assert_eq!(text_width("👍🏽"), 2);
    }

    #[test]
    fn a_character_drawn_as_a_stand_in_takes_one_column() {
        assert_eq!(text_width("a\u{200b}b"), 3);
        assert_eq!(text_width("a\u{202e}b"), 3);
        assert_eq!(text_width("\x1b"), 1);
        assert_eq!(stand_in("\u{200b}"), Some(crate::stand_in::MARKER));
        assert_eq!(
            stand_in("a\u{200d}"),
            None,
            "a joiner inside a cluster is drawn with it"
        );
    }

    #[test]
    fn clusters_carry_their_utf16_length() {
        let lengths: Vec<u32> = clusters("a😀e\u{301}").map(|(_, n)| n).collect();
        assert_eq!(lengths, [1, 2, 2]);
    }
}
