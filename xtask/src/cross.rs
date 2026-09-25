//! Testing the Windows and macOS targets from a Linux host.
//!
//! The premium runners run only on a release tag or on explicit request (see
//! the header of `.github/workflows/ci.yml`). Without another check, other
//! pushes would give no information about the platforms most deco users use.
//! A Linux runner can provide most of it with two checks. CI runs both once a
//! day on main rather than on every push, because they detect changes in
//! dependencies or targets, which happen independently of this repository:
//!
//! * **A type check per shipped triple.** `cargo check` stops before linking,
//!   so it needs no MSVC toolchain and no Apple SDK, only the target's prebuilt
//!   `std`, which rustup provides for all four. It checks code behind `#[cfg]`
//!   that a Linux build does not compile: the branch of `paths.rs` that uses
//!   `%APPDATA%`, the branch of `binding.rs` that maps `cmd` instead of `ctrl`,
//!   and the frontend's per-platform windowing code.
//!
//! * **The tests, run under Wine.** Built for `x86_64-pc-windows-gnu` with
//!   MinGW and run through Wine as cargo's target runner. These are real
//!   Windows binaries running Windows code paths, including the tests that
//!   spawn a child process. The painting tests in [`WINE_SKIPS`] are excluded
//!   because they need a console, which Wine does not have here.
//!
//! Neither check covers the following, so the tagged run on real runners is
//! still needed:
//!
//! * **macOS at runtime.** There is no Wine for Darwin. The macOS check only
//!   compiles.
//! * **The MSVC ABI.** Wine runs the GNU target, so problems specific to the
//!   MSVC linker or C runtime are not detected.
//! * **Wine's accuracy.** Wine reimplements Win32. Where it differs from
//!   Windows, a test can pass here and fail on Windows, or the reverse.
//! * **The GPU frontend and the real console.** `deco-gui` is excluded because
//!   wgpu and winit need a GPU adapter and a compositor, which a headless Wine
//!   does not have. [`WINE_SKIPS`] excludes tests for the same reason at the
//!   console level.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The triples the release matrix ships that no Linux runner can link.
///
/// Keep this in sync with the matrices in `ci.yml` and `release.yml`. A shipped
/// target missing here is first built only after a tag is pushed.
pub const CHECK_TARGETS: &[&str] = &[
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

/// The triple the Wine pass builds and runs.
///
/// GNU rather than MSVC, because a Linux host can link it: MinGW is an apt
/// package, while the MSVC target needs Microsoft's linker and import
/// libraries. The ABI differs, but the `#[cfg(windows)]` code, which this pass
/// runs, is the same.
pub const WINE_TARGET: &str = "x86_64-pc-windows-gnu";

/// The linker MinGW installs for [`WINE_TARGET`].
pub const MINGW_LINKER: &str = "x86_64-w64-mingw32-gcc";

/// Crates the Wine pass leaves out.
///
/// `deco-gui` needs a GPU adapter and a compositor. `xtask` is host tooling
/// that runs git and npm and asserts on this repository's layout. Running it as
/// a Windows binary under Wine would test the tooling rather than the editor.
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
/// All missing targets are reported at once rather than one per failed build,
/// so a single `rustup target add` fixes them.
pub fn missing_targets(installed: &str, wanted: &[&str]) -> Vec<String> {
    let present: Vec<&str> = installed
        .lines()
        .map(str::trim)
        // `rustup target list --installed` prints bare triples, but the
        // unfiltered `list` appends ` (installed)`. Accept both formats.
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
/// `--all-features` so the GPU frontend is also checked. It has the most
/// platform-specific code and is behind a feature flag, so a default build
/// would skip it.
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
/// Every test that paints a frame through crossterm. On Windows, crossterm
/// decides once, at first use, whether the terminal supports ANSI. If not, every
/// command goes to the console API instead of to the given writer, so a test
/// that paints into a `Vec<u8>` still needs the process to own a console. On a
/// CI runner Wine has no terminal to create one from, and the calls fail with
/// `Invalid handle`. A real Windows runner has a console and passes these
/// tests, so they are still covered there.
///
/// A **prefix** rather than test names, because every test that calls `paint`
/// fails here. With a list of names, a new painting test would not be skipped.
/// All other `deco-tui` tests compare rendered strings and never reach
/// crossterm, so the rest of the frontend's suite runs under Wine, and
/// `painting_` matches only the console-bound tests.
pub const WINE_SKIPS: &[&str] = &["painting_"];

/// The `cargo test` invocation the Wine pass runs.
///
/// Default features, unlike the check above. `--all-features` would enable
/// `deco`'s `gui` feature and add wgpu and winit to the build, although their
/// tests are excluded.
///
/// `--no-fail-fast` runs every test binary even after one fails, so one run
/// reports every failure instead of only the first failing crate's.
pub fn wine_test_args() -> Vec<String> {
    let mut args: Vec<String> = ["test", "--locked", "--workspace", "--no-fail-fast"]
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
/// `$WINE` first, so a Wine installed in a non-standard location can be
/// specified directly. `/usr/lib/wine/wine64` last, because Ubuntu's `wine64`
/// package installs the binary there and adds nothing to `PATH`. The
/// `/usr/bin/wine` wrapper belongs to the `wine` package, which installs the
/// 32-bit stack and requires a second apt architecture.
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
/// Runs the cheapest checks first, like `cargo xtask ci`. The type checks need
/// only rustup targets, so a failure that both passes would detect is reported
/// before the Wine build.
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

    // Wine logs every unimplemented stub it calls, which for a test binary is
    // several screens of output. Keep an explicit setting, so
    // `WINEDEBUG=+file cargo xtask cross` still works.
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
/// The first `wine` in a new `$WINEPREFIX` takes about ten seconds to create it
/// (registry, drive mappings, services), and cargo can start the next test
/// binary during that time. Tests then fail for reasons unrelated to the code,
/// such as a missing directory or console. A CI runner always starts with a new
/// prefix, so this rarely happens on a developer machine.
///
/// Failure is not fatal: if `wineboot` fails, the run continues and the tests
/// report the actual problem, which is more useful than a setup error here.
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

    // A missing rustup is not a failure. A distribution-packaged Rust may have
    // the targets installed by other means, and otherwise the following build
    // reports the error.
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
        // A Linux runner type-checks every target the release matrix builds on
        // a premium runner. This detects a target added to release.yml but not
        // here.
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
    fn the_skip_is_a_rule_and_not_a_list_of_names() {
        // The current console-bound test names. If the prefix were replaced with
        // these names, a new painting test would not be skipped and would fail
        // under Wine. This happened before and is why this test exists.
        for name in [
            "painting_writes_every_span_and_positions_the_cursor",
            "painting_a_frame_with_no_cursor_leaves_it_hidden",
            "painting_never_emits_a_span_s_own_escape_sequence",
        ] {
            assert!(
                WINE_SKIPS.iter().any(|skip| name.starts_with(skip)),
                "{name} would run under Wine"
            );
        }
        assert!(
            WINE_SKIPS.iter().all(|skip| !skip.contains("_the_")),
            "the skips read like a prefix rather than whole test names"
        );
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
