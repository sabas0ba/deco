//! Provisioning a deco, using this machine as the remote.
//!
//! `deco_remote::install` decides what to refuse against a scripted remote, so
//! the refusals are pinned there. What cannot be pinned there is whether the
//! commands it sends are the right commands: `dd`, `chmod` and `mv` either move
//! an executable binary or they do not, and a fake that agrees with the code
//! about them proves nothing.
//!
//! So the runner below is real. It runs each argv on this machine with `HOME`
//! pointed at a temporary directory, which is the whole of the difference
//! between it and [`deco_remote::TransportRunner`] — that one puts `ssh host`
//! in front. The binary really is copied, the install really is executed, and
//! the last test connects a client to what came out of it.
//!
//! Unix only: `uname`, `dd`, `chmod` and `mv` are the remote's tools, and a
//! Windows machine standing in for the remote has none of them. That is not a
//! gap in coverage of Windows *clients*, which build the same argv either way.

#![cfg(unix)]

use std::io::Read;
use std::path::{Path, PathBuf};

use deco_remote::client::Client;
use deco_remote::install::{self, ForOther, InstallError, Output, Runner};
use deco_remote::transport::Command;
use deco_remote::Installed;

/// A remote that is this machine, with a home directory of its own.
struct LocalRunner {
    home: PathBuf,
}

impl Runner for LocalRunner {
    fn run(
        &mut self,
        argv: &[String],
        stdin: Option<&mut dyn Read>,
    ) -> Result<Output, std::io::Error> {
        use std::process::{Command as OsCommand, Stdio};

        let mut child = OsCommand::new(&argv[0])
            .args(&argv[1..])
            // The point of the temporary home: `$HOME` in the probe's script is
            // expanded by the "remote", so this is what decides where a default
            // install lands — and nothing touches the real one.
            .env("HOME", &self.home)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(source) = stdin {
            let mut sink = child.stdin.take().expect("stdin was piped");
            std::io::copy(source, &mut sink)?;
            drop(sink);
        }
        let output = child.wait_with_output()?;
        Ok(Output {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// An empty home directory for a remote to have.
fn remote_home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "deco-install-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("a home directory");
    home
}

/// This binary, which is what a real `--remote-install` sends.
fn this_deco() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_deco"))
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[test]
fn an_install_puts_a_binary_there_that_actually_runs_and_serves() {
    let home = remote_home("round-trip");
    let mut runner = LocalRunner { home: home.clone() };

    let outcome = install::ensure(&mut runner, None, this_deco(), version(), ForOther::Refuse)
        .expect("an install");
    let expected = home.join(".deco/bin/deco");
    assert_eq!(
        outcome,
        Installed::Sent {
            path: expected.display().to_string(),
            version: format!("deco {}", version()),
            replaced: None,
        }
    );

    // It arrived whole. A truncated upload is the failure this would catch, and
    // the one a fake runner never could.
    assert_eq!(
        std::fs::metadata(&expected).expect("a binary").len(),
        std::fs::metadata(this_deco()).expect("this binary").len()
    );
    // And nothing is left beside it: the staging file is renamed, not copied.
    assert!(!home.join(".deco/bin/deco.incoming").exists());

    // The point of all of it — what was installed serves the protocol. Run
    // directly rather than over a transport, which is the same substitution
    // `remote_session.rs` makes.
    let workspace = home.join("project");
    std::fs::create_dir_all(&workspace).expect("a workspace");
    std::fs::write(workspace.join("main.rs"), "fn main() {}\n").expect("a file");
    let mut client = Client::start(&Command {
        program: expected.display().to_string(),
        args: vec![
            "--server".to_owned(),
            "--stdio".to_owned(),
            "--workspace".to_owned(),
            workspace.display().to_string(),
        ],
    })
    .expect("the installed deco should start");
    client.handshake().expect("a handshake");
    assert_eq!(client.read("main.rs").expect("a read"), "fn main() {}\n");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_second_install_of_the_same_version_sends_nothing() {
    let home = remote_home("idempotent");
    let mut runner = LocalRunner { home: home.clone() };

    install::ensure(&mut runner, None, this_deco(), version(), ForOther::Refuse)
        .expect("an install");
    let installed = home.join(".deco/bin/deco");
    let stamp = std::fs::metadata(&installed)
        .and_then(|meta| meta.modified())
        .expect("a timestamp");

    let outcome = install::ensure(&mut runner, None, this_deco(), version(), ForOther::Refuse)
        .expect("a second install");
    let expected = format!("deco {}", version());
    assert!(
        matches!(&outcome, Installed::AlreadyThere { version, .. } if version == &expected),
        "{outcome:?}"
    );
    // Not rewritten: the version answer is what decides, and a re-upload over a
    // slow link every time the editor starts would be the cost of getting it
    // wrong.
    assert_eq!(
        std::fs::metadata(&installed)
            .and_then(|meta| meta.modified())
            .expect("a timestamp"),
        stamp
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_real_file_that_is_not_deco_survives_being_pointed_at() {
    // The refusal is unit-tested against a fake; this checks the file is still
    // there afterwards, which is the part that actually matters to whoever
    // mistyped the path.
    let home = remote_home("stranger");
    let notes = home.join("notes.txt");
    std::fs::write(&notes, "not a binary\n").expect("a file");
    let mut runner = LocalRunner { home: home.clone() };

    let error = install::ensure(
        &mut runner,
        Some(&notes.display().to_string()),
        this_deco(),
        version(),
        ForOther::Refuse,
    )
    .expect_err("a refusal");
    assert!(matches!(error, InstallError::NotDeco { .. }), "{error}");
    assert_eq!(
        std::fs::read_to_string(&notes).expect("still there"),
        "not a binary\n"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_install_that_cannot_be_completed_fails_rather_than_half_happening() {
    // The destination's directory cannot be created, because a file is already
    // in its way. Chosen over a read-only directory deliberately: this test also
    // runs as root in CI's containers, and root is not stopped by a permission
    // bit — it is stopped by `bin` not being a directory.
    let home = remote_home("blocked");
    let blocker = home.join("bin");
    std::fs::write(&blocker, "in the way\n").expect("a file");
    let mut runner = LocalRunner { home: home.clone() };

    let error = install::ensure(
        &mut runner,
        Some(&blocker.join("deco").display().to_string()),
        this_deco(),
        version(),
        ForOther::Refuse,
    )
    .expect_err("a failure");
    // It says which step, because "could not put deco on the remote" alone
    // leaves a person with nothing to check.
    assert!(matches!(error, InstallError::Step { .. }), "{error}");
    assert!(error.to_string().contains("install directory"), "{error}");

    // And the file that was in the way is untouched — a failed install is not
    // allowed to be destructive.
    assert_eq!(
        std::fs::read_to_string(&blocker).expect("still there"),
        "in the way\n"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// A [`LocalRunner`] that claims to be a machine this one is not.
///
/// Only the probe is intercepted; everything after it runs for real, so what is
/// exercised below is the actual staging, upload, `chmod`, rename and
/// does-it-run check — with the *download* path chosen instead of "send the
/// binary I am running".
struct ForeignRunner {
    inner: LocalRunner,
    os: &'static str,
    arch: &'static str,
}

impl Runner for ForeignRunner {
    fn run(
        &mut self,
        argv: &[String],
        stdin: Option<&mut dyn Read>,
    ) -> Result<Output, std::io::Error> {
        if argv.iter().any(|part| part.contains("uname -s")) {
            return Ok(Output {
                status: Some(0),
                stdout: format!(
                    "{}\n{}\n{}\n",
                    if self.os == "macos" {
                        "Darwin"
                    } else {
                        "Linux"
                    },
                    self.arch,
                    self.inner.home.display()
                ),
                stderr: String::new(),
            });
        }
        self.inner.run(argv, stdin)
    }
}

/// A [`Fetcher`](deco_remote::fetch::Fetcher) answering from a table.
struct Canned(Vec<(String, Vec<u8>)>);

impl deco_remote::fetch::Fetcher for Canned {
    fn get(&mut self, url: &str) -> Result<Vec<u8>, String> {
        self.0
            .iter()
            .find(|(at, _)| at == url)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| format!("no such release asset: {url}"))
    }
}

/// A release archive holding a "deco" that answers `--version`.
fn release_archive(root: &Path, target: &str) -> Vec<u8> {
    let stage = root.join(format!("deco-{target}"));
    std::fs::create_dir_all(&stage).expect("a directory");
    std::fs::write(
        stage.join("deco"),
        format!("#!/bin/sh\necho 'deco {}'\n", version()),
    )
    .expect("a file");
    let archive = root.join("release.tar.gz");
    assert!(std::process::Command::new("tar")
        .arg("czf")
        .arg(&archive)
        .arg("-C")
        .arg(root)
        .arg(format!("deco-{target}"))
        .status()
        .expect("tar runs")
        .success());
    let bytes = std::fs::read(&archive).expect("the archive");
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&stage);
    bytes
}

#[test]
fn a_different_platform_is_still_refused_unless_the_download_was_asked_for() {
    // The rule the separate flag exists for. `--remote-install` alone reaches
    // nothing: it sends the file already running here, or it refuses.
    let home = remote_home("mismatch-refused");
    let mut runner = ForeignRunner {
        inner: LocalRunner { home: home.clone() },
        // A platform this machine is not, whichever machine is running the test.
        os: if cfg!(target_os = "macos") {
            "linux"
        } else {
            "macos"
        },
        arch: "aarch64",
    };

    let error = install::ensure(&mut runner, None, this_deco(), version(), ForOther::Refuse)
        .expect_err("a refusal");
    assert!(
        matches!(error, InstallError::PlatformMismatch { .. }),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_download_for_the_remotes_platform_is_checked_and_then_installed() {
    // The whole path, with only the network replaced: probe says another
    // platform, the checksums and archive come from the table, the checksum is
    // verified here, the member is taken out of the archive, and what lands on
    // the "remote" runs and reports its version.
    let home = remote_home("downloaded");
    let scratch = home.join("scratch");
    std::fs::create_dir_all(&scratch).expect("a directory");

    let (os, arch, target) = if cfg!(target_os = "macos") {
        ("linux", "aarch64", "aarch64-unknown-linux-gnu")
    } else {
        ("macos", "aarch64", "aarch64-apple-darwin")
    };
    let asset = deco_remote::fetch::asset_for(target);
    let archive = release_archive(&scratch, target);
    let sums = format!("{}  {asset}\n", deco_remote::fetch::digest_of(&archive));

    let mut fetcher = Canned(vec![
        (
            deco_remote::fetch::checksums_url(version()),
            sums.into_bytes(),
        ),
        (deco_remote::fetch::asset_url(version(), &asset), archive),
    ]);
    let mut runner = ForeignRunner {
        inner: LocalRunner { home: home.clone() },
        os,
        arch,
    };

    let outcome = install::ensure(
        &mut runner,
        None,
        this_deco(),
        version(),
        ForOther::Download {
            fetcher: &mut fetcher,
            into: &scratch,
        },
    )
    .expect("an install");

    let Installed::Sent {
        path,
        version: reported,
        ..
    } = outcome
    else {
        panic!("expected a send, got {outcome:?}");
    };
    // What arrived is the downloaded binary, not this machine's own.
    assert_eq!(reported, format!("deco {}", version()));
    let landed = std::fs::read_to_string(&path).expect("the installed file");
    assert!(landed.starts_with("#!/bin/sh"), "{landed}");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_download_that_does_not_match_its_checksum_installs_nothing() {
    // The refusal that matters. The bytes arrive, they are not what the release
    // vouches for, and the destination is left as it was — empty.
    let home = remote_home("bad-checksum");
    let scratch = home.join("scratch");
    std::fs::create_dir_all(&scratch).expect("a directory");

    let (os, arch, target) = if cfg!(target_os = "macos") {
        ("linux", "aarch64", "aarch64-unknown-linux-gnu")
    } else {
        ("macos", "aarch64", "aarch64-apple-darwin")
    };
    let asset = deco_remote::fetch::asset_for(target);
    let archive = release_archive(&scratch, target);
    let sums = format!(
        "{}  {asset}\n",
        deco_remote::fetch::digest_of(b"a different archive entirely")
    );

    let mut fetcher = Canned(vec![
        (
            deco_remote::fetch::checksums_url(version()),
            sums.into_bytes(),
        ),
        (deco_remote::fetch::asset_url(version(), &asset), archive),
    ]);
    let mut runner = ForeignRunner {
        inner: LocalRunner { home: home.clone() },
        os,
        arch,
    };

    let error = install::ensure(
        &mut runner,
        None,
        this_deco(),
        version(),
        ForOther::Download {
            fetcher: &mut fetcher,
            into: &scratch,
        },
    )
    .expect_err("a refusal");
    assert!(
        error.to_string().contains("does not match the checksum"),
        "{error}"
    );
    // Nothing was installed: the default destination is untouched.
    assert!(!home.join(".deco/bin/deco").exists());
    let _ = std::fs::remove_dir_all(&home);
}
