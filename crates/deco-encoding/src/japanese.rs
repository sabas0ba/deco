//! Shift_JIS and EUC-JP, following the Encoding Standard's decoder and
//! encoder algorithms, except that a malformed sequence is an error rather
//! than U+FFFD.

use crate::index;

/// Reads Shift_JIS, or the offset of the first malformed sequence.
pub fn decode_shift_jis(bytes: &[u8]) -> Result<String, usize> {
    let mut out = String::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            0x00..=0x80 => {
                out.push(char::from(byte));
                at += 1;
            }
            // Half-width katakana.
            0xA1..=0xDF => {
                out.push(char::from_u32(0xFF61 - 0xA1 + u32::from(byte)).expect("in range"));
                at += 1;
            }
            0x81..=0x9F | 0xE0..=0xFC => {
                let trail = *bytes.get(at + 1).ok_or(at)?;
                let offset = if trail < 0x7F { 0x40 } else { 0x41 };
                let lead_offset = if byte < 0xA0 { 0x81 } else { 0xC1 };
                if !matches!(trail, 0x40..=0x7E | 0x80..=0xFC) {
                    return Err(at);
                }
                let pointer = usize::from(byte - lead_offset) * 188 + usize::from(trail - offset);
                // The user-defined area maps to the Private Use Area.
                let code_point = if (8836..=10715).contains(&pointer) {
                    char::from_u32(0xE000 - 8836 + pointer as u32)
                } else {
                    index::jis0208().code_point(pointer)
                };
                out.push(code_point.ok_or(at)?);
                at += 2;
            }
            _ => return Err(at),
        }
    }
    Ok(out)
}

/// Writes `character` in Shift_JIS, or returns false when it has no form.
pub fn encode_shift_jis(character: char, out: &mut Vec<u8>) -> bool {
    let code_point = u32::from(character);
    match character {
        '\0'..='\u{80}' => out.push(code_point as u8),
        '\u{A5}' => out.push(0x5C),
        '\u{203E}' => out.push(0x7E),
        '\u{FF61}'..='\u{FF9F}' => out.push((code_point - 0xFF61 + 0xA1) as u8),
        _ => {
            let character = if character == '\u{2212}' {
                '\u{FF0D}'
            } else {
                character
            };
            let Some(pointer) = index::shift_jis_pointer(character) else {
                return false;
            };
            let lead = pointer / 188;
            let lead_offset = if lead < 0x1F { 0x81 } else { 0xC1 };
            let trail = pointer % 188;
            let offset = if trail < 0x3F { 0x40 } else { 0x41 };
            out.push((lead + lead_offset) as u8);
            out.push((trail + offset) as u8);
        }
    }
    true
}

/// Reads EUC-JP, or the offset of the first malformed sequence.
pub fn decode_euc_jp(bytes: &[u8]) -> Result<String, usize> {
    let mut out = String::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            0x00..=0x7F => {
                out.push(char::from(byte));
                at += 1;
            }
            // Half-width katakana: 0x8E then one byte.
            0x8E => {
                let trail = *bytes.get(at + 1).ok_or(at)?;
                if !(0xA1..=0xDF).contains(&trail) {
                    return Err(at);
                }
                out.push(char::from_u32(0xFF61 - 0xA1 + u32::from(trail)).expect("in range"));
                at += 2;
            }
            // JIS X 0212: 0x8F then two bytes.
            0x8F => {
                let (Some(&lead), Some(&trail)) = (bytes.get(at + 1), bytes.get(at + 2)) else {
                    return Err(at);
                };
                if !(0xA1..=0xFE).contains(&lead) || !(0xA1..=0xFE).contains(&trail) {
                    return Err(at);
                }
                let pointer = usize::from(lead - 0xA1) * 94 + usize::from(trail - 0xA1);
                out.push(index::jis0212().code_point(pointer).ok_or(at)?);
                at += 3;
            }
            // JIS X 0208: two bytes.
            0xA1..=0xFE => {
                let trail = *bytes.get(at + 1).ok_or(at)?;
                if !(0xA1..=0xFE).contains(&trail) {
                    return Err(at);
                }
                let pointer = usize::from(byte - 0xA1) * 94 + usize::from(trail - 0xA1);
                out.push(index::jis0208().code_point(pointer).ok_or(at)?);
                at += 2;
            }
            _ => return Err(at),
        }
    }
    Ok(out)
}

/// Writes `character` in EUC-JP, or returns false when it has no form.
///
/// As in the standard, JIS X 0212 is read but not written.
pub fn encode_euc_jp(character: char, out: &mut Vec<u8>) -> bool {
    let code_point = u32::from(character);
    match character {
        '\0'..='\u{7F}' => out.push(code_point as u8),
        '\u{A5}' => out.push(0x5C),
        '\u{203E}' => out.push(0x7E),
        '\u{FF61}'..='\u{FF9F}' => {
            out.push(0x8E);
            out.push((code_point - 0xFF61 + 0xA1) as u8);
        }
        _ => {
            let character = if character == '\u{2212}' {
                '\u{FF0D}'
            } else {
                character
            };
            let Some(pointer) = index::jis0208().pointer(character) else {
                return false;
            };
            out.push((pointer / 94 + 0xA1) as u8);
            out.push((pointer % 94 + 0xA1) as u8);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shift_jis(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for c in text.chars() {
            assert!(encode_shift_jis(c, &mut out), "{c:?}");
        }
        out
    }

    fn euc_jp(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for c in text.chars() {
            assert!(encode_euc_jp(c, &mut out), "{c:?}");
        }
        out
    }

    #[test]
    fn shift_jis_reads_and_writes_known_bytes() {
        // 日本語: 0x93FA 0x967B 0x8CEA. ｱ is half-width, one byte.
        let bytes = [0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, b'a', 0xB1];
        assert_eq!(decode_shift_jis(&bytes).unwrap(), "日本語aｱ");
        assert_eq!(shift_jis("日本語aｱ"), bytes);
        // A trail byte in 0x80..: 「ソ」 0x835C has 0x5C as its trail.
        assert_eq!(decode_shift_jis(&[0x83, 0x5C]).unwrap(), "ソ");
        assert_eq!(shift_jis("ソ"), [0x83, 0x5C]);
    }

    #[test]
    fn shift_jis_refuses_a_cut_or_unmapped_sequence() {
        assert_eq!(decode_shift_jis(&[b'a', 0x93]), Err(1));
        assert_eq!(decode_shift_jis(&[0x93, 0x20]), Err(0));
        assert_eq!(decode_shift_jis(&[0xFD]), Err(0));
    }

    #[test]
    fn shift_jis_user_defined_area_is_the_private_use_area() {
        assert_eq!(decode_shift_jis(&[0xF0, 0x40]).unwrap(), "\u{E000}");
    }

    #[test]
    fn euc_jp_reads_and_writes_known_bytes() {
        // 日本語: 0xC6FC 0xCBDC 0xB8EC; ｱ is 0x8E 0xB1.
        let bytes = [0xC6, 0xFC, 0xCB, 0xDC, 0xB8, 0xEC, b'a', 0x8E, 0xB1];
        assert_eq!(decode_euc_jp(&bytes).unwrap(), "日本語aｱ");
        assert_eq!(euc_jp("日本語aｱ"), bytes);
    }

    #[test]
    fn euc_jp_reads_jis_x_0212() {
        // 0x8F 0xA2 0xAF is U+02D8 BREVE in JIS X 0212.
        assert_eq!(decode_euc_jp(&[0x8F, 0xA2, 0xAF]).unwrap(), "\u{02D8}");
        assert_eq!(decode_euc_jp(&[0x8F, 0xA2]), Err(0));
    }

    #[test]
    fn every_jis0208_character_round_trips_through_both() {
        let index = crate::index::jis0208();
        for pointer in 0..11_280 {
            let Some(c) = index.code_point(pointer) else {
                continue;
            };
            let mut bytes = Vec::new();
            if encode_shift_jis(c, &mut bytes) {
                assert_eq!(
                    decode_shift_jis(&bytes).unwrap().chars().next(),
                    Some(c),
                    "{c:?}"
                );
            }
            let mut bytes = Vec::new();
            if encode_euc_jp(c, &mut bytes) {
                assert_eq!(
                    decode_euc_jp(&bytes).unwrap().chars().next(),
                    Some(c),
                    "{c:?}"
                );
            }
        }
    }
}
