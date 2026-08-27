//! Running the `git` binary.
//!
//! [`crate::status`] is a pure parser; this is the part that is not. It exists
//! because deco does what VS Code does — it shells out to `git` rather than
//! linking a library — and the reasons are the same three:
//!
//! - **It inherits the user's git.** Their `includeIf` config, their
//!   `credential.helper`, their hooks, their `core.fsmonitor`. A library
//!   reimplements a subset of that and disagrees with the command line the
//!   user checks their work with.
//! - **It costs no dependency.** The binary is already on the machine of
//!   anyone who has a repository to open; libgit2's subtree is not, and the
//!   [README](https://github.com/sabas0ba/deco#readme) counts its crates in
//!   public.
//! - **Absent is a state it can be in.** No git on the machine, or a folder
//!   that is not a repository, is [`ScmError::NoBinary`] or
//!   [`ScmError::NotARepository`] — a feature that is not there, rather than a
//!   broken one.
//!
//! Two rules this module keeps, and the failure each prevents:
//!
//! - **No shell, ever.** Arguments go as a vector, the way `deco-lsp` spawns a
//!   language server. A repository is something a user cloned, and a branch
//!   called `$(rm -rf ~)` is a legal branch name.
//! - **It cannot ask a question.** A child that decides to prompt for a
//!   passphrase with the editor holding its pipes is a hang with no way out,
//!   so the environment tells git that no terminal is available.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::status::{self, Malformed, Status};

/// Why there is no status to show.
///
/// Split by what the user would have to do about it: install git, open a
/// repository, or report a bug. A caller that renders these all the same way
/// is throwing away the difference between "this folder has no git in it",
/// which is normal, and "git said something incomprehensible", which is not.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScmError {
    /// There is no `git` to run.
    #[error("`{}` is not on this machine", .0.to_string_lossy())]
    NoBinary(OsString),
    /// The folder is not in a working tree.
    #[error("`{}` is not inside a git repository", .0.display())]
    NotARepository(PathBuf),
    /// git ran and refused.
    #[error("git exited with {}: {message}", code.map(|c| c.to_string()).unwrap_or_else(|| "a signal".into()))]
    Refused {
        /// Its exit status, when it had one rather than a signal.
        code: Option<i32>,
        /// What it wrote to stderr, trimmed.
        message: String,
    },
    /// git could not be run, or its output could not be read.
    #[error("could not run git: {0}")]
    Unusable(String),
    /// git ran, said something, and it was not the documented format.
    #[error(transparent)]
    Malformed(#[from] Malformed),
}

/// The `git` to run.
///
/// A path rather than a hardcoded `"git"` so that VS Code's `git.path` setting
/// means what it means there: a machine with git somewhere unusual is a
/// machine the setting exists for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Git {
    program: OsString,
}

impl Default for Git {
    fn default() -> Self {
        Self::new("git")
    }
}

impl Git {
    /// Whatever `git.path` said, or `git` to be found on `PATH`.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// What `git status` says about the working tree containing `directory`.
    ///
    /// Blocking. On a repository with a very large working tree this is
    /// hundreds of milliseconds, so a frontend calls it from somewhere it can
    /// afford to wait — not from a render, and not on a keystroke.
    pub fn status(&self, directory: &Path) -> Result<Status, ScmError> {
        // `--branch` for the four header lines the status bar needs; `-z` so
        // paths arrive unquoted (see `status::parse`); `--porcelain=v2`
        // because v1 cannot say which side of a change is staged.
        //
        // `--untracked-files=normal` is git's default, but it is also what
        // `status.showUntrackedFiles` can change out from under this, and a
        // count that silently stopped including new files would be wrong in
        // the quiet direction. Named so it means what it says here.
        let output = self.run(
            directory,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=normal",
            ],
        )?;
        Ok(status::parse(&output)?)
    }

    /// Runs git in `directory` and hands back its stdout.
    fn run(&self, directory: &Path, args: &[&str]) -> Result<String, ScmError> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(directory)
            // Not `--no-optional-locks`: an unknown *flag* is a hard error on
            // an older git, while an unknown environment variable is ignored.
            // Either way the point is that showing a status must not take the
            // index lock — a status bar refreshing on save should never be the
            // reason a `git commit` in a terminal fails.
            .env("GIT_OPTIONAL_LOCKS", "0")
            // Nothing here can answer a question, so anything that would ask
            // one must fail instead of waiting for an answer that is not
            // coming.
            .env("GIT_TERMINAL_PROMPT", "0")
            // The porcelain format is not translated, but the *errors* are,
            // and one of them has to be told apart from the rest below.
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::null());

        let output = match command.output() {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ScmError::NoBinary(self.program.clone()))
            }
            Err(error) => return Err(ScmError::Unusable(error.to_string())),
        };

        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            // Matching git's English is not something to be proud of, and it
            // is why `LC_ALL=C` is set above. The alternative is a second
            // process — `git rev-parse --show-toplevel` before every status —
            // to learn something this run has already found out.
            if message.contains("not a git repository") {
                return Err(ScmError::NotARepository(directory.to_path_buf()));
            }
            return Err(ScmError::Refused {
                code: output.status.code(),
                message,
            });
        }

        // Lossy rather than an error: a repository can hold a path that is not
        // UTF-8, and losing the name of one file is better than losing the
        // branch and the count as well. The parser splits on NUL, which
        // survives the conversion.
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_binary_is_a_feature_that_is_absent() {
        let git = Git::new("git-that-is-not-installed-anywhere");
        let error = git
            .status(Path::new("."))
            .expect_err("there is no such program");
        assert!(
            matches!(error, ScmError::NoBinary(_)),
            "a machine without git has the feature missing, not broken: {error}"
        );
    }

    #[test]
    fn the_program_is_whatever_the_setting_said() {
        assert_eq!(Git::default(), Git::new("git"));
        assert_ne!(Git::default(), Git::new("/opt/homebrew/bin/git"));
    }

    /// The rest of this module needs a real git, and CI has one — but a
    /// contributor's machine may not, and a test that fails there for a reason
    /// that is not their change is worse than one that says why it skipped.
    fn git_or_skip() -> Option<Git> {
        let git = Git::default();
        match git.status(Path::new(env!("CARGO_MANIFEST_DIR"))) {
            Err(ScmError::NoBinary(_)) => {
                eprintln!("skipped: no git on this machine");
                None
            }
            _ => Some(git),
        }
    }

    #[test]
    fn decos_own_checkout_reads_as_a_repository() {
        let Some(git) = git_or_skip() else { return };
        // This crate is in deco's repository, so this is a working tree
        // whatever else is true — and running against it rather than a
        // fixture means the test exercises a real `git status`, with whatever
        // the contributor's config does to it.
        let status = git
            .status(Path::new(env!("CARGO_MANIFEST_DIR")))
            .expect("deco's own checkout");
        assert!(
            !status.head.label().is_empty(),
            "a checkout is on a branch or at a commit; both have a label"
        );
    }

    #[test]
    fn a_folder_outside_a_repository_says_so() {
        let Some(git) = git_or_skip() else { return };
        // The temporary directory is not inside deco's checkout, and creating
        // nothing in it keeps the test from depending on what is.
        let outside = std::env::temp_dir();
        match git.status(&outside) {
            Err(ScmError::NotARepository(path)) => assert_eq!(path, outside),
            // Somebody's `TMPDIR` is inside a repository. Unusual, not wrong,
            // and not something to fail a build over.
            Ok(_) => eprintln!("skipped: {} is inside a working tree", outside.display()),
            Err(other) => panic!("expected a plain refusal, got {other}"),
        }
    }
}
