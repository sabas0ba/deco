//! Repository tooling.
//!
//! Everything CI does is a subcommand here, so a contributor can reproduce any
//! CI step locally with the same command CI runs. The workflows stay short
//! enough to read, and the logic that decides what a release contains is
//! ordinary Rust with unit tests rather than inline YAML shell that can only be
//! exercised by pushing a tag.
//!
//! ```console
//! $ cargo xtask ci                                  # everything the CI checks run
//! $ cargo xtask cross                               # the Windows and macOS targets, from Linux
//! $ cargo xtask dist                                # package for this machine
//! $ cargo xtask dist --target aarch64-apple-darwin  # …or for another target
//! $ cargo xtask docs                                # regenerate the docs images
//! ```

mod commitlint;
mod cross;
mod dist;
mod docs;
mod release;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

/// deco's repository tooling.
#[derive(Debug, Parser)]
#[command(name = "xtask", about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the checks CI runs: formatting, lints, docs and tests.
    Ci {
        /// Only check formatting and lints.
        #[arg(long)]
        lint_only: bool,
        /// Only run the tests.
        #[arg(long)]
        test_only: bool,
    },
    /// Exercise the Windows and macOS targets from a Linux host.
    ///
    /// Type-checks every triple the release matrix ships, then builds the
    /// tests for the Windows target and runs them under Wine. Needs
    /// `mingw-w64` and `wine64` for the second half; see `xtask/src/cross.rs`
    /// for what this does and does not stand in for.
    Cross {
        /// Only type-check, skipping the Wine run.
        #[arg(long)]
        check_only: bool,
        /// Only run the tests under Wine.
        #[arg(long)]
        wine_only: bool,
    },
    /// Build and package a release artifact.
    Dist {
        /// Target triple to package for. Defaults to this machine's.
        #[arg(long)]
        target: Option<String>,
        /// Where to write the archive.
        #[arg(long, default_value = "dist")]
        out: PathBuf,
        /// Package an already-built binary instead of building one.
        #[arg(long)]
        skip_build: bool,
    },
    /// Merge the per-artifact `.sha256` files into one `SHA256SUMS`.
    Checksums {
        /// Directory holding the downloaded artifacts.
        #[arg(long, default_value = "dist")]
        dir: PathBuf,
    },
    /// Write a tag's release notes, taken from `CHANGELOG.md`.
    ReleaseNotes {
        /// The tag being released, e.g. `v0.1.0`.
        #[arg(long)]
        tag: String,
        /// Where to write the notes.
        ///
        /// Deliberately not under `dist`: the release uploads `dist/*` as
        /// assets, and the notes are the release's text rather than a file to
        /// download.
        #[arg(long, default_value = "target/release-notes.md")]
        out: PathBuf,
    },
    /// Run the extension host's own test suite.
    HostTest,
    /// Check the dependency graph against the supply-chain policy in deny.toml.
    Deny,
    /// Regenerate the animated demonstrations in `docs/img`.
    Docs {
        /// Check that the committed files match what the code renders, instead
        /// of rewriting them.
        #[arg(long)]
        check: bool,
    },
    /// Check commit messages against Conventional Commits.
    Commitlint {
        /// Revision range to check, e.g. `origin/main..HEAD`.
        ///
        /// Defaults to the commits this branch adds to `origin/main`, which is
        /// what a pull request is asking to merge.
        #[arg(long)]
        range: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = repository_root()?;

    match cli.command {
        Command::Ci {
            lint_only,
            test_only,
        } => ci(&root, lint_only, test_only),
        Command::Cross {
            check_only,
            wine_only,
        } => cross::run(&root, check_only, wine_only),
        Command::Dist {
            target,
            out,
            skip_build,
        } => {
            let out = absolute(&root, &out);
            std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
            let archive = dist::run(&root, target.as_deref(), &out, skip_build)?;
            println!("{}", archive.display());
            Ok(())
        }
        Command::Checksums { dir } => {
            let combined = dist::combine_checksums(&absolute(&root, &dir))?;
            println!("{}", combined.display());
            Ok(())
        }
        Command::ReleaseNotes { tag, out } => release::run(&root, &tag, &absolute(&root, &out)),
        Command::HostTest => host_test(&root),
        Command::Deny => deny(&root),
        Command::Docs { check } => {
            let written = docs::run(&root, check)?;
            if check {
                println!("{} demonstrations are up to date", written.len());
            } else {
                for path in &written {
                    println!("{}", path.display());
                }
            }
            Ok(())
        }
        Command::Commitlint { range } => commit_lint(&root, range.as_deref()),
    }
}

/// Checks every commit in `range` against Conventional Commits.
///
/// Only the commits a branch adds, not the whole history: the convention was
/// adopted partway through, and rewriting merged commits to satisfy it would
/// change history other people have already pulled.
fn commit_lint(root: &Path, range: Option<&str>) -> Result<()> {
    let range = range.unwrap_or("origin/main..HEAD").to_owned();

    // NUL-separated, because a commit message contains blank lines and
    // everything else a line-based split would trip over.
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(["log", "--no-merges", "--format=%B%x00", &range])
        .output()
        .context("running `git log` — is this a git checkout?")?;

    if !output.status.success() {
        anyhow::bail!(
            "`git log {range}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let messages: Vec<&str> = text
        .split('\0')
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .collect();

    if messages.is_empty() {
        println!("no commits in {range} to check");
        return Ok(());
    }

    let mut failed = 0usize;
    for message in &messages {
        let subject = message.lines().next().unwrap_or_default();
        let problems = commitlint::check(message);
        if problems.is_empty() {
            println!("ok   {subject}");
            continue;
        }
        failed += 1;
        println!("FAIL {subject}");
        for problem in problems {
            println!(
                "     [{}] {}",
                problem.rule,
                problem.detail.replace('\n', "\n     ")
            );
        }
    }

    if failed > 0 {
        anyhow::bail!(
            "{failed} of {} commit messages are not Conventional Commits.              Rewrite them with `git rebase -i` or `git commit --amend`.",
            messages.len()
        );
    }
    println!("{} commit messages are well formed", messages.len());
    Ok(())
}

/// Runs `cargo deny` against deny.toml.
///
/// Separate from `ci` because it needs a tool that is not part of a default
/// Rust installation, and because a newly published advisory can turn it red
/// without anything in this repository changing.
fn deny(root: &Path) -> Result<()> {
    run(root, "cargo", &["deny", "--all-features", "check"], &[])
        .context("`cargo deny` failed — install it with `cargo install cargo-deny --locked`")
}

/// Runs the checks, in the order that fails fastest.
fn ci(root: &Path, lint_only: bool, test_only: bool) -> Result<()> {
    if !test_only {
        run_cargo(root, &["fmt", "--all", "--", "--check"])?;
        // --all-features so the GPU frontend is linted too; it is behind a
        // feature flag and would otherwise never be checked.
        run_cargo(
            root,
            &[
                "clippy",
                "--locked",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ],
        )?;
        run_with_env(
            root,
            "cargo",
            &[
                "doc",
                "--locked",
                "--workspace",
                "--no-deps",
                "--all-features",
            ],
            &[("RUSTDOCFLAGS", "-D warnings")],
        )?;
    }
    if !lint_only {
        run_cargo(root, &["test", "--locked", "--workspace", "--all-features"])?;
    }
    Ok(())
}

/// Runs the Node extension host's tests.
///
/// Delegates to `npm test` rather than restating the invocation: the script in
/// `extension-host/package.json` is the single definition of how those tests
/// run, and CI calls the same one.
fn host_test(root: &Path) -> Result<()> {
    check_node_version()?;
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    run(&root.join("extension-host"), npm, &["test"], &[])?;
    // The other half: the Rust side against the real host. Ignored by default so
    // `cargo test` stays runnable where there is no Node — under Wine, for one — and
    // run here, which is the command that already requires it.
    run_cargo(
        root,
        &[
            "test",
            "--locked",
            "-p",
            "deco-ext",
            "--test",
            "host_round_trip",
            "--",
            "--ignored",
            // The container round trip is selected separately below, because it
            // needs something this one does not.
            "--skip",
            CONTAINER_TESTS,
        ],
    )?;

    // The editor's half: a directory becoming a palette entry, and the command
    // being run. Needs Node for the same reason and nothing more.
    run_cargo(
        root,
        &[
            "test",
            "--locked",
            "-p",
            "deco-tui",
            "--test",
            "extension_commands",
            "--",
            "--ignored",
        ],
    )?;

    // The same stack again, with the files on the other end of a connection: an
    // extension's read has to go through the server rather than around it, and
    // only a real host asking a real server can show which one happened.
    run_cargo(
        root,
        &[
            "test",
            "--locked",
            "-p",
            "deco",
            "--test",
            "remote_extension",
            "--",
            "--ignored",
            // One at a time: each scenario starts its own host and its own
            // server, and the bootstrap path they need is process-wide.
            "--test-threads=1",
        ],
    )?;

    // And the same stack in the container deco actually ships with. This needs a
    // container runtime, which not every machine has — so the decision is
    // *printed* either way. A test that quietly does not run is worse than one
    // that is not written, because the output looks the same as passing.
    match deco_ext::sandbox::find_runtime(
        &deco_ext::sandbox::RUNTIMES,
        std::env::var_os("PATH").as_deref(),
    ) {
        Some(runtime) => {
            println!("container round trip: using {}", runtime.display());
            run_cargo(
                root,
                &[
                    "test",
                    "--locked",
                    "-p",
                    "deco-ext",
                    "--test",
                    "host_round_trip",
                    "--",
                    "--ignored",
                    CONTAINER_TESTS,
                ],
            )
        }
        None => {
            println!(
                "container round trip: SKIPPED — none of {} is on the PATH, so the \
                 default sandbox cannot be exercised here",
                deco_ext::sandbox::RUNTIMES.join(" or ")
            );
            Ok(())
        }
    }
}

/// The name fragment selecting the round-trip tests that need a container.
const CONTAINER_TESTS: &str = "a_container";

/// The oldest Node the host runs on, as `(major, minor)`.
///
/// `--permission` became the flag's stable spelling in 22.13, and the host passes
/// that spelling. An older Node rejects it and exits before printing anything about
/// deco, so the version is worth stating before the tests rather than after.
const OLDEST_NODE: (u64, u64) = (22, 13);

/// Refuses to run the host tests on a Node too old to accept `--permission`.
fn check_node_version() -> Result<()> {
    let node = if cfg!(windows) { "node.exe" } else { "node" };
    let output = std::process::Command::new(node)
        .arg("--version")
        .output()
        .with_context(|| format!("running `{node} --version` — is Node installed?"))?;
    let said = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let version = parse_node_version(&said)
        .with_context(|| format!("`{node} --version` said {said:?}, which is not a version"))?;
    anyhow::ensure!(
        version >= OLDEST_NODE,
        "the extension host needs Node {}.{} or newer for `--permission`, but this is {said}",
        OLDEST_NODE.0,
        OLDEST_NODE.1
    );
    Ok(())
}

/// Reads `v22.13.0` as `(22, 13)`.
fn parse_node_version(said: &str) -> Option<(u64, u64)> {
    let mut parts = said.trim_start_matches('v').split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Runs `cargo` with `args` in `root`.
pub fn run_cargo(root: &Path, args: &[&str]) -> Result<()> {
    run(root, "cargo", args, &[])
}

fn run_with_env(root: &Path, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<()> {
    run(root, program, args, env)
}

fn run(dir: &Path, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<()> {
    eprintln!("$ {program} {}", args.join(" "));
    let mut command = std::process::Command::new(program);
    command.args(args).current_dir(dir);
    for (key, value) in env {
        command.env(key, value);
    }
    let status = command
        .status()
        .with_context(|| format!("could not run `{program}` — is it installed?"))?;
    if !status.success() {
        bail!("`{program} {}` failed with {status}", args.join(" "));
    }
    Ok(())
}

/// Resolves `path` against `root` unless it is already absolute.
fn absolute(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// The repository root, derived from this crate's location rather than the
/// working directory, so `cargo xtask` works from any subdirectory.
fn repository_root() -> Result<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .context("could not determine the repository root")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn dist_defaults_to_the_dist_directory_and_this_machine() {
        let cli = Cli::parse_from(["xtask", "dist"]);
        match cli.command {
            Command::Dist {
                target,
                out,
                skip_build,
            } => {
                assert_eq!(target, None);
                assert_eq!(out, PathBuf::from("dist"));
                assert!(!skip_build);
            }
            other => panic!("expected dist, got {other:?}"),
        }
    }

    #[test]
    fn dist_accepts_a_target() {
        let cli = Cli::parse_from(["xtask", "dist", "--target", "aarch64-apple-darwin"]);
        match cli.command {
            Command::Dist { target, .. } => {
                assert_eq!(target.as_deref(), Some("aarch64-apple-darwin"))
            }
            other => panic!("expected dist, got {other:?}"),
        }
    }

    #[test]
    fn ci_can_be_narrowed_to_lints_or_tests() {
        match Cli::parse_from(["xtask", "ci", "--lint-only"]).command {
            Command::Ci {
                lint_only,
                test_only,
            } => assert!(lint_only && !test_only),
            other => panic!("expected ci, got {other:?}"),
        }
        match Cli::parse_from(["xtask", "ci", "--test-only"]).command {
            Command::Ci {
                lint_only,
                test_only,
            } => assert!(!lint_only && test_only),
            other => panic!("expected ci, got {other:?}"),
        }
    }

    #[test]
    fn cross_can_be_narrowed_to_either_half() {
        match Cli::parse_from(["xtask", "cross"]).command {
            Command::Cross {
                check_only,
                wine_only,
            } => assert!(!check_only && !wine_only),
            other => panic!("expected cross, got {other:?}"),
        }
        match Cli::parse_from(["xtask", "cross", "--check-only"]).command {
            Command::Cross {
                check_only,
                wine_only,
            } => assert!(check_only && !wine_only),
            other => panic!("expected cross, got {other:?}"),
        }
        match Cli::parse_from(["xtask", "cross", "--wine-only"]).command {
            Command::Cross {
                check_only,
                wine_only,
            } => assert!(!check_only && wine_only),
            other => panic!("expected cross, got {other:?}"),
        }
    }

    #[test]
    fn release_notes_needs_a_tag_and_writes_beside_the_artifacts() {
        // The tag is required rather than defaulted to the version in Cargo.toml:
        // the workflow knows which tag it is building, and guessing would let a
        // mistyped tag publish the wrong version's notes without complaint.
        assert!(Cli::try_parse_from(["xtask", "release-notes"]).is_err());
        match Cli::parse_from(["xtask", "release-notes", "--tag", "v0.1.0"]).command {
            Command::ReleaseNotes { tag, out } => {
                assert_eq!(tag, "v0.1.0");
                // Not under `dist`, which is uploaded wholesale as assets.
                assert_eq!(out, PathBuf::from("target/release-notes.md"));
            }
            other => panic!("expected release-notes, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["xtask", "deploy-to-production"]).is_err());
    }

    #[test]
    fn relative_output_paths_resolve_against_the_repository_root() {
        let root = Path::new("/repo");
        assert_eq!(
            absolute(root, Path::new("dist")),
            PathBuf::from("/repo/dist")
        );
        assert_eq!(
            absolute(root, Path::new("/tmp/out")),
            PathBuf::from("/tmp/out")
        );
    }

    #[test]
    fn node_versions_are_read_and_compared_by_number_and_not_by_text() {
        assert_eq!(parse_node_version("v22.13.0"), Some((22, 13)));
        assert_eq!(parse_node_version("22.13.1"), Some((22, 13)));
        assert_eq!(parse_node_version("v24.0.0"), Some((24, 0)));
        assert_eq!(parse_node_version(""), None);
        assert_eq!(parse_node_version("v22"), None);
        assert_eq!(parse_node_version("not a version"), None);
        // Text order would put 22.9 above 22.13, which is the mistake this guards.
        assert!(parse_node_version("v22.9.0").unwrap() < OLDEST_NODE);
        assert!(parse_node_version("v20.20.2").unwrap() < OLDEST_NODE);
        assert!(parse_node_version("v22.13.0").unwrap() >= OLDEST_NODE);
        assert!(parse_node_version("v24.2.0").unwrap() >= OLDEST_NODE);
    }

    #[test]
    fn the_oldest_node_matches_what_the_host_package_declares() {
        // Two places have to agree about the runtime: this check and the manifest npm
        // reads. Stating it once is not possible — npm will not read a Rust constant —
        // so the next best thing is a test that notices when they drift apart.
        let manifest = std::fs::read_to_string(
            repository_root()
                .unwrap()
                .join("extension-host/package.json"),
        )
        .expect("the host manifest");
        let wanted = format!("\">={}.{}.0\"", OLDEST_NODE.0, OLDEST_NODE.1);
        assert!(
            manifest.contains(&wanted),
            "extension-host/package.json should require node {wanted}"
        );
    }

    #[test]
    fn the_repository_root_holds_the_workspace_manifest() {
        let root = repository_root().unwrap();
        assert!(
            root.join("Cargo.toml").exists(),
            "{} has no Cargo.toml",
            root.display()
        );
        assert!(root.join("crates").is_dir());
    }
}
