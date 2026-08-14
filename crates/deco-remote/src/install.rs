//! Getting a deco onto the remote in the first place.
//!
//! Everything else in this crate assumes the far end already has a `deco` to
//! run. This is the part that puts one there — and it is the part where the
//! interesting decision is not technical.
//!
//! # What this is allowed to do
//!
//! Pointing an editor at a machine is not the same as authorising it to install
//! software there, so nothing here happens on its own. A session that finds no
//! `deco` on the remote fails and says that `--remote-install` exists; it does
//! not helpfully fix it. The rules that follow from that:
//!
//! - **Only when asked.** There is no code path that installs without the
//!   caller having decided to.
//! - **Never a binary that cannot run.** The remote is asked what it is before
//!   anything is sent, and a platform that does not match this machine's is a
//!   refusal rather than an upload that fails later with `Exec format error`.
//! - **Never over something that is not a deco.** If the destination already
//!   holds a program that does not identify itself as deco, it is left alone.
//!   `--remote-server-path /usr/bin/vim` is a typo, not an instruction.
//! - **Never a half-written binary at the destination.** The upload goes to a
//!   temporary name beside it and is renamed once complete, so an interrupted
//!   install leaves the old deco — or nothing — rather than a truncated file
//!   that is executable and broken.
//!
//! # What it cannot do yet
//!
//! It uploads *this* machine's binary, so it works when both ends are the same
//! platform: Linux to Linux, and every WSL and container case. A macOS laptop
//! provisioning a Linux server is exactly the case it refuses, because the fix
//! is fetching a release built for the remote rather than sending the wrong
//! file, and where deco is willing to download from is its own decision to make
//! rather than one to slip in here.
//!
//! The remote is assumed to have a POSIX shell and `uname`, `mkdir`, `dd`,
//! `chmod` and `mv`. That is the same assumption already made by running
//! `deco --server` over `ssh`.

use std::io::Read;

use crate::transport::{command_for, Command, TransportOptions};
use crate::Authority;

/// What running a command on the remote produced.
#[derive(Debug, Clone, Default)]
pub struct Output {
    /// The exit status, or `None` if the process was killed by a signal.
    pub status: Option<i32>,
    /// Standard output, as text. Nothing here is expected to be binary.
    pub stdout: String,
    /// Standard error, kept because it is usually the only useful diagnosis.
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
/// A trait rather than a concrete transport so that the decisions in this
/// module — what to refuse, in what order to do things — can be tested without
/// a second machine. [`TransportRunner`] is the real one.
pub trait Runner {
    /// Runs `argv` on the remote, feeding `stdin` to it if given.
    ///
    /// `argv` is a program and its arguments, never a shell string, for the
    /// reason [`transport`](crate::transport) gives.
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
        let command = command_for(&self.authority, argv, self.options)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        run_locally(&command, stdin)
    }
}

/// Spawns `command` on this machine, which is where every transport starts.
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

    // Written on this thread while the child's output is not being read, which
    // is safe only because nothing here sends more than a binary and the
    // pipes involved are drained by `wait_with_output` afterwards. A command
    // that produced megabytes of stdout while reading stdin could deadlock;
    // none of the ones below does.
    if let Some(source) = stdin {
        let mut sink = child.stdin.take().expect("stdin was piped");
        std::io::copy(source, &mut sink)?;
        // Dropped explicitly: `dd` reads until end of file, and leaving the pipe
        // open would hang the wait below rather than fail it.
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
    /// The remote could not be asked what it is.
    #[error("could not ask the remote what it is: {0}")]
    Probe(String),
    /// The remote answered, but not in a way this understands.
    #[error("the remote did not say what it is; it answered `{answer}`")]
    Unrecognised {
        /// What came back.
        answer: String,
    },
    /// The remote is a different platform from this machine.
    #[error(
        "this deco is built for {local} and the remote is {remote}, so sending it there \
         would produce a binary that cannot run. Install a deco built for {remote} on it \
         and point `--remote-server-path` at that."
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
    /// The uploaded binary did not run on the remote.
    #[error("the deco that was installed at `{path}` does not run there: {detail}")]
    Unusable {
        /// Where it was put.
        path: String,
        /// What went wrong when it was asked for its version.
        detail: String,
    },
}

/// What the remote said it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// The operating system, in Rust's spelling: `linux`, `macos`.
    pub os: String,
    /// The architecture, in Rust's spelling: `x86_64`, `aarch64`.
    pub arch: String,
    /// The home directory of whoever the transport logs in as.
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

    /// `os-arch`, which is what the mismatch message names.
    pub fn name(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }

    fn matches(&self, other: &Self) -> bool {
        self.os == other.os && self.arch == other.arch
    }
}

/// Translates `uname -s` into the name Rust uses for the same system.
///
/// Unknown systems keep their own lowercased name rather than being guessed at:
/// the only thing this value decides is whether the two ends match, and an
/// unrecognised name that fails to match is the right outcome.
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

/// Asks the remote what it is and where its home directory is.
///
/// One `sh -c` with a constant script, which is the exception to this crate's
/// no-shell-strings rule and is allowed precisely because nothing is
/// interpolated into it: `$HOME` is expanded *by the remote*, and no value from
/// this end appears in the text at all.
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
    // A trailing slash is stripped so the join below does not produce `//.deco`,
    // which POSIX allows an implementation to treat as its own thing. A home of
    // exactly `/` — some minimal images run as root that way — becomes empty,
    // and `/.deco/bin/deco` is then the right answer rather than an error.
    let home = home.trim_end_matches('/');
    Ok(Platform {
        os: os_name(os.trim()),
        arch: arch_name(arch.trim()),
        home: home.to_owned(),
    })
}

/// Where deco installs itself when no path was given.
///
/// Under the account's own home directory rather than anywhere on the system
/// path: installing for one user needs no privileges and affects nobody else,
/// which is the least surprising thing an editor can do to a machine.
pub fn default_path(platform: &Platform) -> String {
    format!("{}/.deco/bin/deco", platform.home)
}

/// What is at `path` on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AtPath {
    /// Nothing, so there is a free space to install into.
    Nothing,
    /// A deco, which says it is this version.
    Deco(String),
    /// Something else, which must not be written over.
    Stranger,
}

/// Looks at `path` on the remote without writing anything.
///
/// Existence is asked separately from runnability, and the difference is the
/// whole point: a file that is there but does not answer `--version` — a
/// `notes.txt` a `--remote-server-path` typo landed on, a binary from another
/// architecture, something not executable — is a stranger rather than an empty
/// space. Deciding by "did `--version` work" alone would make each of those
/// look like nothing was there, and the install would overwrite it.
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
    // `deco 0.1.0` — the prefix is what makes this an identification rather than
    // a guess that anything which answers `--version` is safe to overwrite.
    if output.ok() && said.starts_with("deco ") {
        AtPath::Deco(said)
    } else {
        AtPath::Stranger
    }
}

/// What `ensure` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// A deco of the wanted version was already there.
    AlreadyThere {
        /// Where it is.
        path: String,
        /// What it reports.
        version: String,
    },
    /// This machine's binary was sent.
    Sent {
        /// Where it was put.
        path: String,
        /// What it reports now that it is there.
        version: String,
        /// What was there before, if anything was.
        replaced: Option<String>,
    },
}

impl Installed {
    /// Where the deco is, either way.
    pub fn path(&self) -> &str {
        match self {
            Self::AlreadyThere { path, .. } | Self::Sent { path, .. } => path,
        }
    }
}

/// Makes sure a deco of this version is on the remote, and says where it is.
///
/// `path` is where to put it; `None` means [`default_path`]. `binary` is the
/// local file to send, which the caller gets from `std::env::current_exe`
/// rather than this module reaching for it, so that a test can send something
/// it chose.
pub fn ensure(
    runner: &mut dyn Runner,
    path: Option<&str>,
    binary: &std::path::Path,
    version: &str,
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
        // Refused before the platform check, because "that is not deco" is the
        // more useful thing to hear about a mistyped path than "that machine is
        // a different architecture".
        AtPath::Stranger => return Err(InstallError::NotDeco { path: destination }),
        AtPath::Nothing => None,
    };

    let local = Platform::local();
    if !platform.matches(&local) {
        return Err(InstallError::PlatformMismatch {
            local: local.name(),
            remote: platform.name(),
        });
    }

    let (directory, _) = destination
        .rsplit_once('/')
        .unwrap_or((".", destination.as_str()));
    step(
        runner,
        "creating the install directory",
        &["mkdir".to_owned(), "-p".to_owned(), directory.to_owned()],
        None,
    )?;

    // Beside the destination rather than in a temporary directory, so the rename
    // below cannot cross a filesystem and stop being atomic.
    let staged = format!("{destination}.incoming");
    let mut file = std::fs::File::open(binary)?;
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

    // Asked rather than assumed: the upload can succeed and still leave
    // something that will not run — a partially full disk, a `noexec` mount, a
    // binary needing a libc the remote does not have. Better here than as a
    // handshake that mysteriously never answers.
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

/// Runs one step of the install, turning a non-zero exit into an error that says
/// which step it was.
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

    /// A remote that answers from a script rather than existing.
    #[derive(Default)]
    struct Fake {
        /// What to answer, matched by the first argument that appears in the key.
        answers: Vec<(&'static str, Output)>,
        /// Whether something is already at the destination before any of this.
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

        /// Whether the install has put the binary in place yet, which is what
        /// makes `test -e` and `--version` answer differently before and after.
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
            // Asked before the scripted answers, because whether a file is
            // there is a fact about the fake rather than something a test
            // spells out — and a fake that said "yes, something is there" to
            // every `test -e` would make every install look like an overwrite.
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
                // install, and a fake that ignored that would let the check
                // *after* the upload pass for the wrong reason.
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
    /// platform check is not what a test trips over unless it means to.
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
    /// Named per thread because these tests run in parallel in one process, and
    /// a shared path meant one test truncating the file another was reading —
    /// which showed up as a send of zero bytes, occasionally.
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
        let error = ensure(&mut fake, None, &binary(), "0.1.0").expect_err("a refusal");
        assert!(
            matches!(&error, InstallError::PlatformMismatch { remote, .. } if remote == "linux-sparc64"),
            "{error}"
        );
        // The point of refusing early: nothing was written to that machine.
        assert!(
            !fake.programs().contains(&"dd"),
            "sent anyway: {:?}",
            fake.programs()
        );
        // And the message says what to do instead, because the person hitting
        // this cannot fix it by retrying.
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
        let error = ensure(&mut fake, Some("/usr/bin/vim"), &binary(), "0.1.0")
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
        let outcome = ensure(&mut fake, None, &binary(), "0.1.0").expect("already there");
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
        // The version answer is fixed, so the check after the upload sees the old
        // string too; what this pins is that a differing version is a replace
        // rather than a refusal, and that the old one is reported.
        let outcome = ensure(&mut fake, None, &binary(), "0.1.0").expect("a replacement");
        assert!(
            matches!(&outcome, Installed::Sent { replaced, .. } if replaced.as_deref() == Some("deco 0.0.9")),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_install_stages_beside_the_destination_and_renames_it_into_place() {
        let mut fake = Fake::answering(vec![("uname", same_platform())]);
        ensure(&mut fake, Some("/opt/deco/bin/deco"), &binary(), "0.1.0").expect("an install");

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
        // Never written directly to the destination: an interrupted upload must
        // not leave a truncated binary where the next session will run it.
        assert_eq!(dd[1], "of=/opt/deco/bin/deco.incoming");
        assert_eq!(
            mv[1..],
            ["/opt/deco/bin/deco.incoming", "/opt/deco/bin/deco"]
        );
        // Staged in the destination's own directory, so the rename cannot cross a
        // filesystem and stop being atomic.
        assert!(mv[1].starts_with("/opt/deco/bin/"), "{:?}", mv[1]);
        assert_eq!(fake.sent, 4096, "the whole binary should be sent");
    }

    #[test]
    fn a_file_that_is_there_but_answers_nothing_is_a_stranger_rather_than_free_space() {
        // The case that makes existence worth asking about separately: a
        // `--remote-server-path` typo that lands on someone's notes, or a binary
        // for another architecture. Neither answers `--version`, and deciding by
        // that alone would read both as "nothing is there" and overwrite them.
        let mut fake = Fake::holding(vec![
            ("uname", same_platform()),
            ("--version", fails(126, "Permission denied")),
        ]);
        let error = ensure(&mut fake, Some("/home/u/notes.txt"), &binary(), "0.1.0")
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
        let error = ensure(&mut fake, None, &binary(), "0.1.0").expect_err("a failure");
        let said = error.to_string();
        assert!(said.contains("sending the binary"), "{said}");
        assert!(said.contains("No space left on device"), "{said}");
        // And it stopped there rather than renaming a partial file into place.
        assert!(!fake.programs().contains(&"mv"), "{:?}", fake.programs());
    }

    #[test]
    fn a_binary_that_arrives_but_will_not_run_is_reported_rather_than_connected_to() {
        // Everything succeeds except that the installed binary never answers
        // `--version`: a `noexec` mount, or a libc the remote does not have.
        let mut fake = Fake::answering(vec![
            ("uname", same_platform()),
            ("--version", fails(126, "Permission denied")),
        ]);
        let error = ensure(&mut fake, None, &binary(), "0.1.0").expect_err("a failure");
        assert!(matches!(error, InstallError::Unusable { .. }), "{error}");
    }

    #[test]
    fn a_remote_that_answers_nothing_useful_is_not_guessed_at() {
        let mut fake = Fake::answering(vec![("uname", ok("Linux\n"))]);
        let error = ensure(&mut fake, None, &binary(), "0.1.0").expect_err("a refusal");
        assert!(
            matches!(error, InstallError::Unrecognised { .. }),
            "{error}"
        );

        let mut fake = Fake::answering(vec![("uname", fails(127, "sh: uname: not found"))]);
        let error = ensure(&mut fake, None, &binary(), "0.1.0").expect_err("a refusal");
        assert!(error.to_string().contains("uname: not found"), "{error}");
    }

    #[test]
    fn the_probe_interpolates_nothing_of_ours_into_the_shell() {
        // The one `sh -c` in this crate. It is allowed because the script is a
        // constant — `$HOME` is expanded by the remote, and no value from this
        // end reaches the text. If that ever stops being true, this fails.
        let mut fake = Fake::answering(vec![("uname", same_platform())]);
        probe(&mut fake).expect("a platform");
        let script = &fake.ran[0][2];
        assert_eq!(script, "uname -s && uname -m && printf '%s\\n' \"$HOME\"");
    }

    #[test]
    fn a_home_directory_with_a_trailing_slash_does_not_double_it() {
        // `//.deco/bin/deco` is a path POSIX lets an implementation treat as its
        // own thing, so the slash is stripped rather than joined onto.
        let mut fake = Fake::answering(vec![("uname", ok("Linux\nx86_64\n/home/u/\n"))]);
        let platform = probe(&mut fake).expect("a platform");
        assert_eq!(default_path(&platform), "/home/u/.deco/bin/deco");

        // And `$HOME` of exactly `/` — root on some minimal images — is a real
        // home rather than a missing one.
        let mut fake = Fake::answering(vec![("uname", ok("Linux\nx86_64\n/\n"))]);
        let platform = probe(&mut fake).expect("a platform");
        assert_eq!(default_path(&platform), "/.deco/bin/deco");
    }
}
