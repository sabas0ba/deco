//! Building and packaging a release artifact.
//!
//! The naming, staging and checksum rules live in plain functions with unit
//! tests, so the parts that decide *what* goes into a release are verified on
//! every `cargo test` rather than only when someone pushes a tag.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Which archive format a target gets.
///
/// Chosen from the *target* triple rather than the host, so cross-building a
/// Windows artifact from Linux still produces a `.zip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `.tar.gz`
    TarGz,
    /// `.zip`
    Zip,
}

impl ArchiveKind {
    /// The format used for `target`.
    pub fn for_target(target: &str) -> Self {
        if target.contains("windows") {
            ArchiveKind::Zip
        } else {
            ArchiveKind::TarGz
        }
    }

    /// The file extension, without a leading dot.
    pub fn extension(self) -> &'static str {
        match self {
            ArchiveKind::TarGz => "tar.gz",
            ArchiveKind::Zip => "zip",
        }
    }
}

/// The directory name inside the archive, which is also the artifact stem.
pub fn stage_name(target: &str) -> String {
    format!("deco-{target}")
}

/// The archive's file name.
pub fn archive_name(target: &str) -> String {
    format!(
        "{}.{}",
        stage_name(target),
        ArchiveKind::for_target(target).extension()
    )
}

/// The name of the binary inside the archive.
pub fn binary_name(target: &str) -> String {
    if target.contains("windows") {
        "deco.exe".to_owned()
    } else {
        "deco".to_owned()
    }
}

/// Whether a path inside `extension-host` belongs in a release.
///
/// The host's own tests and any local `node_modules` are development-only;
/// shipping them would roughly double the archive for no benefit.
pub fn include_in_host(relative: &Path) -> bool {
    !relative.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("test" | "node_modules" | ".git")
        )
    })
}

/// One line of a `sha256sum`-format checksum file.
pub fn checksum_line(hash: &str, file_name: &str) -> String {
    // Two spaces: the format `sha256sum -c` expects for a binary-mode entry.
    format!("{hash}  {file_name}")
}

/// The SHA-256 of a file, lowercase hex.
pub fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Copies `from` into `to`, keeping only paths `filter` accepts.
pub fn copy_filtered(
    from: &Path,
    to: &Path,
    filter: &dyn Fn(&Path) -> bool,
    prefix: &Path,
) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let source = entry.path();
        let relative = prefix.join(entry.file_name());
        if !filter(&relative) {
            continue;
        }
        let destination = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_filtered(&source, &destination, filter, &relative)?;
        } else {
            fs::copy(&source, &destination)
                .with_context(|| format!("copying {}", source.display()))?;
        }
    }
    Ok(())
}

/// Where cargo puts the release binary for `target`.
pub fn built_binary(root: &Path, target: Option<&str>, binary: &str) -> PathBuf {
    let mut path = root.join("target");
    if let Some(target) = target {
        path = path.join(target);
    }
    path.join("release").join(binary)
}

/// Builds and packages a release artifact, returning the archive's path.
pub fn run(root: &Path, target: Option<&str>, out_dir: &Path, skip_build: bool) -> Result<PathBuf> {
    let target_triple = match target {
        Some(target) => target.to_owned(),
        None => host_target()?,
    };

    if !skip_build {
        let mut args = vec!["build", "--locked", "--release", "--package", "deco"];
        if let Some(target) = target {
            args.push("--target");
            args.push(target);
        }
        crate::run_cargo(root, &args)?;
    }

    let binary = binary_name(&target_triple);
    let built = built_binary(root, target, &binary);
    if !built.exists() {
        bail!(
            "expected a release binary at {}, but it is not there",
            built.display()
        );
    }

    let stage_root = out_dir.join(stage_name(&target_triple));
    if stage_root.exists() {
        fs::remove_dir_all(&stage_root)
            .with_context(|| format!("clearing {}", stage_root.display()))?;
    }
    fs::create_dir_all(&stage_root)
        .with_context(|| format!("creating {}", stage_root.display()))?;

    fs::copy(&built, stage_root.join(&binary))
        .with_context(|| format!("staging {}", built.display()))?;
    for file in ["README.md", "LICENSE"] {
        fs::copy(root.join(file), stage_root.join(file))
            .with_context(|| format!("staging {file}"))?;
    }
    // Extensions need the host at runtime, so it ships alongside the binary.
    copy_filtered(
        &root.join("extension-host"),
        &stage_root.join("extension-host"),
        &include_in_host,
        Path::new(""),
    )
    .context("staging the extension host")?;

    let archive = out_dir.join(archive_name(&target_triple));
    match ArchiveKind::for_target(&target_triple) {
        ArchiveKind::TarGz => write_tar_gz(&stage_root, &stage_name(&target_triple), &archive)?,
        ArchiveKind::Zip => write_zip(&stage_root, &stage_name(&target_triple), &archive)?,
    }

    let hash = hash_file(&archive)?;
    let checksum_path = out_dir.join(format!("{}.sha256", archive_name(&target_triple)));
    fs::write(
        &checksum_path,
        format!("{}\n", checksum_line(&hash, &archive_name(&target_triple))),
    )
    .with_context(|| format!("writing {}", checksum_path.display()))?;

    Ok(archive)
}

fn write_tar_gz(stage_root: &Path, inner_name: &str, archive: &Path) -> Result<()> {
    let file =
        fs::File::create(archive).with_context(|| format!("creating {}", archive.display()))?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder
        .append_dir_all(inner_name, stage_root)
        .with_context(|| format!("adding {} to the archive", stage_root.display()))?;
    builder.into_inner()?.finish()?;
    Ok(())
}

fn write_zip(stage_root: &Path, inner_name: &str, archive: &Path) -> Result<()> {
    let file =
        fs::File::create(archive).with_context(|| format!("creating {}", archive.display()))?;
    let mut writer = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut stack = vec![(stage_root.to_path_buf(), PathBuf::from(inner_name))];
    while let Some((directory, relative)) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let inner = relative.join(entry.file_name());
            let name = inner.to_string_lossy().replace('\\', "/");
            if entry.file_type()?.is_dir() {
                writer.add_directory(format!("{name}/"), options)?;
                stack.push((entry.path(), inner));
            } else {
                writer.start_file(name, options)?;
                writer.write_all(&fs::read(entry.path())?)?;
            }
        }
    }
    writer.finish()?;
    Ok(())
}

/// Merges every `*.sha256` in `dir` into one `SHA256SUMS`, removing the parts.
pub fn combine_checksums(dir: &Path) -> Result<PathBuf> {
    let mut lines: Vec<String> = Vec::new();
    let mut parts: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sha256") {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        lines.extend(
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_owned),
        );
        parts.push(path);
    }
    // Sorted so the file is stable whatever order the artifacts downloaded in.
    lines.sort();

    let combined = dir.join("SHA256SUMS");
    fs::write(&combined, format!("{}\n", lines.join("\n")))
        .with_context(|| format!("writing {}", combined.display()))?;
    for part in parts {
        fs::remove_file(part)?;
    }
    Ok(combined)
}

/// The host's target triple, read from `rustc -vV`.
pub fn host_target() -> Result<String> {
    let output = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .context("running rustc")?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_host_triple(&text).context("could not find the host triple in `rustc -vV` output")
}

/// Extracts the `host:` line from `rustc -vV` output.
pub fn parse_host_triple(rustc_version_verbose: &str) -> Option<String> {
    rustc_version_verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|host| host.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_targets_get_a_zip_and_everything_else_a_tarball() {
        assert_eq!(
            ArchiveKind::for_target("x86_64-pc-windows-msvc"),
            ArchiveKind::Zip
        );
        assert_eq!(
            ArchiveKind::for_target("aarch64-pc-windows-msvc"),
            ArchiveKind::Zip
        );
        assert_eq!(
            ArchiveKind::for_target("x86_64-unknown-linux-gnu"),
            ArchiveKind::TarGz
        );
        assert_eq!(
            ArchiveKind::for_target("aarch64-apple-darwin"),
            ArchiveKind::TarGz
        );
        assert_eq!(
            ArchiveKind::for_target("x86_64-unknown-linux-musl"),
            ArchiveKind::TarGz
        );
    }

    #[test]
    fn archive_names_carry_the_target_and_the_right_extension() {
        assert_eq!(
            archive_name("x86_64-unknown-linux-gnu"),
            "deco-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            archive_name("x86_64-pc-windows-msvc"),
            "deco-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn the_binary_gets_an_exe_suffix_only_on_windows() {
        assert_eq!(binary_name("x86_64-pc-windows-msvc"), "deco.exe");
        assert_eq!(binary_name("x86_64-unknown-linux-gnu"), "deco");
    }

    #[test]
    fn development_only_host_files_are_left_out() {
        assert!(include_in_host(Path::new("package.json")));
        assert!(include_in_host(Path::new("src/bootstrap.js")));
        assert!(!include_in_host(Path::new("test/rpc.test.js")));
        assert!(!include_in_host(Path::new("node_modules/x/index.js")));
        assert!(!include_in_host(Path::new("src/node_modules/x.js")));
    }

    #[test]
    fn checksum_lines_are_in_sha256sum_format() {
        // `sha256sum -c` requires exactly two spaces between hash and name.
        assert_eq!(
            checksum_line("abc123", "deco.tar.gz"),
            "abc123  deco.tar.gz"
        );
    }

    #[test]
    fn the_host_triple_is_read_from_rustc() {
        let output = "rustc 1.82.0 (f6e511eec 2024-10-15)\nbinary: rustc\nhost: x86_64-unknown-linux-gnu\nrelease: 1.82.0\n";
        assert_eq!(
            parse_host_triple(output).as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(parse_host_triple("no host line here"), None);
    }

    #[test]
    fn the_built_binary_path_accounts_for_cross_compilation() {
        let root = Path::new("/repo");
        assert_eq!(
            built_binary(root, None, "deco"),
            Path::new("/repo/target/release/deco")
        );
        assert_eq!(
            built_binary(root, Some("aarch64-apple-darwin"), "deco"),
            Path::new("/repo/target/aarch64-apple-darwin/release/deco")
        );
    }

    #[test]
    fn combining_checksums_sorts_and_removes_the_parts() {
        let dir = std::env::temp_dir().join(format!("deco-sums-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b.tar.gz.sha256"), "bbb  b.tar.gz\n").unwrap();
        fs::write(dir.join("a.zip.sha256"), "aaa  a.zip\n").unwrap();
        fs::write(dir.join("not-a-checksum.txt"), "ignored").unwrap();

        let combined = combine_checksums(&dir).unwrap();
        assert_eq!(
            fs::read_to_string(&combined).unwrap(),
            "aaa  a.zip\nbbb  b.tar.gz\n"
        );
        assert!(!dir.join("a.zip.sha256").exists());
        assert!(dir.join("not-a-checksum.txt").exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hashing_a_file_gives_the_known_sha256() {
        let path = std::env::temp_dir().join(format!("deco-hash-{}", std::process::id()));
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            hash_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn copying_applies_the_filter_recursively() {
        let root = std::env::temp_dir().join(format!("deco-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (from, to) = (root.join("from"), root.join("to"));
        fs::create_dir_all(from.join("src")).unwrap();
        fs::create_dir_all(from.join("test")).unwrap();
        fs::write(from.join("package.json"), "{}").unwrap();
        fs::write(from.join("src/a.js"), "a").unwrap();
        fs::write(from.join("test/a.test.js"), "t").unwrap();

        copy_filtered(&from, &to, &include_in_host, Path::new("")).unwrap();
        assert!(to.join("package.json").exists());
        assert!(to.join("src/a.js").exists());
        assert!(
            !to.join("test").exists(),
            "development-only files were copied"
        );

        fs::remove_dir_all(&root).ok();
    }
}
