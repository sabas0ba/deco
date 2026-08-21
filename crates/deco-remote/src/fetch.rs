//! Getting a deco built for *the remote's* platform, rather than this one's.
//!
//! [`install`](crate::install) can only send the binary this machine is running,
//! so a macOS laptop provisioning a Linux server had nothing to send. This is
//! the part that fetches one — from the same releases the README tells a person
//! to download by hand, verified against the same `SHA256SUMS` it tells them to
//! check.
//!
//! # Plumbing outside, verification inside
//!
//! The transfer and the decompression are done by `curl` and `tar`, which every
//! platform this runs on already ships. The **checksum is computed here**, in
//! deco's own code, against bytes already in memory.
//!
//! That line is deliberate and it is the only one that matters. Shelling out for
//! a download is delegating a chore; shelling out for the integrity check would
//! be delegating the answer to "is this the binary the project published", which
//! is the whole question. An editor that let an external tool decide that would
//! not be checking anything — it would be asking something else whether it had
//! checked.
//!
//! It also keeps the dependency this adds to one crate rather than the roughly
//! forty an in-process HTTPS stack costs, which for a program that argues about
//! its dependency count is not a small consideration.
//!
//! # What is still refused
//!
//! A platform with no published build. `uname` is asked what the remote is, and
//! a combination the release matrix does not carry is refused **by name** rather
//! than answered with the nearest thing — a binary for the wrong libc runs far
//! enough to be confusing and not far enough to work.
//!
//! `uname` also cannot tell glibc from musl, so an Alpine remote is asked for
//! and given the `-gnu` build. That is caught, late but honestly, by the check
//! [`install::ensure`](crate::install::ensure) already makes after uploading:
//! the binary is asked for its version, and one that cannot run does not answer.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::install::Platform;

/// Where releases live, from the manifest rather than written out again here.
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Why a binary for the remote could not be obtained.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// No release is published for that platform.
    #[error(
        "there is no published deco for {platform}, so there is nothing to send. \
         Build one for it and point `--remote-server-path` at that."
    )]
    NoBuildFor {
        /// The remote, as `os-arch`.
        platform: String,
    },
    /// A tool this needs is not on the PATH.
    #[error(
        "`{tool}` is needed to {purpose} and is not on this machine's PATH. \
         Install it, or put a deco built for the remote there yourself and point \
         `--remote-server-path` at it."
    )]
    NoTool {
        /// The program that was looked for.
        tool: &'static str,
        /// What it was needed for, in words.
        purpose: &'static str,
    },
    /// A download did not produce what was asked for.
    #[error("could not download {what} from {url}: {detail}")]
    Download {
        /// Which file, in words.
        what: &'static str,
        /// Where it was asked for.
        url: String,
        /// What went wrong.
        detail: String,
    },
    /// The checksums file has no line for the archive.
    #[error(
        "{asset} is not listed in the SHA256SUMS for {version}, so there is nothing \
         to check it against and it was not used"
    )]
    NotListed {
        /// The archive that was downloaded.
        asset: String,
        /// The release it came from.
        version: String,
    },
    /// The archive is not the one the release published.
    #[error(
        "{asset} does not match the checksum {version} publishes for it \
         (expected {expected}, got {actual}); it was discarded"
    )]
    Corrupt {
        /// The archive that was downloaded.
        asset: String,
        /// The release it came from.
        version: String,
        /// What `SHA256SUMS` says.
        expected: String,
        /// What arrived.
        actual: String,
    },
    /// The archive does not hold the binary it should.
    #[error("{asset} does not contain {member}: {detail}")]
    NoBinary {
        /// The archive that was downloaded.
        asset: String,
        /// The path that was looked for inside it.
        member: String,
        /// What went wrong.
        detail: String,
    },
    /// The extracted binary could not be written where the uploader can read it.
    #[error("could not write the downloaded binary to {path}: {source}")]
    Unwritable {
        /// Where it was going.
        path: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
}

/// The release target triple for a platform, if one is published.
///
/// Only the combinations the release matrix actually builds. The Windows
/// triples are absent on purpose: the installer talks to the remote with
/// `uname`, `mkdir`, `dd`, `chmod` and `mv`, so a remote it can reach is a
/// POSIX one and a `.zip` is never the file to fetch.
pub fn target_for(platform: &Platform) -> Result<&'static str, FetchError> {
    match (platform.os.as_str(), platform.arch.as_str()) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => Err(FetchError::NoBuildFor {
            platform: platform.name(),
        }),
    }
}

/// The archive a release publishes for `target`.
pub fn asset_for(target: &str) -> String {
    format!("deco-{target}.tar.gz")
}

/// The path inside that archive holding the binary.
///
/// `cargo xtask dist` stages under a directory named for the target, so the
/// member is predictable rather than something to search for.
pub fn member_for(target: &str) -> String {
    format!("deco-{target}/deco")
}

/// Where `asset` is downloaded from for `version`.
pub fn asset_url(version: &str, asset: &str) -> String {
    format!("{REPOSITORY}/releases/download/v{version}/{asset}")
}

/// Where the checksums for `version` are downloaded from.
pub fn checksums_url(version: &str) -> String {
    format!("{REPOSITORY}/releases/download/v{version}/SHA256SUMS")
}

/// The expected hash for `asset`, from the text of a `SHA256SUMS` file.
///
/// The format `sha256sum` writes and reads: a hash, two spaces, a name. Matched
/// on the whole name rather than a suffix, so `deco-x86_64-unknown-linux-gnu.tar.gz`
/// cannot be satisfied by a line for some other file ending the same way.
pub fn checksum_for<'a>(checksums: &'a str, asset: &str) -> Option<&'a str> {
    checksums.lines().find_map(|line| {
        let (hash, name) = line.split_once("  ")?;
        (name.trim() == asset).then_some(hash.trim())
    })
}

/// The SHA-256 of `bytes`, lowercase hex.
///
/// deco's own, for the reason in this module's header.
pub fn digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Refuses `bytes` unless they hash to what the release published.
///
/// Compared case-insensitively, because a hash is a number and `AB` and `ab`
/// are the same one — a comparison that said otherwise would reject a perfectly
/// good download for its spelling.
pub fn verify(bytes: &[u8], expected: &str, asset: &str, version: &str) -> Result<(), FetchError> {
    let actual = digest_of(bytes);
    if actual.eq_ignore_ascii_case(expected.trim()) {
        return Ok(());
    }
    Err(FetchError::Corrupt {
        asset: asset.to_owned(),
        version: version.to_owned(),
        expected: expected.trim().to_owned(),
        actual,
    })
}

/// Something that can retrieve a URL.
///
/// A trait so the tests never reach the network. Everything above it — which is
/// all of the deciding — runs against bytes a test chose.
pub trait Fetcher {
    /// The bytes at `url`, or why not.
    fn get(&mut self, url: &str) -> Result<Vec<u8>, String>;
}

/// A [`Fetcher`] that runs the machine's own `curl`.
///
/// `--fail` so an error page is an error rather than a 400-byte "binary";
/// `--location` because a release asset is served as a redirect; `--silent`
/// with `--show-error` so the only thing on stderr is the reason it did not
/// work.
#[derive(Debug, Default)]
pub struct Curl;

impl Fetcher for Curl {
    fn get(&mut self, url: &str) -> Result<Vec<u8>, String> {
        let output = std::process::Command::new("curl")
            .args(["--fail", "--location", "--silent", "--show-error", url])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        Ok(output.stdout)
    }
}

/// Whether `tool` can be found on `path`.
fn on_path(tool: &str, path: Option<&std::ffi::OsStr>) -> bool {
    let Some(path) = path else { return false };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(tool);
        candidate.is_file() || candidate.with_extension("exe").is_file()
    })
}

/// The binary inside `archive`, without unpacking the archive.
///
/// `tar xzO` writes the one member to standard output, so nothing from the
/// archive reaches the filesystem — a tree with a `..` in it or a symlink
/// pointing somewhere it should not has nowhere to land. The archive has already
/// been checked against the release's own checksum by the time this runs, so
/// what is being read here is byte-for-byte what the project published.
fn binary_from(archive: &Path, member: &str, asset: &str) -> Result<Vec<u8>, FetchError> {
    if !on_path("tar", std::env::var_os("PATH").as_deref()) {
        return Err(FetchError::NoTool {
            tool: "tar",
            purpose: "take the binary out of the downloaded archive",
        });
    }
    let output = std::process::Command::new("tar")
        .arg("xzOf")
        .arg(archive)
        .arg(member)
        .output()
        .map_err(|error| FetchError::NoBinary {
            asset: asset.to_owned(),
            member: member.to_owned(),
            detail: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(FetchError::NoBinary {
            asset: asset.to_owned(),
            member: member.to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    if output.stdout.is_empty() {
        return Err(FetchError::NoBinary {
            asset: asset.to_owned(),
            member: member.to_owned(),
            detail: "it is there and empty".to_owned(),
        });
    }
    Ok(output.stdout)
}

/// A deco built for `platform`, downloaded and checked, left at a local path.
///
/// The caller uploads it exactly as it uploads this machine's own binary — the
/// difference between the two is over by the time this returns.
pub fn binary_for(
    fetcher: &mut dyn Fetcher,
    platform: &Platform,
    version: &str,
    into: &Path,
) -> Result<PathBuf, FetchError> {
    let target = target_for(platform)?;
    let asset = asset_for(target);

    // The checksums first. Downloading the archive and *then* discovering there
    // is nothing to check it against would leave a decision to make about bytes
    // already in hand, and the only safe answer to that is the one taken here.
    let checksums_at = checksums_url(version);
    let checksums = fetcher
        .get(&checksums_at)
        .map_err(|detail| FetchError::Download {
            what: "the checksums",
            url: checksums_at,
            detail,
        })?;
    let checksums = String::from_utf8_lossy(&checksums);
    let Some(expected) = checksum_for(&checksums, &asset) else {
        return Err(FetchError::NotListed {
            asset,
            version: version.to_owned(),
        });
    };
    // Copied out so the borrow of `checksums` ends before `asset` is moved into
    // an error below.
    let expected = expected.to_owned();

    let archive_at = asset_url(version, &asset);
    let archive = fetcher
        .get(&archive_at)
        .map_err(|detail| FetchError::Download {
            what: "the release archive",
            url: archive_at,
            detail,
        })?;

    verify(&archive, &expected, &asset, version)?;

    // Written only after it has been checked. A file on disk is something
    // another process can pick up, and an unverified download that briefly looks
    // like a deco is worth not creating at all.
    let staged = into.join(&asset);
    if let Some(parent) = staged.parent() {
        std::fs::create_dir_all(parent).map_err(|source| FetchError::Unwritable {
            path: staged.clone(),
            source,
        })?;
    }
    std::fs::write(&staged, &archive).map_err(|source| FetchError::Unwritable {
        path: staged.clone(),
        source,
    })?;

    let member = member_for(target);
    let binary = binary_from(&staged, &member, &asset)?;
    let _ = std::fs::remove_file(&staged);

    let path = into.join("deco");
    std::fs::write(&path, &binary).map_err(|source| FetchError::Unwritable {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform(os: &str, arch: &str) -> Platform {
        Platform {
            os: os.to_owned(),
            arch: arch.to_owned(),
            home: "/home/u".to_owned(),
        }
    }

    #[test]
    fn every_published_posix_target_is_reachable_and_nothing_else_is() {
        assert_eq!(
            target_for(&platform("linux", "x86_64")).unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            target_for(&platform("linux", "aarch64")).unwrap(),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(
            target_for(&platform("macos", "aarch64")).unwrap(),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            target_for(&platform("macos", "x86_64")).unwrap(),
            "x86_64-apple-darwin"
        );

        // Refused by name rather than answered with the nearest thing.
        let error = target_for(&platform("freebsd", "x86_64")).expect_err("a refusal");
        assert!(error.to_string().contains("freebsd-x86_64"), "{error}");
        assert!(target_for(&platform("linux", "riscv64")).is_err());
        // Windows has published builds, but not ones this can reach: the
        // installer speaks POSIX to the remote, so it is never the answer here.
        assert!(target_for(&platform("windows", "x86_64")).is_err());
    }

    #[test]
    fn the_urls_are_the_ones_the_readme_tells_a_person_to_use() {
        // If these drift from the documented manual install, one of the two is
        // wrong and nobody finds out until a release.
        assert_eq!(
            asset_url("0.1.0", &asset_for("x86_64-unknown-linux-gnu")),
            "https://github.com/sabas0ba/deco/releases/download/v0.1.0/\
             deco-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            checksums_url("0.1.0"),
            "https://github.com/sabas0ba/deco/releases/download/v0.1.0/SHA256SUMS"
        );
        assert_eq!(
            member_for("aarch64-apple-darwin"),
            "deco-aarch64-apple-darwin/deco"
        );
    }

    const SUMS: &str = "\
4ce3b344a078603b7c907b935188ead640fd7506f913b06381c8dab6be788912  deco-x86_64-unknown-linux-musl.tar.gz
bc03aec5dc6d531fb826ba05569f4a7900604e35a2d27ee9c1308d4e9e19dfbc  deco-x86_64-unknown-linux-gnu.tar.gz
ca77f2fe1ecba8fe554b4a7108fa267b0a5c63ee02553883c003086c05ea6b16  SHA256SUMS
";

    #[test]
    fn a_checksum_is_found_by_its_whole_name_and_not_by_a_suffix() {
        assert_eq!(
            checksum_for(SUMS, "deco-x86_64-unknown-linux-gnu.tar.gz"),
            Some("bc03aec5dc6d531fb826ba05569f4a7900604e35a2d27ee9c1308d4e9e19dfbc")
        );
        // `-gnu` and `-musl` end alike enough that a suffix match would confuse
        // them, and the two are different binaries.
        assert_eq!(
            checksum_for(SUMS, "deco-x86_64-unknown-linux-musl.tar.gz"),
            Some("4ce3b344a078603b7c907b935188ead640fd7506f913b06381c8dab6be788912")
        );
        assert_eq!(checksum_for(SUMS, "deco-aarch64-apple-darwin.tar.gz"), None);
        assert_eq!(checksum_for(SUMS, "linux-gnu.tar.gz"), None);
    }

    #[test]
    fn the_digest_is_the_one_sha256sum_would_print() {
        // The empty string and "abc" are the values every SHA-256
        // implementation is checked against, so a broken wiring of the crate
        // shows up here rather than as a release that will not install.
        assert_eq!(
            digest_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn bytes_that_do_not_match_are_refused_and_the_message_names_both_hashes() {
        let error = verify(
            b"not the release",
            &digest_of(b"the release"),
            "a.tar.gz",
            "0.1.0",
        )
        .expect_err("a refusal");
        let said = error.to_string();
        assert!(said.contains("does not match"), "{said}");
        assert!(said.contains(&digest_of(b"the release")), "{said}");
        assert!(said.contains(&digest_of(b"not the release")), "{said}");
        assert!(said.contains("discarded"), "{said}");

        // A hash is a number; its spelling is not part of it.
        assert!(verify(b"abc", &digest_of(b"abc").to_uppercase(), "a", "0.1.0").is_ok());
        assert!(verify(b"abc", &format!(" {}\n", digest_of(b"abc")), "a", "0.1.0").is_ok());
    }

    /// A [`Fetcher`] answering from a table, so nothing here touches a network.
    struct Canned(Vec<(String, Vec<u8>)>);

    impl Fetcher for Canned {
        fn get(&mut self, url: &str) -> Result<Vec<u8>, String> {
            self.0
                .iter()
                .find(|(at, _)| at == url)
                .map(|(_, bytes)| bytes.clone())
                .ok_or_else(|| "404".to_owned())
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "deco-fetch-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a directory");
        root
    }

    /// A real `.tar.gz` holding one member, built by the machine's own `tar`.
    ///
    /// A fixture rather than a checked-in blob, so what is parsed is what this
    /// platform's `tar` actually writes.
    fn archive_holding(root: &Path, target: &str, contents: &[u8]) -> Vec<u8> {
        let stage = root.join(format!("deco-{target}"));
        std::fs::create_dir_all(&stage).expect("a directory");
        std::fs::write(stage.join("deco"), contents).expect("a file");
        let archive = root.join("made.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("czf")
            .arg(&archive)
            .arg("-C")
            .arg(root)
            .arg(format!("deco-{target}"))
            .status()
            .expect("tar runs");
        assert!(status.success());
        let bytes = std::fs::read(&archive).expect("the archive");
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_dir_all(&stage);
        bytes
    }

    #[test]
    fn a_good_download_ends_as_a_binary_on_disk() {
        let root = scratch("good");
        let target = "x86_64-unknown-linux-gnu";
        let asset = asset_for(target);
        let archive = archive_holding(&root, target, b"#!/bin/sh\necho deco 0.1.0\n");
        let sums = format!("{}  {asset}\n", digest_of(&archive));

        let mut fetcher = Canned(vec![
            (checksums_url("0.1.0"), sums.into_bytes()),
            (asset_url("0.1.0", &asset), archive),
        ]);
        let out = root.join("out");
        let path = binary_for(&mut fetcher, &platform("linux", "x86_64"), "0.1.0", &out)
            .expect("a binary");

        assert_eq!(
            std::fs::read(&path).expect("it"),
            b"#!/bin/sh\necho deco 0.1.0\n"
        );
        // The archive is not left behind next to it.
        assert!(!out.join(&asset).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_archive_that_is_not_what_the_release_published_is_discarded() {
        // The test this module exists for. The bytes arrive, they are wrong, and
        // nothing is written.
        let root = scratch("corrupt");
        let target = "x86_64-unknown-linux-gnu";
        let asset = asset_for(target);
        let archive = archive_holding(&root, target, b"the real one");
        let sums = format!("{}  {asset}\n", digest_of(b"something else entirely"));

        let mut fetcher = Canned(vec![
            (checksums_url("0.1.0"), sums.into_bytes()),
            (asset_url("0.1.0", &asset), archive),
        ]);
        let out = root.join("out");
        let error = binary_for(&mut fetcher, &platform("linux", "x86_64"), "0.1.0", &out)
            .expect_err("a refusal");
        assert!(matches!(error, FetchError::Corrupt { .. }), "{error}");
        assert!(
            !out.join("deco").exists(),
            "a rejected download was written"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_archive_nothing_vouches_for_is_refused_before_it_is_downloaded() {
        // No line for this asset means no way to check it, and the answer to
        // that is not "use it anyway". The archive is deliberately absent from
        // the table too: reaching for it would be a 404 rather than a refusal.
        let root = scratch("unlisted");
        let mut fetcher = Canned(vec![(
            checksums_url("0.1.0"),
            b"deadbeef  something-else.tar.gz\n".to_vec(),
        )]);
        let error = binary_for(
            &mut fetcher,
            &platform("linux", "x86_64"),
            "0.1.0",
            &root.join("out"),
        )
        .expect_err("a refusal");
        assert!(matches!(error, FetchError::NotListed { .. }), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_release_that_is_not_there_is_reported_with_the_url_that_was_tried() {
        let root = scratch("missing");
        let mut fetcher = Canned(Vec::new());
        let error = binary_for(
            &mut fetcher,
            &platform("linux", "x86_64"),
            "9.9.9",
            &root.join("out"),
        )
        .expect_err("a refusal");
        let said = error.to_string();
        assert!(said.contains("v9.9.9/SHA256SUMS"), "{said}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_archive_without_the_binary_in_it_says_which_path_was_looked_for() {
        let root = scratch("empty-archive");
        let target = "x86_64-unknown-linux-gnu";
        let asset = asset_for(target);
        // Built for a different target, so the member this looks for is absent.
        let archive = archive_holding(&root, "aarch64-apple-darwin", b"wrong tree");
        let sums = format!("{}  {asset}\n", digest_of(&archive));

        let mut fetcher = Canned(vec![
            (checksums_url("0.1.0"), sums.into_bytes()),
            (asset_url("0.1.0", &asset), archive),
        ]);
        let error = binary_for(
            &mut fetcher,
            &platform("linux", "x86_64"),
            "0.1.0",
            &root.join("out"),
        )
        .expect_err("a refusal");
        let said = error.to_string();
        assert!(
            said.contains("deco-x86_64-unknown-linux-gnu/deco"),
            "{said}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tool_is_looked_for_on_the_path_and_not_assumed() {
        let root = scratch("on-path");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("a directory");
        std::fs::write(bin.join("pretend-tool"), b"").expect("a file");

        let path = std::env::join_paths([bin.as_path()]).expect("a PATH");
        assert!(on_path("pretend-tool", Some(path.as_os_str())));
        assert!(!on_path("definitely-not-installed", Some(path.as_os_str())));
        assert!(!on_path("pretend-tool", None));
        let _ = std::fs::remove_dir_all(&root);
    }
}
