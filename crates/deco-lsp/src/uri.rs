//! Conversion between filesystem paths and `file:` URIs.
//!
//! Every LSP message identifies a document by URI, never by path, so this
//! conversion applies to every request the editor sends and every diagnostic it
//! receives. Errors here have visible effects. If a server receives
//! `file:///c%3A/src/main.rs` when it expected `file:///c:/src/main.rs`, it
//! reports diagnostics for a document the editor does not consider open, and
//! the diagnostics are never shown.
//!
//! The rules implemented here are VS Code's, because language servers are
//! tested against VS Code:
//!
//! - The drive letter is lower-cased, and its colon is *not* percent-encoded.
//!   `C:\src` is `file:///c:/src`. This is valid because a colon is a `pchar`
//!   under RFC 3986 and needs no escape inside a path.
//! - Backslashes become forward slashes.
//! - A UNC path `\\server\share` becomes `file://server/share`, so the host is
//!   in the authority, not in the path.
//! - Everything outside RFC 3986's `pchar` set is percent-encoded as UTF-8,
//!   which covers spaces, `#`, `?` and every non-ASCII character.
//!
//! This is implemented by hand instead of with a URL crate. Only one scheme is
//! needed, and a general URL parser is a much larger dependency than the
//! roughly two hundred lines below. See the repository's Dependencies section
//! for the general policy.

use std::fmt;
use std::path::{Path, PathBuf};

/// Which set of path rules to apply.
///
/// Explicit rather than read from `cfg!(windows)` so that the test suite
/// covers both sets on every CI platform. A Windows path can also reach a
/// Linux machine in remote development, where the editor and the server run on
/// different operating systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStyle {
    /// `/` separators, no drive letters.
    Unix,
    /// `\` or `/` separators, `C:` drive letters, `\\server\share` UNC paths.
    Windows,
}

impl PathStyle {
    /// The style of the machine this build is running on.
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// How the editor's paths relate to the ones a language server sees.
///
/// On one machine the paths are the same and no mapping is applied. In a
/// remote session the editor holds paths relative to the remote workspace.
/// Quick open lists these paths and documents are keyed by them. The server
/// runs on the remote machine and uses absolute paths under that workspace.
/// The prefix is added and removed here, where paths are converted to URIs,
/// so no other code has to handle it.
///
/// The map also stores the path style. As described for [`PathStyle`], the
/// two sides of a remote session can run different operating systems, and the
/// *server's* rules determine the form of its URIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMap {
    style: PathStyle,
    /// What the editor's paths are relative to, on the machine the server runs
    /// on. `None` when that machine is this one.
    root: Option<PathBuf>,
}

impl PathMap {
    /// The server runs on this machine, so paths are passed through unchanged.
    pub fn host() -> Self {
        Self::local(PathStyle::host())
    }

    /// The same, with an explicit style instead of this machine's style.
    ///
    /// This lets tests cover both sets of rules on any platform, which is also
    /// why [`PathStyle`] is an enum rather than a `cfg!`.
    pub fn local(style: PathStyle) -> Self {
        Self { style, root: None }
    }

    /// The server runs on a remote serving `root`, and the editor's paths are
    /// relative to it.
    ///
    /// The style is Unix because deco's remote support assumes a POSIX remote:
    /// the server is started through a POSIX shell and provisioned with `dd`
    /// and `chmod`. Supporting a Windows remote would require more changes than
    /// a different style here.
    pub fn remote(root: PathBuf) -> Self {
        Self {
            style: PathStyle::Unix,
            root: Some(root),
        }
    }

    /// Which rules the server's paths follow.
    pub fn style(&self) -> PathStyle {
        self.style
    }

    /// The URI naming `path` on the machine the server runs on.
    pub fn to_uri(&self, path: &Path) -> Result<Uri, UriError> {
        let Some(root) = &self.root else {
            return Uri::from_path(path, self.style);
        };
        let path = path.to_string_lossy();
        // An absolute path is left unchanged. The editor holds a path in
        // absolute form only when the server supplied it, and prefixing the
        // root would produce a path that does not exist.
        if is_absolute_in(self.style, &path) {
            return Uri::from_path(Path::new(path.as_ref()), self.style);
        }
        let root = root.to_string_lossy();
        Uri::from_path(Path::new(&join_in(self.style, &root, &path)), self.style)
    }

    /// The path the editor knows `uri` by.
    ///
    /// A URI outside the root keeps its absolute form rather than being refused:
    /// language servers can return locations in indexed dependencies. The file
    /// server separately enforces workspace boundaries and reports any refused
    /// read.
    pub fn from_uri(&self, uri: &Uri) -> Result<PathBuf, UriError> {
        let path = uri.to_path(self.style)?;
        let Some(root) = &self.root else {
            return Ok(path);
        };
        let text = path.to_string_lossy();
        let root = root.to_string_lossy();
        let root = root.trim_end_matches(['/', '\\']);
        // Compared as text rather than with `Path::strip_prefix`, which
        // compares components under *this* machine's rules. Requiring a
        // separator after the root prevents `/home/u/project-secrets` from being
        // treated as a path inside `/home/u/project`.
        match text
            .strip_prefix(root)
            .and_then(|rest| rest.strip_prefix(['/', '\\']))
        {
            Some(rest) => Ok(PathBuf::from(rest)),
            None => Ok(path.clone()),
        }
    }
}

/// Whether `path` is absolute under `style`, rather than under this machine.
///
/// `Path::is_absolute` uses the host's rules, which is wrong for a path from
/// another machine: on Windows it treats `/home/u` as relative, and on Unix it
/// treats `C:\\code` as relative.
fn is_absolute_in(style: PathStyle, path: &str) -> bool {
    match style {
        PathStyle::Unix => path.starts_with('/'),
        PathStyle::Windows => {
            path.starts_with("\\\\")
                || path.starts_with("//")
                || matches!(path.as_bytes().get(1), Some(b':'))
        }
    }
}

/// Joins a relative path onto a root using `style`'s separator.
///
/// Not `PathBuf::join`, which uses this machine's separator. A Windows editor
/// joining `src/main.rs` onto a Linux remote's `/home/u/project` produced
/// `/home/u/project\\src/main.rs`. The backslash was then percent-encoded in
/// the URI as `%5C`, which is a path unknown to the server. This cannot be
/// detected on a single platform, so CI includes a Windows target.
fn join_in(style: PathStyle, root: &str, path: &str) -> String {
    let separator = match style {
        PathStyle::Unix => '/',
        PathStyle::Windows => '\\',
    };
    format!("{}{separator}{path}", root.trim_end_matches(['/', '\\']))
}

/// A `file:` URI, kept as the exact string that goes on the wire.
///
/// Stored rather than re-derived because a server may return a URI spelled
/// differently from the one it received, for example with different escaping
/// of the same characters. Comparisons must use the exact exchanged string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uri(String);

/// Why a path or URI could not be converted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UriError {
    /// A relative path was given. LSP has no working directory, so a relative
    /// path cannot be converted to a URI without assuming one.
    #[error("`{0}` is relative; a file URI needs an absolute path")]
    NotAbsolute(String),
    /// The scheme was not `file:`.
    ///
    /// deco accepts these on receipt, because a server may legitimately refer
    /// to `untitled:` or `jdt:` URIs. They cannot be converted to paths.
    #[error("`{0}` is not a file: URI")]
    NotAFileUri(String),
    /// A `%` escape was truncated or not hexadecimal.
    #[error("`{0}` contains a malformed percent-escape")]
    BadEscape(String),
    /// The decoded bytes were not UTF-8.
    #[error("`{0}` does not decode to valid UTF-8")]
    NotUtf8(String),
    /// The path contained a NUL. No filesystem accepts it, and the underlying C
    /// APIs would truncate the path at that point.
    #[error("path contains a NUL byte")]
    InteriorNul,
    /// The authority named a host that a Unix path cannot represent. Only an
    /// empty authority or `localhost` means this machine.
    #[error("`{0}` names a host other than this machine")]
    NonLocalHost(String),
    /// On Windows, the URI would decode to something other than a drive path
    /// or a `\\server\share` path, such as a device namespace (`\\.\`,
    /// `\\?\`), a host with credentials or a port, or a UNC path assembled
    /// from an escaped `\`.
    #[error("`{0}` does not name a drive or UNC share path")]
    NotAWindowsPath(String),
}

impl fmt::Display for Uri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Uri {
    /// Wraps a URI string that came from a server, without interpreting it.
    ///
    /// Non-`file:` schemes are accepted intentionally. A server may report
    /// diagnostics for a document the editor cannot open, and keeping a URI
    /// that cannot be converted is preferable to dropping the message.
    pub fn from_string(uri: impl Into<String>) -> Self {
        Self(uri.into())
    }

    /// The URI as it appears on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is a `file:` URI, and so convertible to a path.
    pub fn is_file(&self) -> bool {
        self.0.len() >= 5 && self.0[..5].eq_ignore_ascii_case("file:")
    }

    /// Builds a `file:` URI from an absolute path.
    pub fn from_path(path: &Path, style: PathStyle) -> Result<Self, UriError> {
        let path = path.to_string_lossy();
        if path.contains('\0') {
            return Err(UriError::InteriorNul);
        }
        match style {
            PathStyle::Unix => Self::from_unix_path(&path),
            PathStyle::Windows => Self::from_windows_path(&path),
        }
    }

    fn from_unix_path(path: &str) -> Result<Self, UriError> {
        if !path.starts_with('/') {
            return Err(UriError::NotAbsolute(path.to_owned()));
        }
        Ok(Self(format!("file://{}", encode_path(path))))
    }

    fn from_windows_path(path: &str) -> Result<Self, UriError> {
        let path = path.replace('\\', "/");

        // UNC: `//server/share/...` puts the server in the authority. This is
        // what VS Code emits, and it is the only form that round-trips:
        // `to_path` refuses `file:////server/share`.
        if let Some(rest) = path.strip_prefix("//") {
            let (host, tail) = match rest.split_once('/') {
                Some((host, tail)) => (host, format!("/{tail}")),
                None => (rest, String::new()),
            };
            if host.is_empty() {
                return Err(UriError::NotAbsolute(path.clone()));
            }
            return Ok(Self(format!(
                "file://{}{}",
                encode_segment(host),
                encode_path(&tail)
            )));
        }

        if let Some(drive) = drive_letter(&path) {
            let rest = &path[2..];
            // VS Code lower-cases the drive letter so that two spellings of the
            // same file do not become two documents. The colon stays literal.
            return Ok(Self(format!(
                "file:///{}:{}",
                drive.to_ascii_lowercase(),
                encode_path(rest)
            )));
        }

        if path.starts_with('/') {
            // A rooted path with no drive, e.g. `\src\main.rs`. It is ambiguous
            // without the current drive, but a URI can still be formed, which
            // is preferable to rejecting it.
            return Ok(Self(format!("file://{}", encode_path(&path))));
        }

        Err(UriError::NotAbsolute(path))
    }

    /// Converts back to a path, or fails if this is not a `file:` URI.
    pub fn to_path(&self, style: PathStyle) -> Result<PathBuf, UriError> {
        if !self.is_file() {
            return Err(UriError::NotAFileUri(self.0.clone()));
        }
        let rest = &self.0[5..];

        // `file:/x`, `file://x` and `file:///x` are all used in practice. Only
        // the third has an empty authority. In the second, the text up to the
        // next `/` is a host.
        let (authority, path) = if let Some(after) = rest.strip_prefix("//") {
            match after.find('/') {
                Some(slash) => (&after[..slash], &after[slash..]),
                None => (after, ""),
            }
        } else {
            ("", rest)
        };

        let authority = decode(authority)?;
        let path = decode(path)?;
        if authority.contains('\0') || path.contains('\0') {
            return Err(UriError::InteriorNul);
        }

        let local = authority.is_empty() || authority.eq_ignore_ascii_case("localhost");
        Ok(match style {
            PathStyle::Unix => {
                // A Unix path cannot represent a host. `localhost` is the only
                // host that means "this machine" and is safe to drop. Any other
                // host is refused, because dropping it would open the local file
                // with the same path.
                if !local {
                    return Err(UriError::NonLocalHost(self.0.clone()));
                }
                PathBuf::from(if path.is_empty() { "/" } else { &path })
            }
            PathStyle::Windows => {
                // Every `/` becomes `\` below, so a decoded `\` would let the
                // path supply its own separators, for example a leading `\\`
                // that turns a local path into a UNC path.
                if path.contains('\\') {
                    return Err(UriError::NotAWindowsPath(self.0.clone()));
                }
                if !local {
                    // The authority becomes the server of `\\server\share`.
                    // `.` and `?` would select the `\\.\` and `\\?\` device
                    // namespaces, and a separator, credentials or a port have
                    // no place in a UNC server name.
                    if authority == "."
                        || authority == "?"
                        || authority.contains(['/', '\\', '@', ':'])
                    {
                        return Err(UriError::NotAWindowsPath(self.0.clone()));
                    }
                    // `\\server\c:\x` is not a valid UNC path.
                    let tail = path.strip_prefix('/').unwrap_or(&path);
                    let share = tail.split('/').next().unwrap_or("");
                    if drive_letter(share).is_some() {
                        return Err(UriError::NotAWindowsPath(self.0.clone()));
                    }
                    PathBuf::from(format!("\\\\{}{}", authority, path.replace('/', "\\")))
                } else if path.starts_with("//") {
                    // `file:////server/share` would reach a UNC path, including
                    // `\\.\` and `\\?\`, without the checks above. The authority
                    // is the only accepted way to name a server.
                    return Err(UriError::NotAWindowsPath(self.0.clone()));
                } else {
                    let trimmed = path.strip_prefix('/').unwrap_or(&path);
                    if let Some(drive) = drive_letter(trimmed) {
                        // Restore the conventional upper-case drive letter.
                        let mut out = String::with_capacity(trimmed.len());
                        out.push(drive.to_ascii_uppercase());
                        out.push_str(&trimmed[1..].replace('/', "\\"));
                        PathBuf::from(out)
                    } else {
                        PathBuf::from(path.replace('/', "\\"))
                    }
                }
            }
        })
    }
}

/// The drive letter of `C:` or `c:/...`, if the string starts with one.
fn drive_letter(path: &str) -> Option<char> {
    let mut chars = path.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    // `C:` is a drive. `C:foo` without a separator is drive-relative, and
    // treating it as absolute would assume a root directory.
    match (chars.next(), chars.next()) {
        (Some(':'), None) | (Some(':'), Some('/')) => Some(letter),
        _ => None,
    }
}

/// Percent-encodes a path, leaving `/` as the separator.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte == b'/' || is_pchar(byte) {
            out.push(byte as char);
        } else {
            push_escape(&mut out, byte);
        }
    }
    out
}

/// Percent-encodes a single component, escaping `/` as well.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if is_pchar(byte) {
            out.push(byte as char);
        } else {
            push_escape(&mut out, byte);
        }
    }
    out
}

/// RFC 3986 `pchar` minus `%`, which must always be escaped so that an escape
/// in the original filename is not mistaken for one this function produced.
fn is_pchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
        )
}

fn push_escape(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(HEX[(byte >> 4) as usize] as char);
    out.push(HEX[(byte & 0xf) as usize] as char);
}

/// Reverses percent-encoding, decoding the result as UTF-8.
fn decode(input: &str) -> Result<String, UriError> {
    if !input.contains('%') {
        return Ok(input.to_owned());
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .ok_or_else(|| UriError::BadEscape(input.to_owned()))?;
            let high = hex_value(hex[0]).ok_or_else(|| UriError::BadEscape(input.to_owned()))?;
            let low = hex_value(hex[1]).ok_or_else(|| UriError::BadEscape(input.to_owned()))?;
            out.push(high << 4 | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| UriError::NotUtf8(input.to_owned()))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl serde::Serialize for Uri {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Uri {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn on_one_machine_a_path_map_changes_nothing() {
        let map = PathMap::host();
        let path = if cfg!(windows) {
            PathBuf::from(r"C:\code\main.rs")
        } else {
            PathBuf::from("/code/main.rs")
        };
        let uri = map.to_uri(&path).expect("a uri");
        assert_eq!(map.from_uri(&uri).expect("a path"), path);
    }

    #[test]
    fn a_remote_map_adds_the_workspace_and_takes_it_off_again() {
        // In a remote session the editor holds paths relative to the remote
        // workspace, and the server sees the absolute path on the remote side.
        let map = PathMap::remote(PathBuf::from("/home/u/project"));
        let uri = map.to_uri(Path::new("src/main.rs")).expect("a uri");
        assert_eq!(uri.as_str(), "file:///home/u/project/src/main.rs");
        assert_eq!(
            map.from_uri(&uri).expect("a path"),
            PathBuf::from("src/main.rs")
        );
    }

    #[test]
    fn a_remote_map_leaves_a_path_outside_the_workspace_absolute() {
        // Preserve an indexed dependency's absolute path. Workspace access
        // restrictions are enforced by the file server, not URI conversion.
        let map = PathMap::remote(PathBuf::from("/home/u/project"));
        let outside = Uri::from_string("file:///home/u/.cargo/registry/src/lib.rs");
        assert_eq!(
            map.from_uri(&outside).expect("a path"),
            PathBuf::from("/home/u/.cargo/registry/src/lib.rs")
        );

        // An absolute outgoing path is not prefixed to become
        // `/home/u/project/home/u/...`, as a plain join would do.
        let uri = map
            .to_uri(Path::new("/home/u/.cargo/registry/src/lib.rs"))
            .expect("a uri");
        assert_eq!(uri.as_str(), "file:///home/u/.cargo/registry/src/lib.rs");
    }

    #[test]
    fn a_path_is_joined_and_judged_by_the_far_ends_rules() {
        // Regression test for a bug that did not occur on Linux and broke every
        // Windows client connected to a Unix remote. `PathBuf::join` uses
        // *this* machine's separator, so the URI became
        // `…/project%5Csrc/main.rs`. Testing only through `to_uri` would detect
        // the bug only on the platform CI runs under Wine, so the rules are also
        // checked directly here.
        assert_eq!(
            join_in(PathStyle::Unix, "/home/u/project", "src/main.rs"),
            "/home/u/project/src/main.rs"
        );
        assert_eq!(
            join_in(PathStyle::Windows, r"C:\code", "src/main.rs"),
            r"C:\code\src/main.rs"
        );
        // A trailing separator on the root is not doubled.
        assert_eq!(
            join_in(PathStyle::Unix, "/home/u/project/", "src/main.rs"),
            "/home/u/project/src/main.rs"
        );

        // Absoluteness follows the *server's* rules: a leading slash is
        // absolute on Unix and a drive letter is absolute on Windows,
        // regardless of the machine performing the check.
        assert!(is_absolute_in(PathStyle::Unix, "/home/u"));
        assert!(!is_absolute_in(PathStyle::Unix, r"C:\code"));
        assert!(is_absolute_in(PathStyle::Windows, r"C:\code"));
        assert!(is_absolute_in(PathStyle::Windows, r"\\server\share"));
        assert!(!is_absolute_in(PathStyle::Windows, "src/main.rs"));
    }

    #[test]
    fn a_remote_map_does_not_confuse_a_sibling_for_a_child() {
        // `/home/u/project-secrets` only starts with the same text. Stripping
        // the prefix as text alone would report it as `-secrets/notes.txt`
        // *inside* the workspace.
        let map = PathMap::remote(PathBuf::from("/home/u/project"));
        let sibling = Uri::from_string("file:///home/u/project-secrets/notes.txt");
        assert_eq!(
            map.from_uri(&sibling).expect("a path"),
            PathBuf::from("/home/u/project-secrets/notes.txt")
        );
    }

    #[test]
    fn a_remote_map_uses_the_remotes_rules_not_this_machines() {
        // For a Windows client connected to a Linux remote, a backslash is a
        // filename character rather than a separator, and drive letters do not
        // exist. The style must be the server's.
        let map = PathMap::remote(PathBuf::from("/home/u/project"));
        assert_eq!(map.style(), PathStyle::Unix);
        assert_eq!(
            map.to_uri(Path::new("src/main.rs"))
                .expect("a uri")
                .as_str(),
            "file:///home/u/project/src/main.rs"
        );
    }
    use super::*;

    fn unix(path: &str) -> String {
        Uri::from_path(Path::new(path), PathStyle::Unix).unwrap().0
    }

    fn windows(path: &str) -> String {
        Uri::from_path(Path::new(path), PathStyle::Windows)
            .unwrap()
            .0
    }

    #[test]
    fn a_plain_unix_path_needs_no_escaping() {
        assert_eq!(unix("/src/main.rs"), "file:///src/main.rs");
    }

    #[test]
    fn a_windows_drive_letter_is_lowercased_and_its_colon_kept() {
        // The exact spelling VS Code produces. A server that indexes by URI
        // string sees the same key from both editors.
        assert_eq!(windows(r"C:\src\main.rs"), "file:///c:/src/main.rs");
        assert_eq!(windows("C:/src/main.rs"), "file:///c:/src/main.rs");
    }

    #[test]
    fn both_spellings_of_a_drive_produce_one_uri() {
        // Otherwise the same file opened with two spellings becomes two
        // documents, and the second didOpen is a protocol error.
        assert_eq!(windows(r"c:\a"), windows(r"C:\a"));
    }

    #[test]
    fn a_unc_path_puts_the_server_in_the_authority() {
        assert_eq!(
            windows(r"\\build\share\main.rs"),
            "file://build/share/main.rs"
        );
    }

    #[test]
    fn spaces_and_hashes_are_escaped() {
        // An unescaped `#` would start a fragment and truncate the path, so the
        // server would receive the wrong file.
        assert_eq!(unix("/a b/c#d"), "file:///a%20b/c%23d");
        assert_eq!(unix("/q?x"), "file:///q%3Fx");
    }

    #[test]
    fn a_literal_percent_is_escaped_first() {
        // `/a%20b` is a directory whose name contains a percent sign. Emitting
        // it unescaped would decode back to `/a b`, a different file.
        let uri = unix("/a%20b");
        assert_eq!(uri, "file:///a%2520b");
        assert_eq!(
            Uri(uri).to_path(PathStyle::Unix).unwrap(),
            PathBuf::from("/a%20b")
        );
    }

    #[test]
    fn non_ascii_is_encoded_as_utf8_bytes() {
        assert_eq!(unix("/日本語.rs"), "file:///%E6%97%A5%E6%9C%AC%E8%AA%9E.rs");
    }

    #[test]
    fn characters_that_are_legal_in_a_path_stay_literal() {
        // Over-escaping changes the URI string, so a server comparing URI
        // strings would see a document it was never told about.
        assert_eq!(unix("/a+b,c;d=e@f!g"), "file:///a+b,c;d=e@f!g");
    }

    #[test]
    fn a_relative_path_is_refused() {
        assert_eq!(
            Uri::from_path(Path::new("src/main.rs"), PathStyle::Unix),
            Err(UriError::NotAbsolute("src/main.rs".into()))
        );
        assert!(matches!(
            Uri::from_path(Path::new(r"src\main.rs"), PathStyle::Windows),
            Err(UriError::NotAbsolute(_))
        ));
    }

    #[test]
    fn a_drive_relative_windows_path_is_refused() {
        // `C:main.rs` means "main.rs on drive C's current directory", which is
        // not a location LSP can express.
        assert!(matches!(
            Uri::from_path(Path::new("C:main.rs"), PathStyle::Windows),
            Err(UriError::NotAbsolute(_))
        ));
    }

    #[test]
    fn a_nul_is_refused_rather_than_truncated() {
        assert_eq!(
            Uri::from_path(Path::new("/a\0b"), PathStyle::Unix),
            Err(UriError::InteriorNul)
        );
    }

    #[test]
    fn paths_round_trip() {
        for path in [
            "/src/main.rs",
            "/a b/c#d",
            "/日本語/ファイル.rs",
            "/a+b,c;d=e",
            "/",
        ] {
            let uri = Uri::from_path(Path::new(path), PathStyle::Unix).unwrap();
            assert_eq!(
                uri.to_path(PathStyle::Unix).unwrap(),
                PathBuf::from(path),
                "round trip failed via {uri}"
            );
        }
    }

    #[test]
    fn windows_paths_round_trip() {
        for (path, expected) in [
            (r"C:\src\main.rs", r"C:\src\main.rs"),
            (r"C:\a b\c#d", r"C:\a b\c#d"),
            (r"\\build\share\x.rs", r"\\build\share\x.rs"),
            // The drive letter comes back upper-cased, which is the
            // conventional spelling and the one Windows APIs report.
            (r"c:\src\main.rs", r"C:\src\main.rs"),
        ] {
            let uri = Uri::from_path(Path::new(path), PathStyle::Windows).unwrap();
            assert_eq!(
                uri.to_path(PathStyle::Windows).unwrap(),
                PathBuf::from(expected),
                "round trip failed via {uri}"
            );
        }
    }

    #[test]
    fn the_three_spellings_of_a_local_file_uri_all_decode() {
        // Servers emit all of these. `file:/x` in particular comes out of
        // several JVM-based servers.
        for spelling in ["file:///src/main.rs", "file:/src/main.rs"] {
            assert_eq!(
                Uri::from_string(spelling).to_path(PathStyle::Unix).unwrap(),
                PathBuf::from("/src/main.rs"),
                "{spelling}"
            );
        }
        // `file://localhost/x` names this machine explicitly.
        assert_eq!(
            Uri::from_string("file://localhost/src/main.rs")
                .to_path(PathStyle::Windows)
                .unwrap(),
            PathBuf::from(r"\src\main.rs")
        );
    }

    #[test]
    fn an_over_escaped_drive_colon_still_decodes() {
        // Some servers escape the colon although it is not required. The
        // editor must accept both forms.
        assert_eq!(
            Uri::from_string("file:///c%3A/src/main.rs")
                .to_path(PathStyle::Windows)
                .unwrap(),
            PathBuf::from(r"C:\src\main.rs")
        );
    }

    #[test]
    fn a_non_file_scheme_is_carried_but_not_converted() {
        // `jdt:` (Eclipse JDT) and `untitled:` both appear on the wire. The
        // URI must be accepted on receipt; only the conversion fails.
        let uri = Uri::from_string("jdt://contents/rt.jar/java.lang/String.class");
        assert!(!uri.is_file());
        assert_eq!(uri.as_str(), "jdt://contents/rt.jar/java.lang/String.class");
        assert!(matches!(
            uri.to_path(PathStyle::Unix),
            Err(UriError::NotAFileUri(_))
        ));
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively() {
        assert!(Uri::from_string("FILE:///a").is_file());
    }

    #[test]
    fn a_truncated_escape_is_an_error_not_a_panic() {
        for bad in ["file:///a%", "file:///a%2", "file:///a%zz"] {
            assert!(
                matches!(
                    Uri::from_string(bad).to_path(PathStyle::Unix),
                    Err(UriError::BadEscape(_))
                ),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn an_escape_that_is_not_utf8_is_an_error() {
        assert!(matches!(
            Uri::from_string("file:///a%FF%FEb").to_path(PathStyle::Unix),
            Err(UriError::NotUtf8(_))
        ));
    }

    #[test]
    fn an_escaped_nul_is_refused() {
        // `%00` would otherwise become a path that C APIs truncate, so a URI
        // pointing at `/etc/passwd%00.txt` could reach `/etc/passwd`.
        assert_eq!(
            Uri::from_string("file:///etc/passwd%00.txt").to_path(PathStyle::Unix),
            Err(UriError::InteriorNul)
        );
    }

    #[test]
    fn a_foreign_host_is_refused_on_unix() {
        // Dropping the host would open the local file with the same path.
        for uri in ["file://otherhost/etc/passwd", "file://otherhost"] {
            assert_eq!(
                Uri::from_string(uri).to_path(PathStyle::Unix),
                Err(UriError::NonLocalHost(uri.into())),
                "{uri}"
            );
        }
        // `localhost` is matched case-insensitively.
        assert_eq!(
            Uri::from_string("file://LOCALHOST/src/main.rs")
                .to_path(PathStyle::Unix)
                .unwrap(),
            PathBuf::from("/src/main.rs")
        );
    }

    #[test]
    fn windows_uris_that_are_not_a_drive_or_share_path_are_refused() {
        for uri in [
            // Device namespaces `\\.\` and `\\?\`.
            "file://./pipe/x",
            "file://%3F/c:/x",
            "file:////./pipe/x",
            // Separators, credentials or a port in the server name.
            "file://a%2Fb/x",
            "file://a%5Cb/x",
            "file://u@h:445/s/x",
            "file://h:445/s/x",
            // A drive letter where the share name belongs.
            "file://host/c:/x",
            "file://host/C:",
            // An escaped `\` supplying its own separators.
            "file:///%5C%5Ch%5Cs%5Cx",
            "file:///c:/a%5Cb",
            "file://server/share/a%5C..%5Cx",
            // An empty authority followed by a server in the path.
            "file:////server/share/x",
        ] {
            assert_eq!(
                Uri::from_string(uri).to_path(PathStyle::Windows),
                Err(UriError::NotAWindowsPath(uri.into())),
                "{uri}"
            );
        }
    }

    #[test]
    fn valid_windows_uris_still_decode() {
        for (uri, expected) in [
            ("file:///C:/x", r"C:\x"),
            ("file://localhost/C:/x", r"C:\x"),
            ("file://LOCALHOST/c:/x", r"C:\x"),
            ("file://server/share/x", r"\\server\share\x"),
            ("file://server/share/a%20b/c%23d", r"\\server\share\a b\c#d"),
            ("file:///c:/a%20b", r"C:\a b"),
        ] {
            assert_eq!(
                Uri::from_string(uri).to_path(PathStyle::Windows).unwrap(),
                PathBuf::from(expected),
                "{uri}"
            );
        }
    }

    #[test]
    fn a_uri_serialises_as_a_bare_string() {
        let uri = Uri::from_string("file:///a");
        assert_eq!(serde_json::to_string(&uri).unwrap(), "\"file:///a\"");
        assert_eq!(
            serde_json::from_str::<Uri>("\"file:///a\"").unwrap(),
            uri,
            "a URI must not gain a wrapper object on the wire"
        );
    }
}
