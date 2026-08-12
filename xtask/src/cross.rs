//! Exercising the Windows and macOS targets from a Linux host.
//!
//! The premium runners are held back to a release tag and to a deliberate
//! request (see the header of `.github/workflows/ci.yml`), which would leave
//! every other push with no signal at all about the platforms most of deco's
//! users are on. Two things a Linux runner can do give most of it back:
//!
//! * **A type check per shipped triple.** `cargo check` stops before the link
//!   step, so it needs no MSVC toolchain and no Apple SDK — only the target's
//!   prebuilt `std`, which rustup hands out for all four. What it catches is
//!   what a `#[cfg]` hides: the branch of `paths.rs` that picks `%APPDATA%`,
//!   the branch of `binding.rs` that maps `cmd` instead of `ctrl`, and the
//!   frontend's per-platform windowing code, none of which a Linux build
//!   compiles at all.
//!
//! * **The tests, run under Wine.** Built for `x86_64-pc-windows-gnu` with
//!   MinGW and executed through Wine by way of cargo's target runner. These
//!   are real Windows binaries running Windows code paths — the tests that
//!   spawn a child process included — bar the two in [`WINE_SKIPS`], which
//!   need a console Wine has not been given.
//!
//! What neither covers, and what the tagged run on real runners is therefore
//! still for:
//!
//! * **macOS, at runtime.** There is no Wine for Darwin. The macOS half is a
//!   compile check and nothing more.
//! * **The MSVC ABI.** Wine runs the GNU target; a mismatch that is specific to
//!   the linker or the C runtime MSVC uses will not show up here.
//! * **Wine's own fidelity.** It reimplements Win32; where it differs from
//!   Windows, a test can pass here and fail there — or the reverse.
//! * **The GPU frontend and the real console.** `deco-gui` is excluded because
//!   wgpu and winit want an adapter and a compositor, neither of which a
//!   headless Wine has; [`WINE_SKIPS`] is the same shortage one layer down.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The triples the release matrix ships that no Linux runner can link.
///
/// Kept in step with the matrices in `ci.yml` and `release.yml`: a target that
/// is shipped without being checked here is a target whose only build is the
/// one that runs when a tag is already pushed.
pub const CHECK_TARGETS: &[&str] = &[
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

/// The triple the Wine pass builds and runs.
///
/// GNU rather than MSVC because that is the one a Linux host can link: MinGW is
/// an apt package, whereas the MSVC target needs Microsoft's linker and import
/// libraries. The ABI differs, but `#[cfg(windows)]` does not — which is the
/// code this is here to run.
pub const WINE_TARGET: &str = "x86_64-pc-windows-gnu";

/// The linker MinGW installs for [`WINE_TARGET`].
pub const MINGW_LINKER: &str = "x86_64-w64-mingw32-gcc";

/// Crates the Wine pass leaves out.
///
/// `deco-gui` wants a GPU adapter and a compositor. `xtask` is host tooling
/// that shells out to git and npm and asserts on this repository's layout;
/// running it as a Windows binary under Wine would test the harness rather than
/// the editor.
pub const WINE_EXCLUDES: &[&str] = &["deco-gui", "xtask"];

/// `CARGO_TARGET_<TRIPLE>_<SUFFIX>`, the per-target configuration environment
/// variable cargo reads.
///
/// The triple is uppercased with its dashes turned into underscores, so
/// `x86_64-pc-windows-gnu` and `RUNNER` give
/// `CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER`.
pub fn target_env_var(target: &str, suffix: &str) -> String {
    let triple = target.replace('-', "_").to_uppercase();
    format!("CARGO_TARGET_{triple}_{suffix}")
}

/// The entries of `wanted` that `rustup target list --installed` did not list.
///
/// Reported all at once rather than one failed build at a time: the fix is a
/// single `rustup target add` and there is no reason to make someone discover
/// the arguments to it four builds in a row.
pub fn missing_targets(installed: &str, wanted: &[&str]) -> Vec<String> {
    let present: Vec<&str> = installed
        .lines()
        .map(str::trim)
        // `rustup target list --installed` prints bare triples, but the
        // unfiltered `list` marks them ` (installed)`; tolerate both rather
        // than depending on which one the caller ran.
        .map(|line| line.split_whitespace().next().unwrap_or_default())
        .filter(|line| !line.is_empty())
        .collect();
    wanted
        .iter()
        .filter(|target| !present.contains(*target))
        .map(|target| (*target).to_owned())
        .collect()
}

/// The `cargo check` invocation for one target.
///
/// `--all-features` so the GPU frontend is checked too: it is the code with the
/// most per-platform surface, and it is behind a feature flag, so a default
/// build would skip exactly the part worth checking.
pub fn check_args(target: &str) -> Vec<String> {
    ["check", "--locked", "--workspace", "--all-features"]
        .iter()
        .map(|argument| (*argument).to_owned())
        .chain(["--target".to_owned(), target.to_owned()])
        .collect()
}

/// Tests the Wine pass cannot run, matched as substrings the way libtest's
/// `--skip` matches them.
///
/// Both of these paint a frame through crossterm. On Windows crossterm decides
/// once, at first use, whether the terminal understands ANSI; when it decides
/// not, every command it is given goes to the console API instead of to the
/// writer it was handed — so a test that paints into a `Vec<u8>` still needs
/// the process to own a console. A CI runner gives Wine no terminal to make one
/// out of, and the calls come back `Invalid handle`. A real Windows runner has
/// one and passes them, which is where they stay covered.
///
/// These are the only two tests that reach crossterm: everything else in
/// `deco-tui` compares rendered strings, which is why the frontend's own suite
/// is otherwise portable. A new test that paints will need adding here.
pub const WINE_SKIPS: &[&str] = &[
    "painting_writes_every_span_and_positions_the_cursor",
    "painting_a_frame_with_no_cursor_leaves_it_hidden",
];

/// The `cargo test` invocation the Wine pass runs.
///
/// Default features, unlike the check above: `--all-features` would turn on
/// `deco`'s `gui` feature and drag wgpu and winit into a build whose tests are
/// excluded anyway.
pub fn wine_test_args() -> Vec<String> {
    let mut args: Vec<String> = ["test", "--locked", "--workspace"]
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect();
    for crate_name in WINE_EXCLUDES {
        args.push("--exclude".to_owned());
        args.push((*crate_name).to_owned());
    }
    args.push("--target".to_owned());
    args.push(WINE_TARGET.to_owned());
    for skip in WINE_SKIPS {
        // After the `--`, so these reach every test harness rather than cargo.
        if !args.iter().any(|argument| argument == "--") {
            args.push("--".to_owned());
        }
        args.push("--skip".to_owned());
        args.push((*skip).to_owned());
    }
    args
}

/// The places Wine is looked for, in order.
///
/// `$WINE` first, so a Wine that was built or unpacked somewhere unusual can be
/// named without arguing with this list. `/usr/lib/wine/wine64` last because
/// Ubuntu's `wine64` package installs the binary there and puts nothing on
/// `PATH`: the `/usr/bin/wine` wrapper belongs to the `wine` package, which
/// pulls in the 32-bit stack and a second architecture's worth of apt.
pub fn wine_candidates(explicit: Option<&OsStr>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(PathBuf::from(path));
    }
    for name in ["wine64", "wine"] {
        if let Some(found) = which(name) {
            candidates.push(found);
        }
    }
    candidates.push(PathBuf::from("/usr/lib/wine/wine64"));
    candidates
}

/// The first candidate that exists.
pub fn find_wine(explicit: Option<&OsStr>) -> Option<PathBuf> {
    wine_candidates(explicit)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// `program` as found on `PATH`, if it is there.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// Runs the cross-platform checks.
///
/// Ordered cheapest first, like `cargo xtask ci`: the type checks need nothing
/// installed beyond rustup targets, so a failure that both passes would catch
/// is reported before anyone waits on a Wine build.
pub fn run(root: &Path, check_only: bool, wine_only: bool) -> Result<()> {
    if !wine_only {
        check(root)?;
    }
    if !check_only {
        wine(root)?;
    }
    Ok(())
}

/// Type-checks every triple in [`CHECK_TARGETS`].
fn check(root: &Path) -> Result<()> {
    ensure_targets(root, CHECK_TARGETS)?;
    for target in CHECK_TARGETS {
        let args = check_args(target);
        crate::run_cargo(root, &borrow(&args))?;
    }
    Ok(())
}

/// Builds the tests for [`WINE_TARGET`] and runs them under Wine.
fn wine(root: &Path) -> Result<()> {
    ensure_targets(root, &[WINE_TARGET])?;

    if which(MINGW_LINKER).is_none() {
        bail!(
            "`{MINGW_LINKER}` is not on PATH — the {WINE_TARGET} target needs the MinGW \
             toolchain.\nInstall it with `sudo apt-get install -y mingw-w64`, or run \
             `cargo xtask cross --check-only` to skip this pass."
        );
    }

    let wine = find_wine(std::env::var_os("WINE").as_deref()).context(
        "no Wine found — looked at $WINE, `wine64` and `wine` on PATH, and \
         /usr/lib/wine/wine64.\nInstall it with `sudo apt-get install -y wine64`, or run \
         `cargo xtask cross --check-only` to skip this pass.",
    )?;
    let wine = wine.to_str().context("the path to Wine is not UTF-8")?;

    // Wine narrates every unimplemented stub it hits, which for a test binary
    // is several screens of noise around the output that matters. Respect an
    // explicit setting, so `WINEDEBUG=+file cargo xtask cross` still works.
    let debug = std::env::var("WINEDEBUG").unwrap_or_else(|_| "-all".to_owned());

    let env = [
        (
            target_env_var(WINE_TARGET, "LINKER"),
            MINGW_LINKER.to_owned(),
        ),
        (target_env_var(WINE_TARGET, "RUNNER"), wine.to_owned()),
        ("WINEDEBUG".to_owned(), debug),
    ];
    let env: Vec<(&str, &str)> = env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();

    boot_wine(root, wine, &env);

    let args = wine_test_args();
    crate::run_with_env(root, "cargo", &borrow(&args), &env)
}

/// Builds the Wine prefix before any test binary asks for one.
///
/// The first `wine` in a fresh `$WINEPREFIX` spends about ten seconds creating
/// it — registry, drive mappings, services — and cargo will happily start the
/// next test binary in the middle of that. Tests then fail in ways that have
/// nothing to do with the code: a directory that is not there yet, a console
/// that does not exist. A CI runner is always the fresh-prefix case, which is
/// why this is not something a laptop notices.
///
/// Advisory: if `wineboot` fails, the run continues and the tests report
/// whatever is actually wrong, which is more useful than a failure here about
/// the setup for them.
fn boot_wine(root: &Path, wine: &str, env: &[(&str, &str)]) {
    if crate::run_with_env(root, wine, &["wineboot", "--init"], env).is_err() {
        eprintln!("warning: `wineboot --init` failed; continuing to the tests anyway");
    }
}

/// Fails with the `rustup` command that would fix it if any target is missing.
fn ensure_targets(root: &Path, wanted: &[&str]) -> Result<()> {
    let output = std::process::Command::new("rustup")
        .current_dir(root)
        .args(["target", "list", "--installed"])
        .output();

    // A missing rustup is not itself a failure: a distribution-packaged Rust
    // may have the targets installed by other means, and the build that
    // follows will say so plainly enough if it does not.
    let Ok(output) = output else {
        return Ok(());
    };
    if !output.status.success() {
        return Ok(());
    }

    let missing = missing_targets(&String::from_utf8_lossy(&output.stdout), wanted);
    if missing.is_empty() {
        return Ok(());
    }
    bail!(
        "missing rustup targets: {}\nInstall them with `rustup target add {}`.",
        missing.join(", "),
        missing.join(" ")
    );
}

/// Borrows an owned argument list as the `&[&str]` the runners take.
fn borrow(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_env_var_is_uppercase_with_underscores() {
        assert_eq!(
            target_env_var("x86_64-pc-windows-gnu", "RUNNER"),
            "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER"
        );
        assert_eq!(
            target_env_var("aarch64-unknown-linux-gnu", "LINKER"),
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER"
        );
    }

    #[test]
    fn missing_targets_reports_only_what_is_absent() {
        let installed = "x86_64-unknown-linux-gnu\nx86_64-pc-windows-msvc\n";
        assert_eq!(
            missing_targets(installed, &["x86_64-pc-windows-msvc"]),
            Vec::<String>::new()
        );
        assert_eq!(
            missing_targets(installed, CHECK_TARGETS),
            vec![
                "aarch64-pc-windows-msvc",
                "x86_64-apple-darwin",
                "aarch64-apple-darwin"
            ]
        );
    }

    #[test]
    fn missing_targets_tolerates_the_installed_marker() {
        let installed = "x86_64-unknown-linux-gnu (installed)\naarch64-apple-darwin (installed)\n";
        assert_eq!(
            missing_targets(installed, &["aarch64-apple-darwin"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_check_covers_every_shipped_apple_and_windows_triple() {
        // The point of the job: what the release matrix builds on a premium
        // runner is what a Linux runner type-checks. Adding a target to
        // release.yml without adding it here is the mistake this catches.
        for target in CHECK_TARGETS {
            assert!(
                target.contains("windows") || target.contains("apple"),
                "{target} is buildable on Linux and does not belong here"
            );
        }
        assert!(CHECK_TARGETS.contains(&"aarch64-apple-darwin"));
        assert!(CHECK_TARGETS.contains(&"x86_64-pc-windows-msvc"));
    }

    #[test]
    fn the_check_builds_the_frontend_too() {
        let args = check_args("aarch64-apple-darwin");
        assert!(args.contains(&"--all-features".to_owned()));
        assert_eq!(args.last().unwrap(), "aarch64-apple-darwin");
    }

    #[test]
    fn the_wine_pass_leaves_out_the_frontend_and_the_tooling() {
        let args = wine_test_args();
        assert!(!args.contains(&"--all-features".to_owned()));
        for crate_name in WINE_EXCLUDES {
            let position = args
                .iter()
                .position(|argument| argument == crate_name)
                .unwrap_or_else(|| panic!("{crate_name} is not excluded"));
            assert_eq!(args[position - 1], "--exclude");
        }
    }

    #[test]
    fn the_console_bound_tests_are_skipped_after_a_bare_double_dash() {
        let args = wine_test_args();
        let separator = args
            .iter()
            .position(|argument| argument == "--")
            .expect("the skips must reach libtest, not cargo");
        // The target belongs to cargo, so it has to come first.
        assert_eq!(args[separator - 1], WINE_TARGET);
        assert_eq!(
            args.iter().filter(|argument| *argument == "--").count(),
            1,
            "a second `--` would be passed through as a test name filter"
        );
        for skip in WINE_SKIPS {
            let position = args
                .iter()
                .position(|argument| argument == skip)
                .unwrap_or_else(|| panic!("{skip} is not skipped"));
            assert!(position > separator);
            assert_eq!(args[position - 1], "--skip");
        }
    }

    #[test]
    fn an_explicit_wine_is_tried_before_the_packaged_one() {
        let candidates = wine_candidates(Some(OsStr::new("/opt/wine/bin/wine64")));
        assert_eq!(
            candidates.first().unwrap(),
            Path::new("/opt/wine/bin/wine64")
        );
        assert_eq!(
            candidates.last().unwrap(),
            Path::new("/usr/lib/wine/wine64"),
            "Ubuntu's package installs there and puts nothing on PATH"
        );
    }
}
