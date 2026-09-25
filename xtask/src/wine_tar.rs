//! The Windows `tar.exe` the Wine test pass needs.
//!
//! `deco-remote` unpacks a downloaded release with the platform's `tar` (see
//! `crates/deco-remote/src/fetch.rs`), and its tests create archives with it.
//! Windows 10 and later ship `tar.exe`, which is bsdtar from libarchive. Wine
//! does not, so under Wine those tests fail with "program not found".
//!
//! This module builds the same program from a pinned libarchive release with
//! MinGW and puts it where the Wine pass adds it to Wine's `PATH`. It builds
//! once per libarchive version; later runs reuse the result in `target/`.
//!
//! # Requirements
//!
//! `curl`, `cmake`, the MinGW toolchain (`mingw-w64`) and MinGW's zlib
//! (`libz-mingw-w64-dev` on Debian and Ubuntu). The source archive is checked
//! against [`SOURCE_SHA256`] before it is unpacked.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// The libarchive release that is built.
pub const LIBARCHIVE_VERSION: &str = "3.8.9";

/// SHA-256 of `libarchive-3.8.9.tar.gz` as published on the release page.
const SOURCE_SHA256: &str = "f5a6539059cf5e597dbeda37bfa4874b1e8dea063c8d93bf85a2b44af90a5bd4";

/// The C compiler of the MinGW toolchain `cross.rs` already requires.
const MINGW_CC: &str = "x86_64-w64-mingw32-gcc";

/// Where MinGW's headers and libraries are installed on Debian and Ubuntu.
const MINGW_SYSROOT: &str = "/usr/x86_64-w64-mingw32";

/// The directory that holds `tar.exe` once it is built.
pub fn bin_dir(root: &Path) -> PathBuf {
    work_dir(root).join("bin")
}

fn work_dir(root: &Path) -> PathBuf {
    root.join("target").join("wine-tar")
}

fn source_url() -> String {
    format!(
        "https://github.com/libarchive/libarchive/releases/download/v{LIBARCHIVE_VERSION}/\
         libarchive-{LIBARCHIVE_VERSION}.tar.gz"
    )
}

/// Builds `tar.exe` into [`bin_dir`] unless this version is already there.
pub fn ensure(root: &Path) -> Result<PathBuf> {
    let bin = bin_dir(root);
    let marker = bin.join("VERSION");
    if bin.join("tar.exe").is_file()
        && fs::read_to_string(&marker).is_ok_and(|text| text.trim() == LIBARCHIVE_VERSION)
    {
        return Ok(bin);
    }

    let work = work_dir(root);
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;

    let archive = work.join(format!("libarchive-{LIBARCHIVE_VERSION}.tar.gz"));
    crate::run(
        &work,
        "curl",
        &[
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--output",
            archive
                .to_str()
                .context("the work directory is not UTF-8")?,
            &source_url(),
        ],
        &[],
    )
    .context("downloading the libarchive source")?;

    let bytes = fs::read(&archive).with_context(|| format!("reading {}", archive.display()))?;
    let digest = hex(&Sha256::digest(&bytes));
    if digest != SOURCE_SHA256 {
        bail!(
            "{} has SHA-256 {digest}, not the pinned {SOURCE_SHA256}; refusing to build it",
            archive.display()
        );
    }

    let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    tar::Archive::new(decoder)
        .unpack(&work)
        .context("unpacking the libarchive source")?;
    let source = work.join(format!("libarchive-{LIBARCHIVE_VERSION}"));
    let build = work.join("build");

    let args = configure_args(&source, &build);
    crate::run(&work, "cmake", &borrow(&args), &[])
        .context("configuring libarchive — are `cmake` and `libz-mingw-w64-dev` installed?")?;
    crate::run(
        &work,
        "cmake",
        &[
            "--build",
            build.to_str().context("the build directory is not UTF-8")?,
            "--target",
            "bsdtar",
            "--parallel",
        ],
        &[],
    )
    .context("building bsdtar")?;

    fs::create_dir_all(&bin)?;
    fs::copy(build.join("bin").join("bsdtar.exe"), bin.join("tar.exe"))
        .context("copying bsdtar.exe")?;
    // Linked statically where the toolchain allows. MinGW's zlib may only be
    // available as a DLL, so it is copied beside the program when it exists.
    let zlib = Path::new(MINGW_SYSROOT).join("lib").join("zlib1.dll");
    if zlib.is_file() {
        fs::copy(&zlib, bin.join("zlib1.dll")).context("copying zlib1.dll")?;
    }
    fs::write(&marker, LIBARCHIVE_VERSION)?;
    Ok(bin)
}

/// The `cmake` configure arguments: bsdtar with gzip support and nothing else.
///
/// Every optional library is disabled so the build does not depend on what else
/// happens to be installed, and only zlib is needed.
fn configure_args(source: &Path, build: &Path) -> Vec<String> {
    let mut args = vec![
        "-S".to_owned(),
        source.display().to_string(),
        "-B".to_owned(),
        build.display().to_string(),
        "-DCMAKE_BUILD_TYPE=Release".to_owned(),
        "-DCMAKE_SYSTEM_NAME=Windows".to_owned(),
        format!("-DCMAKE_C_COMPILER={MINGW_CC}"),
        "-DCMAKE_RC_COMPILER=x86_64-w64-mingw32-windres".to_owned(),
        format!("-DCMAKE_FIND_ROOT_PATH={MINGW_SYSROOT}"),
        "-DCMAKE_FIND_ROOT_PATH_MODE_PROGRAM=NEVER".to_owned(),
        "-DCMAKE_FIND_ROOT_PATH_MODE_LIBRARY=ONLY".to_owned(),
        "-DCMAKE_FIND_ROOT_PATH_MODE_INCLUDE=ONLY".to_owned(),
        "-DCMAKE_EXE_LINKER_FLAGS=-static-libgcc".to_owned(),
        "-DENABLE_TAR=ON".to_owned(),
        "-DENABLE_TAR_SHARED=OFF".to_owned(),
        "-DENABLE_ZLIB=ON".to_owned(),
        "-DENABLE_TEST=OFF".to_owned(),
        "-DBUILD_TESTING=OFF".to_owned(),
        // A newer MinGW's warnings must not fail a pinned release's build.
        "-DENABLE_WERROR=OFF".to_owned(),
        "-DENABLE_INSTALL=OFF".to_owned(),
    ];
    for feature in [
        "CPIO",
        "CAT",
        "UNZIP",
        "OPENSSL",
        "MBEDTLS",
        "NETTLE",
        "CNG",
        "LIBXML2",
        "EXPAT",
        "LZMA",
        "LZO",
        "BZip2",
        "LZ4",
        "ZSTD",
        "LIBB2",
        "ICONV",
        "ACL",
        "XATTR",
        "PCREPOSIX",
        "PCRE2POSIX",
        "WIN32_XMLLITE",
    ] {
        args.push(format!("-DENABLE_{feature}=OFF"));
    }
    args
}

fn borrow(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `dir` as a Windows path Wine resolves: drive `Z:` maps to `/`.
pub fn wine_path(dir: &Path) -> String {
    format!("Z:{}", dir.display().to_string().replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_is_the_pinned_release() {
        assert_eq!(
            source_url(),
            "https://github.com/libarchive/libarchive/releases/download/v3.8.9/\
             libarchive-3.8.9.tar.gz"
        );
        assert_eq!(SOURCE_SHA256.len(), 64);
    }

    #[test]
    fn only_zlib_is_enabled_among_the_optional_libraries() {
        let args = configure_args(Path::new("/s"), Path::new("/b"));
        assert!(args.contains(&"-DENABLE_ZLIB=ON".to_owned()));
        for disabled in [
            "-DENABLE_OPENSSL=OFF",
            "-DENABLE_LZMA=OFF",
            "-DENABLE_ZSTD=OFF",
        ] {
            assert!(args.contains(&disabled.to_owned()), "{disabled}");
        }
    }

    #[test]
    fn a_unix_directory_becomes_a_z_drive_path() {
        assert_eq!(
            wine_path(Path::new("/home/runner/work/deco/target/wine-tar/bin")),
            r"Z:\home\runner\work\deco\target\wine-tar\bin"
        );
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xab]), "000fab");
    }
}
