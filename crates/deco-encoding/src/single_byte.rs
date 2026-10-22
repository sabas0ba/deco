//! windows-1252, from the Encoding Standard's index.

use crate::index;

/// Reads windows-1252. Every byte has a code point, so this cannot fail.
pub fn decode_windows1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&byte| match byte {
            0x00..=0x7F => char::from(byte),
            _ => index::windows1252()
                .code_point(usize::from(byte - 0x80))
                .expect("windows-1252 maps every byte"),
        })
        .collect()
}

/// Writes `character` in windows-1252, or returns false when it has no form.
pub fn encode_windows1252(character: char, out: &mut Vec<u8>) -> bool {
    if character.is_ascii() {
        out.push(character as u8);
        return true;
    }
    match index::windows1252().pointer(character) {
        Some(pointer) => {
            out.push(0x80 + pointer as u8);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_byte_round_trips() {
        let bytes: Vec<u8> = (0..=255).collect();
        let text = decode_windows1252(&bytes);
        let mut back = Vec::new();
        for c in text.chars() {
            assert!(encode_windows1252(c, &mut back));
        }
        assert_eq!(back, bytes);
        assert!(text.contains('€') && text.contains('é'));
    }
}
