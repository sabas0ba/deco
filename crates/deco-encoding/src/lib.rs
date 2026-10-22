//! Text encodings for reading and writing files, as `files.encoding` names
//! them.
//!
//! Implemented from the [WHATWG Encoding Standard](https://encoding.spec.whatwg.org/):
//! its decoder and encoder algorithms, and its index tables, which are
//! copied unchanged into `data/whatwg/` (see the README there for their
//! provenance and licence). No encoding crate is used.
//!
//! # Strict, unlike a browser
//!
//! The standard's decoders replace a malformed sequence with U+FFFD, which
//! suits a browser showing a page. An editor that did the same would write
//! the replacement characters back on the next save and destroy the bytes
//! the file held. [`decode`] therefore refuses malformed input and reports
//! where it is, and [`encode`] refuses a character the encoding cannot
//! represent *or cannot read back as the same character* — the standard's
//! Shift_JIS and EUC-JP encoders write U+00A5 YEN SIGN as `0x5C`, which
//! decodes as a backslash, and a save must never change text silently.

mod index;
mod japanese;
mod single_byte;
mod utf16;

/// An encoding a file can be read and written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// UTF-8 without a byte order mark.
    #[default]
    Utf8,
    /// UTF-8 with a byte order mark, which a save writes.
    Utf8Bom,
    /// UTF-16, little-endian, with a byte order mark.
    Utf16Le,
    /// UTF-16, big-endian, with a byte order mark.
    Utf16Be,
    /// Windows-1252, the Western European code page.
    Windows1252,
    /// ISO-8859-1: every byte is the code point of the same value.
    ///
    /// The real Latin-1, as VS Code's `iso88591` means, rather than the
    /// Encoding Standard's alias for windows-1252.
    Iso88591,
    /// Shift_JIS, as the Encoding Standard defines it (Windows-31J).
    ShiftJis,
    /// EUC-JP, as the Encoding Standard defines it.
    EucJp,
}

impl Encoding {
    /// Every encoding, in the order a list would offer them.
    pub const ALL: [Self; 8] = [
        Self::Utf8,
        Self::Utf8Bom,
        Self::Utf16Le,
        Self::Utf16Be,
        Self::Windows1252,
        Self::Iso88591,
        Self::ShiftJis,
        Self::EucJp,
    ];

    /// The encoding a `files.encoding` value names, using VS Code's
    /// identifiers. `None` for one this crate does not implement.
    pub fn from_setting(value: &str) -> Option<Self> {
        Some(match value {
            "utf8" => Self::Utf8,
            "utf8bom" => Self::Utf8Bom,
            "utf16le" => Self::Utf16Le,
            "utf16be" => Self::Utf16Be,
            "windows1252" => Self::Windows1252,
            "iso88591" => Self::Iso88591,
            "shiftjis" => Self::ShiftJis,
            "eucjp" => Self::EucJp,
            _ => return None,
        })
    }

    /// The `files.encoding` value that names this encoding.
    pub fn setting(self) -> &'static str {
        match self {
            Self::Utf8 => "utf8",
            Self::Utf8Bom => "utf8bom",
            Self::Utf16Le => "utf16le",
            Self::Utf16Be => "utf16be",
            Self::Windows1252 => "windows1252",
            Self::Iso88591 => "iso88591",
            Self::ShiftJis => "shiftjis",
            Self::EucJp => "eucjp",
        }
    }

    /// The name a status bar shows, as VS Code shows it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8 with BOM",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Windows1252 => "Western (Windows 1252)",
            Self::Iso88591 => "Western (ISO 8859-1)",
            Self::ShiftJis => "Japanese (Shift JIS)",
            Self::EucJp => "Japanese (EUC-JP)",
        }
    }

    /// The byte order mark a save writes first, if any.
    fn bom(self) -> &'static [u8] {
        match self {
            Self::Utf8Bom => &[0xEF, 0xBB, 0xBF],
            Self::Utf16Le => &[0xFF, 0xFE],
            Self::Utf16Be => &[0xFE, 0xFF],
            _ => &[],
        }
    }
}

/// Why bytes could not be read as text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not valid {} at byte {offset}", encoding.label())]
pub struct DecodeError {
    /// The encoding the bytes were read in.
    pub encoding: Encoding,
    /// Where the first malformed sequence starts, counted from the start of
    /// the file, byte order mark included.
    pub offset: usize,
}

/// Why text could not be written in an encoding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{} cannot hold {character:?} (U+{:04X}) on line {line}",
    encoding.label(),
    u32::from(*character)
)]
pub struct EncodeError {
    /// The encoding that was asked for.
    pub encoding: Encoding,
    /// The first character it cannot hold, or cannot read back unchanged.
    pub character: char,
    /// The one-based line it is on.
    pub line: usize,
}

/// Reads `bytes` as text.
///
/// A byte order mark decides the encoding whatever `default` says, as in VS
/// Code: UTF-8, UTF-16 LE or UTF-16 BE. Without one, the bytes are read in
/// `default`. The encoding used is returned with the text, so a save writes
/// the file back the same way.
pub fn decode(bytes: &[u8], default: Encoding) -> Result<(String, Encoding), DecodeError> {
    let (encoding, start) = match bytes {
        [0xEF, 0xBB, 0xBF, ..] => (Encoding::Utf8Bom, 3),
        [0xFF, 0xFE, ..] => (Encoding::Utf16Le, 2),
        [0xFE, 0xFF, ..] => (Encoding::Utf16Be, 2),
        // A UTF-16 file must have a byte order mark to be read as UTF-16, as
        // no other check can tell; without one it is read in `default`.
        _ => (default, 0),
    };
    let body = &bytes[start..];
    let shift = |error: DecodeError| DecodeError {
        offset: error.offset + start,
        ..error
    };
    let text =
        match encoding {
            Encoding::Utf8 | Encoding::Utf8Bom => match std::str::from_utf8(body) {
                Ok(text) => text.to_owned(),
                Err(error) => {
                    return Err(DecodeError {
                        encoding,
                        offset: start + error.valid_up_to(),
                    })
                }
            },
            Encoding::Utf16Le => utf16::decode(body, false)
                .map_err(|offset| shift(DecodeError { encoding, offset }))?,
            Encoding::Utf16Be => utf16::decode(body, true)
                .map_err(|offset| shift(DecodeError { encoding, offset }))?,
            Encoding::Windows1252 => single_byte::decode_windows1252(body),
            Encoding::Iso88591 => body.iter().map(|&byte| char::from(byte)).collect(),
            Encoding::ShiftJis => japanese::decode_shift_jis(body)
                .map_err(|offset| shift(DecodeError { encoding, offset }))?,
            Encoding::EucJp => japanese::decode_euc_jp(body)
                .map_err(|offset| shift(DecodeError { encoding, offset }))?,
        };
    Ok((text, encoding))
}

/// Writes `text` in `encoding`, with its byte order mark if it has one.
///
/// Refuses a character the encoding cannot hold, and also one it would
/// write in a form that reads back as a different character, so a save
/// never changes the text it was given.
pub fn encode(text: &str, encoding: Encoding) -> Result<Vec<u8>, EncodeError> {
    let mut out = encoding.bom().to_vec();
    let unencodable = |character: char, at: usize| EncodeError {
        encoding,
        character,
        line: text[..at].matches('\n').count() + 1,
    };
    for (at, character) in text.char_indices() {
        let before = out.len();
        let written = match encoding {
            Encoding::Utf8 | Encoding::Utf8Bom => {
                let mut buffer = [0u8; 4];
                out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                true
            }
            Encoding::Utf16Le | Encoding::Utf16Be => {
                utf16::encode(character, encoding == Encoding::Utf16Be, &mut out);
                true
            }
            Encoding::Windows1252 => single_byte::encode_windows1252(character, &mut out),
            Encoding::Iso88591 => match u8::try_from(u32::from(character)) {
                Ok(byte) => {
                    out.push(byte);
                    true
                }
                Err(_) => false,
            },
            Encoding::ShiftJis => japanese::encode_shift_jis(character, &mut out),
            Encoding::EucJp => japanese::encode_euc_jp(character, &mut out),
        };
        if !written {
            return Err(unencodable(character, at));
        }
        // The standard's legacy encoders map a few characters to the bytes
        // of a different one. Reading the bytes back catches every such
        // case without listing them.
        if needs_round_trip_check(encoding) {
            let bytes = &out[before..];
            let back = match encoding {
                Encoding::ShiftJis => japanese::decode_shift_jis(bytes).ok(),
                Encoding::EucJp => japanese::decode_euc_jp(bytes).ok(),
                _ => Some(single_byte::decode_windows1252(bytes)),
            };
            if back.as_deref().and_then(|back| back.chars().next()) != Some(character) {
                return Err(unencodable(character, at));
            }
        }
    }
    Ok(out)
}

fn needs_round_trip_check(encoding: Encoding) -> bool {
    matches!(
        encoding,
        Encoding::ShiftJis | Encoding::EucJp | Encoding::Windows1252
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_names_its_encoding_and_back() {
        for encoding in Encoding::ALL {
            assert_eq!(Encoding::from_setting(encoding.setting()), Some(encoding));
        }
        assert_eq!(Encoding::from_setting("gbk"), None);
    }

    #[test]
    fn a_byte_order_mark_decides_the_encoding() {
        assert_eq!(
            decode(b"\xEF\xBB\xBFa", Encoding::ShiftJis).unwrap(),
            ("a".to_owned(), Encoding::Utf8Bom)
        );
        assert_eq!(
            decode(b"\xFF\xFEa\x00", Encoding::Utf8).unwrap(),
            ("a".to_owned(), Encoding::Utf16Le)
        );
        assert_eq!(
            decode(b"\xFE\xFF\x00a", Encoding::Utf8).unwrap(),
            ("a".to_owned(), Encoding::Utf16Be)
        );
    }

    #[test]
    fn malformed_input_is_refused_with_its_offset() {
        assert_eq!(
            decode(b"ab\xFF", Encoding::Utf8),
            Err(DecodeError {
                encoding: Encoding::Utf8,
                offset: 2
            })
        );
        // Counted from the start of the file, mark included.
        assert_eq!(
            decode(b"\xEF\xBB\xBFab\xFF", Encoding::Utf8)
                .unwrap_err()
                .offset,
            5
        );
        assert_eq!(decode(b"\xFF\xFEa", Encoding::Utf8).unwrap_err().offset, 2);
    }

    #[test]
    fn every_encoding_writes_its_mark_and_reads_back_what_it_wrote() {
        for encoding in Encoding::ALL {
            // Text each encoding can hold: the Western ones have no kana, and
            // the Japanese ones no accented Latin letters.
            let text = match encoding {
                Encoding::Windows1252 | Encoding::Iso88591 => "line one\nlína two\n",
                Encoding::ShiftJis | Encoding::EucJp => "line one\n二行目 ｱ\n",
                _ => "line one\nlína 二行目 😀\n",
            };
            let bytes = encode(text, encoding).unwrap();
            assert!(bytes.starts_with(encoding.bom()));
            assert_eq!(
                decode(&bytes, encoding).unwrap(),
                (text.to_owned(), encoding),
                "{encoding:?}"
            );
        }
    }

    #[test]
    fn a_character_the_encoding_cannot_hold_is_refused_with_its_line() {
        assert_eq!(
            encode("ok\n日本", Encoding::Windows1252),
            Err(EncodeError {
                encoding: Encoding::Windows1252,
                character: '日',
                line: 2
            })
        );
        assert!(encode("€", Encoding::Iso88591).is_err());
        assert!(encode("€", Encoding::Windows1252).is_ok());
    }

    #[test]
    fn a_character_that_would_read_back_as_another_is_refused() {
        // The standard writes U+00A5 as 0x5C, which reads back as `\`.
        assert_eq!(encode("¥", Encoding::ShiftJis).unwrap_err().character, '¥');
        assert_eq!(encode("¥", Encoding::EucJp).unwrap_err().character, '¥');
        // And U+2212 MINUS SIGN as the bytes of U+FF0D.
        assert_eq!(
            encode("\u{2212}", Encoding::ShiftJis)
                .unwrap_err()
                .character,
            '\u{2212}'
        );
    }
}
