//! Reading and writing a document's file in its encoding.
//!
//! Every local read of a file the editor opens and every local write of a
//! document go through here, so `files.encoding` and the byte order mark
//! apply to all of them: opening, reverting, reloading after another program
//! or a branch switch changed the file, saving, saving as and saving all.
//!
//! A remote session is not covered: its server sends and receives text, and
//! reads files as UTF-8.

use std::path::Path;

use deco_config::Settings;
use deco_encoding::Encoding;

/// The encoding a file at `path` is read in when it has no byte order mark:
/// `files.encoding`, as resolved for the file's language.
pub fn default_encoding(settings: &Settings, path: &Path) -> Encoding {
    let language = deco_editor::document::language_for_path(path);
    deco_config::EditorSettings::resolve(settings, language).encoding
}

/// Reads `path` as text, in `encoding` unless a byte order mark says
/// otherwise, and returns the encoding used with the text.
///
/// The error names the file and, for bytes that are not valid in the
/// encoding, where they are and which setting chooses another.
pub fn read(path: &Path, encoding: Encoding) -> std::io::Result<(String, Encoding)> {
    let bytes = std::fs::read(path)?;
    deco_encoding::decode(&bytes, encoding).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{error}; `files.encoding` chooses the encoding to read it in"),
        )
    })
}

/// Reads `path` in `encoding`, which the user chose, whatever byte order
/// mark it starts with (`Reopen with Encoding`).
pub fn read_as(path: &Path, encoding: Encoding) -> std::io::Result<(String, Encoding)> {
    let bytes = std::fs::read(path)?;
    deco_encoding::decode_as(&bytes, encoding).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{error}; the file was left as it was"),
        )
    })
}

/// Writes `text` to `path` in `encoding`.
///
/// Nothing is written when the encoding cannot hold the text, so a failed
/// save never leaves a file half-converted.
pub fn write(path: &Path, text: &str, encoding: Encoding) -> std::io::Result<()> {
    let bytes = deco_encoding::encode(text, encoding).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("not saved: {error}"),
        )
    })?;
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deco-text-file-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("file.txt")
    }

    #[test]
    fn a_file_is_written_back_in_the_encoding_it_was_read_in() {
        let path = scratch("round-trip");
        std::fs::write(&path, [0x93, 0xFA, 0x96, 0x7B, b'\n']).unwrap();
        let (text, encoding) = read(&path, Encoding::ShiftJis).unwrap();
        assert_eq!((text.as_str(), encoding), ("日本\n", Encoding::ShiftJis));
        write(&path, "日本語\n", encoding).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, b'\n']
        );
    }

    #[test]
    fn bytes_not_valid_in_the_encoding_are_refused_and_named() {
        let path = scratch("invalid");
        std::fs::write(&path, [b'a', 0xFF]).unwrap();
        let error = read(&path, Encoding::Utf8).unwrap_err();
        assert!(error.to_string().contains("UTF-8 at byte 1"), "{error}");
        assert!(error.to_string().contains("files.encoding"), "{error}");
    }

    #[test]
    fn a_save_the_encoding_cannot_hold_leaves_the_file_alone() {
        let path = scratch("unencodable");
        std::fs::write(&path, b"before\n").unwrap();
        let error = write(&path, "日本\n", Encoding::Windows1252).unwrap_err();
        assert!(error.to_string().contains("U+65E5"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"before\n");
    }

    #[test]
    fn a_chosen_encoding_overrides_the_byte_order_mark() {
        let path = scratch("read-as");
        std::fs::write(&path, [0xEF, 0xBB, 0xBF, b'a']).unwrap();
        assert_eq!(
            read(&path, Encoding::Windows1252).unwrap().1,
            Encoding::Utf8Bom
        );
        assert_eq!(
            read_as(&path, Encoding::Windows1252).unwrap(),
            ("ï»¿a".to_owned(), Encoding::Windows1252)
        );
        std::fs::write(&path, [b'a', 0x82]).unwrap();
        let error = read_as(&path, Encoding::ShiftJis).unwrap_err();
        assert!(error.to_string().contains("byte 1"), "{error}");
    }

    #[test]
    fn the_setting_is_resolved_for_the_file_s_language() {
        let mut settings = Settings::with_defaults();
        settings.set(
            deco_config::Scope::User,
            "files.encoding",
            serde_json::json!("eucjp"),
        );
        assert_eq!(
            default_encoding(&settings, Path::new("a.txt")),
            Encoding::EucJp
        );
    }
}
