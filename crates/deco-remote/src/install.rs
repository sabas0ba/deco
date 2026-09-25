//! Installing deco on the remote.
//!
//! The rest of this crate assumes the remote environment already has a `deco`
//! to run. This module installs one.
//!
//! # Install policy
//!
//! Connecting an editor to a machine does not authorise it to install software
//! there, so nothing here runs automatically. A session that finds no `deco` on
//! the remote fails and mentions `--remote-install`; it does not install deco
//! itself. The rules are:
//!
//! - **Only when requested.** No code path installs without an explicit request
//!   from the caller.
//! - **Never a binary that cannot run.** The remote platform is detected before
//!   anything is sent. If it differs from this machine, the install is either
//!   rejected or uses a downloaded build for that platform, instead of uploading
//!   a binary that later fails with `Exec format error`.
//! - **Never over a file that is not deco.** If the destination holds a program
//!   that does not identify itself as deco, it is left unchanged.
//!   `--remote-server-path /usr/bin/vim` is treated as a typo.
//! - **Never a partially written binary at the destination.** The upload goes to
//!   a temporary name in the same directory and is renamed when complete. An
//!   interrupted install leaves the old deco, or nothing, rather than a
//!   truncated executable.
//!
//! # When the remote is a different platform
//!
//! Uploading this machine's binary works when both platforms match: Linux to
//! Linux, and every WSL and container case. A macOS laptop provisioning a Linux
//! server needs a binary it does not have. Obtaining one requires network access,
//! which is a broader permission than copying the running binary, so it requires
//! a separate opt-in.
//!
//! [`ForOther`] is that opt-in, passed as a parameter rather than read from a
//! setting. [`fetch`](crate::fetch) is reached from here only when the caller
//! passes [`ForOther::Download`], which `--remote-install` alone does not do.
//! Without it, a platform mismatch is rejected.
//!
//! The remote is assumed to have a POSIX shell and `uname`, `mkdir`, `dd`,
//! `chmod` and `mv`. Running `deco --server` over `ssh` already makes the same
//! assumption.

use std::io::Read;

use crate::transport::{command_for, Command, TransportOptions};
use crate::Authority;

/// The result of running a command on the remote.
#[derive(Debug, Clone, Default)]
pub struct Output {
    /// The exit status, or `None` if the process was killed by a signal.
    pub status: Option<i32>,
    /// Standard output, as text. No command here is expected to print binary data.
    pub stdout: String,
    /// Standard error, kept because it usually contains the only useful diagnostic.
    pub stderr: String,
}

impl Output {
    /// Whether the command exited successfully.
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

/// Something that can run a command on the remote.
///
/// A trait rather than a concrete transport so that this module's logic, such
/// as what to reject and in which order to run steps, can be tested without a
/// second machine. [`TransportRunner`] is the real implementation.
pub trait Runner {
    /// Runs `argv` on the remote, feeding `stdin` to it if given.
    ///
    /// `argv` is a program and its arguments, never a shell string. Where the
    /// transport passes it through a remote shell, it quotes each argument, as
    /// [`transport`](crate::transport) describes.
    fn run(
        &mut self,
        argv: &[String],
        stdin: Option<&mut dyn Read>,
    ) -> Result<Output, std::io::Error>;
}

/// Runs commands on a remote over one of deco's transports.
pub struct TransportRunner {
    authority: Authority,
    options: TransportOptions,
}

impl TransportRunner {
    /// Runs commands on `authority`.
    pub fn new(authority: Authority, options: TransportOptions) -> Self {
        Self { authority, options }
    }
}

impl Runner for TransportRunner {
    fn run(
        &mut self,
        argv: &[String],
        stdin: Option<&mut dyn Read>,
    ) -> Result<Output, std::io::Error> {
        let command = command_for(&self.authority, argv, &self.options)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        run_locally(&command, stdin)
    }
}

/// Spawns `command` on this machine, where every transport command starts.
fn run_locally(command: &Command, stdin: Option<&mut dyn Read>) -> Result<Output, std::io::Error> {
    use std::process::{Command as OsCommand, Stdio};

    let mut child = OsCommand::new(&command.program)
        .args(&command.args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Stdin is written on this thread while the child's output is not being
    // read. This is safe only because the input is at most a binary and
    // `wait_with_output` drains the pipes afterwards. A command that wrote
    // megabytes of stdout while reading stdin could deadlock; none of the
    // commands used here does.
    if let Some(source) = stdin {
        let mut sink = child.stdin.take().expect("stdin was piped");
        std::io::copy(source, &mut sink)?;
        // Dropped explicitly because `dd` reads until end of file. Leaving the
        // pipe open would make the wait below hang.
        drop(sink);
    }

    let output = child.wait_with_output()?;
    Ok(Output {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Why an install did not happen.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The remote platform could not be probed.
    #[error("could not ask the remote what it is: {0}")]
    Probe(String),
    /// The probe output could not be parsed.
    #[error("the remote did not say what it is; it answered `{answer}`")]
    Unrecognised {
        /// The probe output.
        answer: String,
    },
    /// The remote is a different platform from this machine.
    #[error(
        "this deco is built for {local} and the remote is {remote}, so sending it there \
         would produce a binary that cannot run. `--remote-install-download` fetches the \
         release built for {remote} and checks it before sending it; or install one there \
         yourself and point `--remote-server-path` at it."
    )]
    PlatformMismatch {
        /// This machine, as `os-arch`.
        local: String,
        /// The remote, as `os-arch`.
        remote: String,
    },
    /// Something is already at the destination and it is not deco.
    #[error(
        "`{path}` on the remote is not deco, so it was left alone; \
         point `--remote-server-path` somewhere else"
    )]
    NotDeco {
        /// The destination that was checked.
        path: String,
    },
    /// A step of the install failed on the remote.
    #[error("{what} on the remote failed{}{}", .status.map(|s| format!(" (exit {s})")).unwrap_or_default(), .stderr.as_ref().map(|e| format!(": {e}")).unwrap_or_default())]
    Step {
        /// Which step, in words.
        what: &'static str,
        /// Its exit status.
        status: Option<i32>,
        /// Whatever it put on stderr.
        stderr: Option<String>,
    },
    /// The local binary could not be read to send.
    #[error("could not read this deco to send it: {0}")]
    LocalBinary(#[from] std::io::Error),
    /// A deco for the remote's platform could not be obtained.
    #[error(transparent)]
    Fetch(#[from] crate::fetch::FetchError),
    /// The uploaded binary did not run on the remote.
    #[error("the deco that was installed at `{path}` does not run there: {detail}")]
    Unusable {
        /// Where it was put.
        path: String,
        /// What went wrong when it was asked for its version.
        detail: String,
    },
}

/// The platform reported by the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// The operating system, in Rust's spelling: `linux`, `macos`.
    pub os: String,
    /// The architecture, in Rust's spelling: `x86_64`, `aarch64`.
    pub arch: String,
    /// The home directory of the account the transport logs in as.
    pub home: String,
}

impl Platform {
    /// This machine, for comparing against a remote.
    pub fn local() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            home: String::new(),
        }
    }

    /// `os-arch`, as shown in the mismatch message.
    pub fn name(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }

    fn matches(&self, other: &Self) -> bool {
        self.os == other.os && self.arch == other.arch
    }
}

/// Translates `uname -s` into the name Rust uses for the same system.
///
/// Unknown systems keep their lowercased name instead of being mapped to a
/// guess. This value only decides whether the two platforms match, and an
/// unrecognised name should not match.
fn os_name(uname: &str) -> String {
    match uname {
        "Linux" => "linux".to_owned(),
        "Darwin" => "macos".to_owned(),
        other => other.to_lowercase(),
    }
}

/// Translates `uname -m` into the name Rust uses for the same architecture.
fn arch_name(uname: &str) -> String {
    match uname {
        "x86_64" | "amd64" => "x86_64".to_owned(),
        "aarch64" | "arm64" => "aarch64".to_owned(),
        other => other.to_lowercase(),
    }
}

/// Queries the remote platform and home directory.
///
/// This runs one `sh -c` with a constant script. It is the only exception to
/// this crate's no-shell-strings rule, and it is allowed because nothing is
/// interpolated into it: the remote expands `$HOME`, and no local value appears
/// in the script. Over SSH the transport quotes the script as one argument, so
/// the login shell passes it to `sh -c` unchanged.
pub fn probe(runner: &mut dyn Runner) -> Result<Platform, InstallError> {
    let argv = [
        "sh".to_owned(),
        "-c".to_owned(),
        "uname -s && uname -m && printf '%s\\n' \"$HOME\"".to_owned(),
    ];
    let output = runner
        .run(&argv, None)
        .map_err(|error| InstallError::Probe(error.to_string()))?;
    if !output.ok() {
        return Err(InstallError::Probe(if output.stderr.trim().is_empty() {
            format!("it exited {:?}", output.status)
        } else {
            output.stderr.trim().to_owned()
        }));
    }
    let mut lines = output.stdout.lines();
    let (Some(os), Some(arch), Some(home)) = (lines.next(), lines.next(), lines.next()) else {
        return Err(InstallError::Unrecognised {
            answer: output.stdout.trim().to_owned(),
        });
    };
    let home = home.trim();
    if home.is_empty() {
        return Err(InstallError::Unrecognised {
            answer: output.stdout.trim().to_owned(),
        });
    }
    // A trailing slash is stripped so the path does not start with `//.deco`,
    // which POSIX allows implementations to interpret specially. A home of
    // exactly `/`, used for root on some minimal images, becomes empty, and the
    // resulting path `/.deco/bin/deco` is correct.
    let home = home.trim_end_matches('/');
    Ok(Platform {
        os: os_name(os.trim()),
        arch: arch_name(arch.trim()),
        home: home.to_owned(),
    })
}

/// Where deco installs itself when no path was given.
///
/// The path is under the account's home directory rather than on the system
/// path. A per-user install needs no privileges and does not affect other
/// users.
pub fn default_path(platform: &Platform) -> String {
    format!("{}/.deco/bin/deco", platform.home)
}

/// What is at `path` on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AtPath {
    /// Nothing, so the path is free to install into.
    Nothing,
    /// A deco reporting this version string.
    Deco(String),
    /// Some other file, which must not be overwritten.
    Stranger,
}

/// Inspects `path` on the remote without writing anything.
///
/// Existence is checked separately from whether the file runs. A file that
/// exists but does not answer `--version` is a `Stranger`, not free space.
/// Examples are a `notes.txt` hit by a `--remote-server-path` typo, a binary
/// for another architecture, or a non-executable file. Checking `--version`
/// alone would treat these as absent, and the install would overwrite them.
fn look_at(runner: &mut dyn Runner, path: &str) -> AtPath {
    let exists = runner
        .run(&["test".to_owned(), "-e".to_owned(), path.to_owned()], None)
        .map(|output| output.ok())
        .unwrap_or(false);
    if !exists {
        return AtPath::Nothing;
    }
    let argv = [path.to_owned(), "--version".to_owned()];
    let Ok(output) = runner.run(&argv, None) else {
        return AtPath::Stranger;
    };
    let said = output.stdout.trim().to_owned();
    // Expected output is `deco 0.1.0`. The prefix identifies deco; answering
    // `--version` alone does not make a file safe to overwrite.
    if output.ok() && said.starts_with("deco ") {
        AtPath::Deco(said)
    } else {
        AtPath::Stranger
    }
}

/// The result of `ensure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// A deco of the requested version was already installed.
    AlreadyThere {
        /// The install path.
        path: String,
        /// The version it reports.
        version: String,
    },
    /// This machine's binary was sent.
    Sent {
        /// The install path.
        path: String,
        /// The version it reports after installation.
        version: String,
        /// The version previously installed, if any.
        replaced: Option<String>,
    },
}

impl Installed {
    /// The install path, in either case.
    pub fn path(&self) -> &str {
        match self {
            Self::AlreadyThere { path, .. } | Self::Sent { path, .. } => path,
        }
    }
}

/// Ensures that a deco of this version is on the remote, and returns its path.
///
/// `path` is the install path; `None` means [`default_path`]. `binary` is the
/// local file to send. The caller obtains it from `std::env::current_exe`
/// instead of this module, so that a test can supply its own file.
/// What to do when the remote platform differs from the one this deco was built
/// for.
///
/// An enum rather than a `bool` because the two variants grant different
/// permissions, and a name at the call site is clearer than `true`.
pub enum ForOther<'a> {
    /// Reject, naming both platforms. This is the behaviour of `--remote-install`
    /// alone.
    Refuse,
    /// Download the release built for the remote platform, verify it, and send
    /// it.
    ///
    /// This variant grants network access. Constructing it is the only way to
    /// request that access.
    Download {
        /// How the bytes are retrieved.
        fetcher: &'a mut dyn crate::fetch::Fetcher,
        /// A directory where the verified binary may be written.
        into: &'a std::path::Path,
    },
}

pub fn ensure(
    runner: &mut dyn Runner,
    path: Option<&str>,
    binary: &std::path::Path,
    version: &str,
    for_other: ForOther<'_>,
) -> Result<Installed, InstallError> {
    let platform = probe(runner)?;
    let destination = match path {
        Some(path) => path.to_owned(),
        None => default_path(&platform),
    };

    let wanted = format!("deco {version}");
    let found = match look_at(runner, &destination) {
        AtPath::Deco(found) if found == wanted => {
            return Ok(Installed::AlreadyThere {
                path: destination,
                version: found,
            });
        }
        AtPath::Deco(found) => Some(found),
        // Rejected before the platform check, because for a mistyped path "not
        // deco" is a more useful error than a platform mismatch.
        AtPath::Stranger => return Err(InstallError::NotDeco { path: destination }),
        AtPath::Nothing => None,
    };

    // Select the file to upload. After this point both cases are handled the
    // same way: a verified download is a local binary, and staging, renaming
    // and the run check are shared.
    let local = Platform::local();
    let sending = if platform.matches(&local) {
        std::borrow::Cow::Borrowed(binary)
    } else {
        match for_other {
            ForOther::Refuse => {
                return Err(InstallError::PlatformMismatch {
                    local: local.name(),
                    remote: platform.name(),
                })
            }
            ForOther::Download { fetcher, into } => std::borrow::Cow::Owned(
                crate::fetch::binary_for(fetcher, &platform, version, into)?,
            ),
        }
    };

    let (directory, _) = destination
        .rsplit_once('/')
        .unwrap_or((".", destination.as_str()));
    step(
        runner,
        "creating the install directory",
        &["mkdir".to_owned(), "-p".to_owned(), directory.to_owned()],
        None,
    )?;

    // Staged next to the destination rather than in a temporary directory, so
    // the rename below stays on one filesystem and remains atomic.
    let staged = format!("{destination}.incoming");
    let mut file = std::fs::File::open(sending.as_ref())?;
    step(
        runner,
        "sending the binary",
        &[
            "dd".to_owned(),
            format!("of={staged}"),
            "bs=65536".to_owned(),
        ],
        Some(&mut file),
    )?;
    step(
        runner,
        "making the binary executable",
        &["chmod".to_owned(), "755".to_owned(), staged.clone()],
        None,
    )?;
    step(
        runner,
        "putting the binary in place",
        &["mv".to_owned(), staged, destination.clone()],
        None,
    )?;

    // Verify that the binary runs. The upload can succeed and still leave a
    // binary that does not run, for example because of a full disk, a `noexec`
    // mount, or a missing libc. Detecting it here gives a clearer error than a
    // handshake that never completes.
    let AtPath::Deco(now) = look_at(runner, &destination) else {
        return Err(InstallError::Unusable {
            path: destination,
            detail: "it does not report a version".to_owned(),
        });
    };
    Ok(Installed::Sent {
        path: destination,
        version: now,
        replaced: found,
    })
}

/// Runs one step of the install, turning a non-zero exit into an error that
/// names the step.
fn step(
    runner: &mut dyn Runner,
    what: &'static str,
    argv: &[String],
    stdin: Option<&mut dyn Read>,
) -> Result<(), InstallError> {
    let output = runner
        .run(argv, stdin)
        .map_err(|error| InstallError::Step {
            what,
            status: None,
            stderr: Some(error.to_string()),
        })?;
    if !output.ok() {
        return Err(InstallError::Step {
            what,
            status: output.status,
            stderr: Some(output.stderr.trim().to_owned()).filter(|e| !e.is_empty()),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake remote that returns scripted answers.
    #[derive(Default)]
    struct Fake {
        /// Answers, selected by the first key contained in the joined argv.
        answers: Vec<(&'static str, Output)>,
        /// Whether a file is already at the destination before the install.
        present: bool,
        /// Every argv that was run, in order.
        ran: Vec<Vec<String>>,
        /// How many bytes were fed to the last command that took stdin.
        sent: usize,
    }

    impl Fake {
        fn answering(answers: Vec<(&'static str, Output)>) -> Self {
            Self {
                answers,
                ..Self::default()
            }
        }

        /// The same, with a file already at the destination.
        fn holding(answers: Vec<(&'static str, Output)>) -> Self {
            Self {
                answers,
                present: true,
                ..Self::default()
            }
        }

        /// Whether the install has moved the binary into place yet. `test -e`
        /// and `--version` answer differently before and after.
        fn moved(&self) -> bool {
            self.ran[..self.ran.len() - 1]
                .iter()
                .any(|argv| argv[0] == "mv")
        }

        fn programs(&self) -> Vec<&str> {
            self.ran.iter().map(|argv| argv[0].as_str()).collect()
        }
    }

    fn ok(stdout: &str) -> Output {
        Output {
            status: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    fn fails(status: i32, stderr: &str) -> Output {
        Output {
            status: Some(status),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    impl Runner for Fake {
        fn run(
            &mut self,
            argv: &[String],
            stdin: Option<&mut dyn Read>,
        ) -> Result<Output, std::io::Error> {
            self.ran.push(argv.to_vec());
            if let Some(source) = stdin {
                let mut swallowed = Vec::new();
                source.read_to_end(&mut swallowed)?;
                self.sent = swallowed.len();
            }
            // Handled before the scripted answers because file existence is
            // part of the fake's state, not a per-test answer. A fake that
            // answered yes to every `test -e` would make every install look
            // like an overwrite.
            if argv[0] == "test" {
                let there = self.present || self.moved();
                return Ok(if there { ok("") } else { fails(1, "") });
            }
            let joined = argv.join(" ");
            for (matches, answer) in &self.answers {
                if joined.contains(matches) {
                    return Ok(answer.clone());
                }
            }
            if joined.contains("--version") {
                // A remote answers `--version` differently before and after an
                // install. Without this, the check after the upload could pass
                // for the wrong reason.
                return Ok(if self.moved() {
                    ok("deco 0.1.0")
                } else {
                    fails(127, "sh: deco: not found")
                });
            }
            Ok(ok(""))
        }
    }

    /// A probe answer for a remote that matches this machine, so that the
    /// platform check fails only in tests that intend it to.
    fn same_platform() -> Output {
        ok(&format!(
            "{}\n{}\n/home/u\n",
            match std::env::consts::OS {
                "linux" => "Linux",
                "macos" => "Darwin",
                other => other,
            },
            std::env::consts::ARCH
        ))
    }

    /// A stand-in for the binary being sent.
    ///
    /// Named per thread because these tests run in parallel in one process.
    /// With a shared path, one test could truncate the file while another read
    /// it, which occasionally resulted in sending zero bytes.
    fn binary() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "deco-install-test-binary-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, vec![7u8; 4096]).expect("a file to send");
        path
    }

    #[test]
    fn a_remote_that_is_a_different_platform_is_refused_before_anything_is_sent() {
        let mut fake = Fake::answering(vec![("uname", ok("Linux\nsparc64\n/home/u\n"))]);
        let error =
            ensure(&mut fake, None, &binary(), "0.1.0", ForOther::Refuse).expect_err("a refusal");
        assert!(
            matches!(&error, InstallError::PlatformMismatch { remote, .. } if remote == "linux-sparc64"),
            "{error}"
        );
        // Rejecting early means nothing was written to the remote.
        assert!(
            !fake.programs().contains(&"dd"),
            "sent anyway: {:?}",
            fake.programs()
        );
        // The message names an alternative, because retrying cannot fix this.
        assert!(
            error.to_string().contains("--remote-server-path"),
            "{error}"
        );
    }

    #[test]
    fn something_that_is_not_deco_is_never_overwritten() {
        let mut fake = Fake::holding(vec![
            ("uname", same_platform()),
            ("--version", ok("VIM - Vi IMproved 9.1")),
        ]);
        let error = ensure(
            &mut fake,
            Some("/usr/bin/vim"),
            &binary(),
            "0.1.0",
            ForOther::Refuse,
        )
        .expect_err("a refusal to overwrite");
        assert!(
            matches!(&error, InstallError::NotDeco { path } if path == "/usr/bin/vim"),
            "{error}"
        );
        assert!(
            !fake.programs().contains(&"dd"),
            "overwrote it: {:?}",
            fake.programs()
        );
    }

    #[test]
    fn a_deco_of_the_same_version_is_left_where_it_is() {
        let mut fake = Fake::holding(vec![
            ("uname", same_platform()),
            ("--version", ok("deco 0.1.0")),
        ]);
        let outcome =
            ensure(&mut fake, None, &binary(), "0.1.0", ForOther::Refuse).expect("already there");
        assert_eq!(
            outcome,
            Installed::AlreadyThere {
                path: "/home/u/.deco/bin/deco".to_owned(),
                version: "deco 0.1.0".to_owned(),
            }
        );
        assert!(!fake.programs().contains(&"dd"), "sent it needlessly");
    }

    #[test]
    fn a_deco_of_another_version_is_replaced_and_says_what_it_replaced() {
        let mut fake = Fake::holding(vec![
            ("uname", same_platform()),
            ("--version", ok("deco 0.0.9")),
        ]);
        // The version answer is fixed, so the check after the upload also sees
        // the old string. This test checks that a different version is replaced
        // rather than rejected, and that the old version is reported.
        let outcome =
            ensure(&mut fake, None, &binary(), "0.1.0", ForOther::Refuse).expect("a replacement");
        assert!(
            matches!(&outcome, Installed::Sent { replaced, .. } if replaced.as_deref() == Some("deco 0.0.9")),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_install_stages_beside_the_destination_and_renames_it_into_place() {
        let mut fake = Fake::answering(vec![("uname", same_platform())]);
        ensure(
            &mut fake,
            Some("/opt/deco/bin/deco"),
            &binary(),
            "0.1.0",
            ForOther::Refuse,
        )
        .expect("an install");

        assert_eq!(
            fake.programs(),
            [
                "sh",
                "test",
                "mkdir",
                "dd",
                "chmod",
                "mv",
                "test",
                "/opt/deco/bin/deco"
            ]
        );
        let dd = fake
            .ran
            .iter()
            .find(|argv| argv[0] == "dd")
            .expect("a send");
        let mv = fake
            .ran
            .iter()
            .find(|argv| argv[0] == "mv")
            .expect("a move");
        // Never written directly to the destination. An interrupted upload must
        // not leave a truncated binary where the next session runs it.
        assert_eq!(dd[1], "of=/opt/deco/bin/deco.incoming");
        assert_eq!(
            mv[1..],
            ["/opt/deco/bin/deco.incoming", "/opt/deco/bin/deco"]
        );
        // Staged in the destination's directory, so the rename stays on one
        // filesystem and remains atomic.
        assert!(mv[1].starts_with("/opt/deco/bin/"), "{:?}", mv[1]);
        assert_eq!(fake.sent, 4096, "the whole binary should be sent");
    }

    #[test]
    fn a_file_that_is_there_but_answers_nothing_is_a_stranger_rather_than_free_space() {
        // This is why existence is checked separately: a `--remote-server-path`
        // typo that points at a notes file, or a binary for another
        // architecture. Neither answers `--version`, and checking that alone
        // would treat both as absent and overwrite them.
        let mut fake = Fake::holding(vec![
            ("uname", same_platform()),
            ("--version", fails(126, "Permission denied")),
        ]);
        let error = ensure(
            &mut fake,
            Some("/home/u/notes.txt"),
            &binary(),
            "0.1.0",
            ForOther::Refuse,
        )
        .expect_err("a refusal to overwrite");
        assert!(matches!(error, InstallError::NotDeco { .. }), "{error}");
        assert!(
            !fake.programs().contains(&"dd"),
            "overwrote it: {:?}",
            fake.programs()
        );
    }

    #[test]
    fn a_failing_step_says_which_step_and_what_the_remote_said() {
        let mut fake = Fake::answering(vec![
            ("uname", same_platform()),
            (
                "dd",
                fails(1, "dd: writing to 'x': No space left on device"),
            ),
        ]);
        let error =
            ensure(&mut fake, None, &binary(), "0.1.0", ForOther::Refuse).expect_err("a failure");
        let said = error.to_string();
        assert!(said.contains("sending the binary"), "{said}");
        assert!(said.contains("No space left on device"), "{said}");
        // The install stopped there instead of renaming a partial file into place.
        assert!(!fake.programs().contains(&"mv"), "{:?}", fake.programs());
    }

    #[test]
    fn a_binary_that_arrives_but_will_not_run_is_reported_rather_than_connected_to() {
        // Every step succeeds, but the installed binary does not answer
        // `--version`, as with a `noexec` mount or a missing libc.
        let mut fake = Fake::answering(vec![
            ("uname", same_platform()),
            ("--version", fails(126, "Permission denied")),
        ]);
        let error =
            ensure(&mut fake, None, &binary(), "0.1.0", ForOther::Refuse).expect_err("a failure");
        assert!(matches!(error, InstallError::Unusable { .. }), "{error}");
    }

    #[test]
    fn a_remote_that_answers_nothing_useful_is_not_guessed_at() {
        let mut fake = Fake::answering(vec![("uname", ok("Linux\n"))]);
        let error =
            ensure(&mut fake, None, &binary(), "0.1.0", ForOther::Refuse).expect_err("a refusal");
        assert!(
            matches!(error, InstallError::Unrecognised { .. }),
            "{error}"
        );

        let mut fake = Fake::answering(vec![("uname", fails(127, "sh: uname: not found"))]);
        let error =
            ensure(&mut fake, None, &binary(), "0.1.0", ForOther::Refuse).expect_err("a refusal");
        assert!(error.to_string().contains("uname: not found"), "{error}");
    }

    #[test]
    fn the_probe_interpolates_nothing_of_ours_into_the_shell() {
        // The only `sh -c` in this crate. It is allowed because the script is a
        // constant: the remote expands `$HOME`, and no local value is included.
        // This test fails if that changes.
        let mut fake = Fake::answering(vec![("uname", same_platform())]);
        probe(&mut fake).expect("a platform");
        let script = &fake.ran[0][2];
        assert_eq!(script, "uname -s && uname -m && printf '%s\\n' \"$HOME\"");
    }

    #[cfg(unix)]
    #[test]
    fn the_probe_script_reaches_sh_intact_through_ssh_quoting() {
        // Replays what the remote login shell does with the SSH command: the
        // arguments after the host, joined with spaces, parsed by a POSIX
        // shell. The probe must reach `sh -c` as one script.
        struct ThroughSsh;
        impl Runner for ThroughSsh {
            fn run(
                &mut self,
                argv: &[String],
                _stdin: Option<&mut dyn Read>,
            ) -> Result<Output, std::io::Error> {
                let command = command_for(
                    &Authority::parse("ssh-remote+myhost").unwrap(),
                    argv,
                    &TransportOptions::default(),
                )
                .unwrap();
                let host = command.args.iter().position(|a| a == "myhost").unwrap();
                let script = command.args[host + 1..].join(" ");
                let output = std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(script)
                    .env("HOME", "/home/u with space")
                    .output()?;
                Ok(Output {
                    status: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                })
            }
        }
        let platform = probe(&mut ThroughSsh).expect("a platform");
        assert_eq!(platform.home, "/home/u with space");
        assert!(!platform.os.is_empty() && !platform.arch.is_empty());
    }

    #[test]
    fn a_home_directory_with_a_trailing_slash_does_not_double_it() {
        // POSIX allows implementations to interpret a leading `//` specially,
        // so the trailing slash is stripped before joining.
        let mut fake = Fake::answering(vec![("uname", ok("Linux\nx86_64\n/home/u/\n"))]);
        let platform = probe(&mut fake).expect("a platform");
        assert_eq!(default_path(&platform), "/home/u/.deco/bin/deco");

        // A `$HOME` of exactly `/`, used for root on some minimal images, is a
        // valid home rather than a missing one.
        let mut fake = Fake::answering(vec![("uname", ok("Linux\nx86_64\n/\n"))]);
        let platform = probe(&mut fake).expect("a platform");
        assert_eq!(default_path(&platform), "/.deco/bin/deco");
    }
}
