//! Characters that are never drawn as themselves, and what is drawn instead.
//!
//! Shared by both frontends so they agree on which characters are replaced.
//! The terminal frontend must not write these to the terminal, which would
//! interpret them; the GPU frontend must not hand them to its text shaper,
//! which would apply the bidirectional ones. Each stand-in is one character,
//! so replacing one does not change a line's column count.

/// The stand-in for a bidirectional formatting or invisible character: `␦`,
/// U+2426.
///
/// Control Pictures has no glyph for these characters. U+2426 is from the same
/// block, so it has the same width as the other stand-ins, one column in every
/// terminal, including those that draw East Asian ambiguous-width characters
/// two columns wide.
pub const MARKER: char = '\u{2426}';

/// The Control Pictures glyph for a C0 control character or DEL, such as `␛`
/// for escape.
///
/// `None` for tab, line feed and carriage return, which a line's content
/// either never holds or has already expanded, and for every other character.
pub fn control_picture(c: char) -> Option<char> {
    match c {
        '\t' | '\n' | '\r' => None,
        // `␀` through `␟`, in order, at U+2400.
        '\0'..='\u{1f}' => char::from_u32(0x2400 + c as u32),
        // Delete, which sits on its own at U+2421.
        '\u{7f}' => Some('␡'),
        _ => None,
    }
}

/// Whether `c` is one of the characters that change the display order of the
/// text around them.
///
/// These are the embeddings, overrides and isolates of Unicode's
/// bidirectional algorithm (UAX #9), and the three implicit marks. A file can
/// use them to display differently from the code that is compiled, which is
/// the Trojan Source attack (CVE-2021-42574).
pub fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        // ARABIC LETTER MARK, LEFT-TO-RIGHT MARK, RIGHT-TO-LEFT MARK.
        '\u{061c}' | '\u{200e}' | '\u{200f}'
        // LRE, RLE, PDF, LRO, RLO.
        | '\u{202a}'..='\u{202e}'
        // LRI, RLI, FSI, PDI.
        | '\u{2066}'..='\u{2069}'
    )
}

/// Whether `c` is a character with no visible form of its own that is not a
/// control or bidirectional character: the zero-width space, joiners and
/// word joiner, the invisible operators, the byte order mark inside a line,
/// the soft hyphen and U+180E.
///
/// They can make two identifiers that look the same differ. VS Code marks
/// them under `editor.unicodeHighlight.invisibleCharacters`.
pub fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}' | '\u{180e}' | '\u{200b}'..='\u{200d}' | '\u{2060}'..='\u{2064}' | '\u{feff}'
    )
}

/// What is drawn for `c` when it is not drawn as itself, or `None` when it is.
pub fn stand_in(c: char) -> Option<char> {
    control_picture(c).or_else(|| (is_bidi_control(c) || is_invisible(c)).then_some(MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_replaced_character_has_a_one_character_stand_in() {
        assert_eq!(stand_in('\x1b'), Some('␛'));
        assert_eq!(stand_in('\x7f'), Some('␡'));
        assert_eq!(stand_in('\u{202e}'), Some(MARKER));
        assert_eq!(stand_in('\u{200b}'), Some(MARKER));
        assert_eq!(stand_in('\t'), None);
        assert_eq!(stand_in('a'), None);
        assert_eq!(stand_in('é'), None);
    }
}
