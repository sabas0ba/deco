//! UTF-16 without its byte order mark, which the caller handles.

/// Reads UTF-16, or the offset of the first malformed code unit: an odd
/// trailing byte or an unpaired surrogate.
pub fn decode(bytes: &[u8], big_endian: bool) -> Result<String, usize> {
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|pair| match pair {
            [a, b] if big_endian => Some(u16::from_be_bytes([*a, *b])),
            [a, b] => Some(u16::from_le_bytes([*a, *b])),
            _ => None,
        })
        .collect::<Option<_>>()
        .ok_or(bytes.len() - 1)?;
    let mut out = String::with_capacity(units.len());
    let mut at = 0usize;
    for decoded in char::decode_utf16(units.iter().copied()) {
        let c = decoded.map_err(|_| at * 2)?;
        out.push(c);
        at += c.len_utf16();
    }
    Ok(out)
}

/// Writes `character` as UTF-16.
pub fn encode(character: char, big_endian: bool, out: &mut Vec<u8>) {
    let mut units = [0u16; 2];
    for unit in character.encode_utf16(&mut units) {
        out.extend(if big_endian {
            unit.to_be_bytes()
        } else {
            unit.to_le_bytes()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surrogate_pairs_and_their_errors() {
        let mut bytes = Vec::new();
        encode('😀', false, &mut bytes);
        assert_eq!(bytes, [0x3D, 0xD8, 0x00, 0xDE]);
        assert_eq!(decode(&bytes, false).unwrap(), "😀");
        // A high surrogate on its own.
        assert_eq!(decode(&[b'a', 0, 0x3D, 0xD8], false), Err(2));
        // An odd byte left over.
        assert_eq!(decode(&[b'a', 0, b'b'], false), Err(2));
    }
}
