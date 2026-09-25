//! Running the `git` binary.
//!
//! [`crate::status`] is a pure parser; this module runs the process. Like VS
//! Code, deco runs the `git` binary rather than linking a library, for three
//! reasons:
//!
//! - **It uses the user's git configuration.** This includes `includeIf`
//!   config, `credential.helper`, hooks and `core.fsmonitor`. A library
//!   implements only a subset of these and can disagree with the git command
//!   line the user relies on.
//! - **It adds no dependency.** Anyone with a repository to open already has
//!   the binary; libgit2's dependency tree would be new, and the
//!   [README](https://github.com/sabas0ba/deco#readme) publishes the crate
//!   count.
//! - **Absence is a supported state.** No git on the machine, or a folder that
//!   is not a repository, is reported as [`ScmError::NoBinary`] or
//!   [`ScmError::NotARepository`]: the feature is unavailable, not broken.
//!
//! This module follows two rules:
//!
//! - **No shell.** Arguments are passed as a vector, as `deco-lsp` does when it
//!   spawns a language server. A cloned repository can contain a branch named
//!   `$(rm -rf ~)`, which is a valid branch name.
//! - **Git cannot prompt.** A child that prompts for a passphrase while the
//!   editor holds its pipes would hang indefinitely, so stdin is closed and
//!   Git's terminal prompt is disabled. Commit hooks are arbitrary programs and
//!   can still use another prompt mechanism.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::change::{
    Branch, CheckoutPlan, Comparison, ComparisonKind, ComparisonRequest, Operation,
};
use crate::status::{self, Malformed, Status};

/// Why a git operation failed.
///
/// The variants correspond to the user's remedy: install git, open a
/// repository, or report a bug. Callers should distinguish "this folder is not
/// a repository", which is normal, from "git returned unexpected output", which
/// is not.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScmError {
    /// The `git` binary was not found.
    #[error("`{}` is not on this machine", .0.to_string_lossy())]
    NoBinary(OsString),
    /// The folder is not in a working tree.
    #[error("`{}` is not inside a git repository", .0.display())]
    NotARepository(PathBuf),
    /// git ran and exited with an error.
    #[error("git exited with {}: {message}", code.map(|c| c.to_string()).unwrap_or_else(|| "a signal".into()))]
    Refused {
        /// The exit status, or `None` if git was killed by a signal.
        code: Option<i32>,
        /// Its stderr output, trimmed.
        message: String,
    },
    /// git could not be run, or its output could not be read.
    #[error("could not run git: {0}")]
    Unusable(String),
    /// A path that is not a plain relative path inside the working tree.
    ///
    /// Rejected rather than passed to git: `HEAD:<path>` resolves an absolute
    /// or `..`-containing path against the working directory, so the caller
    /// would be shown a different file's contents without any error.
    #[error("`{0}` is not a path inside the working tree")]
    NotInWorkingTree(String),
    /// git's output did not match the documented format.
    #[error(transparent)]
    Malformed(#[from] Malformed),
}

/// The `git` to run.
///
/// A configurable path rather than a hardcoded `"git"`, so that VS Code's
/// `git.path` setting works as it does there, for git installed in a
/// non-standard location.
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
    /// The `git.path` setting, or `git` to be looked up on `PATH`.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// The `git status` of the working tree containing `directory`.
    ///
    /// Blocking. On a repository with a very large working tree this takes
    /// hundreds of milliseconds, so a frontend must not call it during
    /// rendering or on a keystroke.
    pub fn status(&self, directory: &Path) -> Result<Status, ScmError> {
        // `--branch` for the four header lines the status bar needs; `-z` so
        // paths arrive unquoted (see `status::parse`); `--porcelain=v2`
        // because v1 does not show which side of a change is staged.
        //
        // `--untracked-files=all` rather than git's default of `normal`, which
        // collapses a new directory into a single `? newdir/` record. The
        // count is documented as one per file, and under `normal` a newly added
        // folder with a dozen files would be counted as `±1`, an undercount in
        // a very common case.
        //
        // The option is also set explicitly because `status.showUntrackedFiles`
        // can change the default, and the count should not depend on a setting
        // deco does not read.
        //
        // Cost: `all` descends into untracked directories, so a large untracked
        // directory that is not in `.gitignore` makes status slower. Ignored
        // files are still excluded, because `--ignored` is not passed, so
        // `target/` and `node_modules/` are usually not walked.
        let output = self.run_bytes(
            directory,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=all",
            ],
        )?;
        let output = String::from_utf8(output).map_err(|error| {
            ScmError::Unusable(format!(
                "git status returned a path or branch name that is not UTF-8: {error}"
            ))
        })?;
        Ok(status::parse(&output)?)
    }

    /// The root of the repository containing `directory`.
    ///
    /// Every path git reports is relative to this root, not to the folder deco
    /// was started in. The two differ when a subdirectory of a repository is
    /// opened, which is common. Querying the root once lets [`Git::committed`]
    /// and [`Status`] use the same relative paths.
    pub fn root(&self, directory: &Path) -> Result<PathBuf, ScmError> {
        let output = self.run_bytes(directory, &["rev-parse", "--show-toplevel"])?;
        // Remove the command's one record terminator, not every trailing
        // newline: a Unix directory name may itself end in one.
        let root = output.strip_suffix(b"\n").unwrap_or(&output);
        let root = root.strip_suffix(b"\r").unwrap_or(root);
        if root.is_empty() {
            return Err(ScmError::NotARepository(directory.to_path_buf()));
        }
        let root = std::str::from_utf8(root).map_err(|error| {
            ScmError::Unusable(format!(
                "git reported a repository path that is not UTF-8: {error}"
            ))
        })?;
        Ok(PathBuf::from(root))
    }

    /// The per-worktree and shared administrative directories used by Git.
    ///
    /// A linked worktree keeps both outside its working tree: `.git` is a file
    /// pointing at the primary checkout's administrative area. Remote callers
    /// need these paths in addition to [`Git::root`] when deciding whether a
    /// repository stays inside an authority boundary.
    ///
    /// Relative answers are resolved against `directory`, which is the working
    /// directory Git used to produce them. The paths are not canonicalised;
    /// confinement callers must do that before comparing them with a boundary
    /// so a symbolic link cannot disguise where one leads.
    pub fn administrative_directories(&self, directory: &Path) -> Result<[PathBuf; 2], ScmError> {
        let read = |args: &[&str], name: &str| -> Result<PathBuf, ScmError> {
            let output = self.run_bytes(directory, args)?;
            let path = output.strip_suffix(b"\n").unwrap_or(&output);
            let path = path.strip_suffix(b"\r").unwrap_or(path);
            if path.is_empty() {
                return Err(ScmError::Unusable(format!(
                    "git reported an empty {name} path"
                )));
            }
            let path = std::str::from_utf8(path).map_err(|error| {
                ScmError::Unusable(format!(
                    "git reported a {name} path that is not UTF-8: {error}"
                ))
            })?;
            let path = PathBuf::from(path);
            Ok(if path.is_absolute() {
                path
            } else {
                directory.join(path)
            })
        };

        Ok([
            read(&["rev-parse", "--absolute-git-dir"], "Git directory")?,
            read(&["rev-parse", "--git-common-dir"], "common Git directory")?,
        ])
    }

    /// The committed text of a file, to compare a buffer against.
    ///
    /// `path` is relative to the **repository root**, as in [`Status`] and
    /// [`Git::root`], not to `directory`. `git show HEAD:a` reads the
    /// repository's `a` regardless of the directory it runs in, so each path
    /// identifies one file. (`HEAD:./a` is resolved against the working
    /// directory instead. The two differ when the workspace is a subdirectory,
    /// and a gutter drawn from the wrong blob is not visibly wrong.)
    ///
    /// An absolute path or one containing `..` is rejected rather than passed
    /// on, for the same reason: git would resolve it to a different file.
    ///
    /// Returns `Ok(None)` when the file is not in `HEAD`, because it is new or
    /// the branch has no commits. This is not an error; the caller marks every
    /// line as added rather than showing no marks.
    pub fn committed(&self, directory: &Path, path: &Path) -> Result<Option<String>, ScmError> {
        let Some(path) = path.to_str() else {
            // `HEAD:<path>` is a string argument, and a non-UTF-8 path cannot be
            // expressed in it. Return no content rather than wrong content.
            return Ok(None);
        };
        if !stays_in_working_tree(path) {
            return Err(ScmError::NotInWorkingTree(path.to_owned()));
        }

        // `--textconv` is intentionally not passed. A repository can configure
        // a filter that runs an arbitrary program to render a file, and the
        // gutter must not execute programs from `.gitattributes`.
        match self.run(directory, &["show", &format!("HEAD:{path}")]) {
            Ok(text) => Ok(Some(text)),
            // The three messages git uses when there is no committed text.
            // None of them is a failure. Quoted from git 2.43:
            //
            //   fatal: path 'new.rs' exists on disk, but not in 'HEAD'
            //   fatal: path 'nosuch.rs' does not exist in 'HEAD'
            //   fatal: invalid object name 'HEAD'.        (nothing committed)
            //
            // `LC_ALL=C` is set in `run` so these English messages can be
            // matched. Any unrecognised message remains an error: treating a
            // real failure as an empty file would mark every line as added,
            // while a missed match only loses the gutter.
            Err(ScmError::Refused { message, .. })
                if message.contains("exists on disk, but not in")
                    || message.contains("does not exist in")
                    || message.contains("invalid object name 'HEAD'") =>
            {
                Ok(None)
            }
            Err(other) => Err(other),
        }
    }

    /// Reads both repository states needed for one source-control diff.
    ///
    /// Staged rows compare `HEAD` with the index. Working-tree rows compare the
    /// index with the file on disk. Keeping the distinction here prevents a
    /// file that was staged and then edited again from showing the two changes
    /// as one.
    pub fn comparison(
        &self,
        directory: &Path,
        request: &ComparisonRequest,
    ) -> Result<Comparison, ScmError> {
        match request.kind {
            ComparisonKind::Staged => {
                let original = request.original.as_deref().unwrap_or(&request.path);
                Ok(Comparison {
                    original: self.committed(directory, original)?,
                    modified: self.indexed(directory, &request.path)?,
                })
            }
            ComparisonKind::WorkingTree => Ok(Comparison {
                original: self.indexed(directory, &request.path)?,
                modified: self.working(directory, &request.path)?,
            }),
        }
    }

    /// Local branches, with the one `HEAD` names marked current.
    ///
    /// Remote-tracking branches are intentionally excluded. Selecting one would
    /// also create a local branch, which is a separate action. Checkout only
    /// switches between existing local branches.
    pub fn branches(&self, directory: &Path) -> Result<Vec<Branch>, ScmError> {
        let output = self.run_bytes(
            directory,
            &[
                "for-each-ref",
                "--format=%(HEAD)%09%(refname)",
                "refs/heads",
            ],
        )?;
        let output = std::str::from_utf8(&output).map_err(|error| {
            ScmError::Unusable(format!(
                "git returned a branch name that is not UTF-8: {error}"
            ))
        })?;
        let mut branches = Vec::new();
        for line in output.lines() {
            let Some((head, reference)) = line.split_once('\t') else {
                return Err(ScmError::Unusable(
                    "git returned a malformed local branch".to_owned(),
                ));
            };
            let Some(name) = reference.strip_prefix("refs/heads/") else {
                return Err(ScmError::Unusable(format!(
                    "git returned a non-local branch `{reference}`"
                )));
            };
            branches.push(Branch {
                name: name.to_owned(),
                current: head == "*",
            });
        }
        branches.sort_by(|left, right| {
            right
                .current
                .cmp(&left.current)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(branches)
    }

    /// Describes a checkout without changing the repository.
    ///
    /// The counts come from the same status parser the source-control view
    /// uses. The plan does not guarantee that local changes carry over cleanly.
    /// The checkout uses no force flag, so Git decides and rejects the switch
    /// rather than overwriting local changes.
    pub fn checkout_plan(&self, directory: &Path, target: &str) -> Result<CheckoutPlan, ScmError> {
        self.require_local_branch(directory, target)?;
        let status = self.status(directory)?;
        if status.conflicted() > 0 {
            return Err(ScmError::Refused {
                code: None,
                message: "resolve the working tree's conflicts before switching branches"
                    .to_owned(),
            });
        }
        let branch_changes = if self.has_commit(directory) {
            let target_ref = format!("refs/heads/{target}");
            let output = self.run_bytes(
                directory,
                &["diff", "--name-only", "-z", "HEAD", &target_ref, "--"],
            )?;
            output
                .split(|byte| *byte == 0)
                .filter(|name| !name.is_empty())
                .count()
        } else {
            0
        };
        Ok(CheckoutPlan {
            current: status.head.label(),
            target: target.to_owned(),
            branch_changes,
            staged: status.staged(),
            unstaged: status.unstaged(),
            untracked: status.untracked(),
        })
    }

    /// The index contents of `path`, if it has a stage-zero entry.
    fn indexed(&self, directory: &Path, path: &Path) -> Result<Option<String>, ScmError> {
        let path = plain_path(path)?;
        match self.run(directory, &["show", &format!(":{path}")]) {
            Ok(text) => Ok(Some(text)),
            Err(ScmError::Refused { message, .. })
                if message.contains("exists on disk, but not in the index")
                    || message.contains("does not exist (neither on disk nor in the index)") =>
            {
                Ok(None)
            }
            Err(other) => Err(other),
        }
    }

    /// The working-tree contents of `path`, if the file still exists.
    fn working(&self, directory: &Path, path: &Path) -> Result<Option<String>, ScmError> {
        let path = plain_path(path)?;
        let full = directory.join(&path);
        let metadata = match std::fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(ScmError::Unusable(format!(
                    "could not inspect working-tree file {path}: {error}"
                )))
            }
        };
        if metadata.file_type().is_symlink() {
            return std::fs::read_link(&full)
                .map(|target| Some(target.as_os_str().to_string_lossy().into_owned()))
                .map_err(|error| {
                    ScmError::Unusable(format!("could not read working-tree link {path}: {error}"))
                });
        }
        match std::fs::read_to_string(full) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ScmError::Unusable(format!(
                "could not read working-tree file {path}: {error}"
            ))),
        }
    }

    /// Carries out a change to the repository.
    ///
    /// Every path is repository-relative and placed after `--`, so a file named
    /// `-f` or `HEAD` is treated as a file rather than an option or a revision.
    /// Paths are first validated as in [`Git::committed`], for the same reason:
    /// git would resolve an absolute or `..`-containing path to another file.
    pub fn apply(&self, directory: &Path, operation: &Operation) -> Result<(), ScmError> {
        let path = |path: &Path| -> Result<String, ScmError> {
            let text = path
                .to_str()
                .ok_or_else(|| ScmError::NotInWorkingTree(path.display().to_string()))?;
            if !stays_in_working_tree(text) {
                return Err(ScmError::NotInWorkingTree(text.to_owned()));
            }
            Ok(text.to_owned())
        };
        match operation {
            Operation::Stage(one) => {
                self.run(directory, &["add", "--", &path(one)?])?;
            }
            // `git add --all` without a pathspec rather than `.`, which would
            // only cover the working directory; the view lists the whole
            // repository.
            Operation::StageAll => {
                self.run(directory, &["add", "--all", "--"])?;
            }
            Operation::Unstage {
                path: one,
                original,
            } => {
                // Both paths of a staged rename must be reset together; see
                // `Operation::Unstage`.
                let mut names = vec![path(one)?];
                if let Some(original) = original {
                    names.push(path(original)?);
                }
                // `restore --staged` requires git 2.23, while `reset` works
                // with any git still in use. On a branch with no commit there is
                // no `HEAD` to reset against, so `rm --cached` is used instead.
                let mut args: Vec<&str> = match self.has_commit(directory) {
                    true => vec!["reset", "--quiet", "HEAD", "--"],
                    false => vec!["rm", "--cached", "--quiet", "--"],
                };
                args.extend(names.iter().map(String::as_str));
                self.run(directory, &args)?;
            }
            // No `-a`: a plain commit records exactly the index, which is what
            // the view shows. (`--only` restricts the commit to the given paths
            // and fails when none are given, so it is not used.)
            //
            // This runs the repository's hooks, which is one reason for running
            // the binary instead of linking a library. A `pre-commit` hook that
            // reformats or rejects is the user's choice, and deco does not skip
            // it. Hook stdin is closed and `GIT_TERMINAL_PROMPT=0` is set, so a
            // hook reading from stdin gets EOF. A hook is an arbitrary program
            // and can still open a terminal or graphical prompt; running hooks
            // means accepting that behaviour.
            Operation::Commit(message) => {
                self.run(directory, &["commit", "--message", message])?;
            }
            Operation::Checkout(target) => {
                // Checked again here instead of relying on the earlier plan:
                // another process may have deleted or replaced the branch while
                // the confirmation was shown. After the check the name is safe
                // to pass as an argument, and no shell is involved.
                self.require_local_branch(directory, target)?;
                self.run(directory, &["checkout", "--quiet", target])?;
            }
        }
        Ok(())
    }

    /// Rejects anything except the exact name of an existing local branch.
    fn require_local_branch(&self, directory: &Path, target: &str) -> Result<(), ScmError> {
        if self
            .branches(directory)?
            .iter()
            .any(|branch| branch.name == target)
        {
            Ok(())
        } else {
            Err(ScmError::Refused {
                code: None,
                message: format!("`{target}` is not a local branch"),
            })
        }
    }

    /// Whether `HEAD` names a commit. False on a branch with no commits.
    fn has_commit(&self, directory: &Path) -> bool {
        self.run(directory, &["rev-parse", "--verify", "--quiet", "HEAD"])
            .is_ok_and(|out| !out.trim().is_empty())
    }

    /// Runs git in the directory and returns its stdout as text.
    ///
    /// Used where the output is text or is ignored. Status paths are handled
    /// differently: a lossy conversion could turn a non-UTF-8 filename into
    /// another valid filename and make a write command act on the wrong file,
    /// so status uses run_bytes and rejects that case.
    fn run(&self, directory: &Path, args: &[&str]) -> Result<String, ScmError> {
        Ok(String::from_utf8_lossy(&self.run_bytes(directory, args)?).into_owned())
    }

    /// Runs git in the directory and returns its stdout unchanged.
    fn run_bytes(&self, directory: &Path, args: &[&str]) -> Result<Vec<u8>, ScmError> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(directory)
            // Not `--no-optional-locks`: an unknown flag is a hard error on an
            // older git, while an unknown environment variable is ignored.
            // Both prevent status from taking the index lock, so a status bar
            // refresh on save cannot make a `git commit` in a terminal fail.
            .env("GIT_OPTIONAL_LOCKS", "0")
            // No prompt can be answered here, so any prompt must fail
            // immediately instead of waiting.
            .env("GIT_TERMINAL_PROMPT", "0")
            // The porcelain format is not translated, but error messages are,
            // and one of them is matched below.
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
            // Matching git's English message is why `LC_ALL=C` is set above.
            // The alternative is a second process, `git rev-parse
            // --show-toplevel` before every status, to obtain information this
            // run already has.
            if message.contains("not a git repository") {
                return Err(ScmError::NotARepository(directory.to_path_buf()));
            }
            return Err(ScmError::Refused {
                code: output.status.code(),
                message,
            });
        }

        Ok(output.stdout)
    }
}

/// A repository-relative path safe to embed in Git's `revision:path` syntax.
fn plain_path(path: &Path) -> Result<String, ScmError> {
    let text = path
        .to_str()
        .ok_or_else(|| ScmError::NotInWorkingTree(path.display().to_string()))?;
    if !stays_in_working_tree(text) {
        return Err(ScmError::NotInWorkingTree(text.to_owned()));
    }
    Ok(text.to_owned())
}

/// Whether `text` names a path inside the working tree when resolved against
/// its root.
///
/// Only plain names and `.` are allowed. A root (`/etc/passwd`), a Windows
/// prefix (`C:`, `\\server\share`) or `..` would make git resolve the path to
/// another file. `Path::components` applies the platform's rules, so on Windows
/// a path such as `/etc/passwd`, which has a root but no drive and therefore is
/// not `is_absolute`, is still refused, and `\` is a separator.
fn stays_in_working_tree(text: &str) -> bool {
    !text.is_empty()
        && !text.split('/').any(|part| part == "..")
        && Path::new(text)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
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

    /// The remaining tests need a real git. CI has one, but a contributor's
    /// machine may not, so the tests skip with a message instead of failing
    /// for a reason unrelated to the change.
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
        // This crate is in deco's repository, so the directory is always a
        // working tree. Using it instead of a fixture exercises a real
        // `git status` with the contributor's configuration.
        let status = git
            .status(Path::new(env!("CARGO_MANIFEST_DIR")))
            .expect("deco's own checkout");
        assert!(
            !status.head.label().is_empty(),
            "a checkout is on a branch or at a commit; both have a label"
        );
    }

    /// A fresh repository in its own directory, removed when the test ends.
    /// Only `git init` is run, because an unborn branch is a real state worth
    /// testing against a real git.
    fn scratch_repo(git: &Git, name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deco-scm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        let status = std::process::Command::new(&git.program)
            .args(["init", "--quiet"])
            .current_dir(&dir)
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed in {}", dir.display());
        dir
    }

    #[test]
    fn every_file_in_a_new_directory_is_counted() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "untracked");
        std::fs::create_dir_all(dir.join("newdir/sub")).expect("a directory");
        for path in ["newdir/one.rs", "newdir/two.rs", "newdir/sub/three.rs"] {
            std::fs::write(dir.join(path), "//\n").expect("a file");
        }
        std::fs::write(dir.join("loose.rs"), "//\n").expect("a file");

        let status = git.status(&dir).expect("a fresh repository");
        let _ = std::fs::remove_dir_all(&dir);

        // Git's default of `--untracked-files=normal` reports one
        // `? newdir/` record for the three files inside it, which would make
        // this 2, the undercount this option prevents.
        assert_eq!(
            status.changed(),
            4,
            "a new folder's files each count: {:?}",
            status
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(status.untracked(), 4);
        assert!(
            matches!(status.head, crate::status::Head::Unborn(_)),
            "nothing has been committed, and git says so: {:?}",
            status.head
        );
    }

    // Not on macOS: APFS rejects a file name that is not valid UTF-8 with
    // `EILSEQ`, so the file this test needs cannot be created and git there
    // cannot report such a path.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_non_utf8_status_path_is_refused_before_it_can_name_another_file() {
        use std::os::unix::ffi::OsStringExt;

        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "non-utf8");
        let invalid = std::ffi::OsString::from_vec(vec![b'f', 0xff]);
        std::fs::write(dir.join(invalid), "one\n").expect("a file");

        let status = git.status(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(status, Err(ScmError::Unusable(ref reason)) if reason.contains("not UTF-8")),
            "a lossy path could collide with a real filename: {status:?}"
        );
    }

    /// Commits everything in `dir`, so there is a `HEAD` to read from.
    ///
    /// Identity is set on the repository rather than read from the machine,
    /// so the test does not fail for a contributor with no `user.email`.
    fn commit(git: &Git, dir: &Path) {
        for args in [
            &["config", "user.email", "test@example.invalid"][..],
            &["config", "user.name", "deco tests"][..],
            &["add", "-A"][..],
            &["commit", "--quiet", "-m", "fixture"][..],
        ] {
            let status = std::process::Command::new(&git.program)
                .args(args)
                .current_dir(dir)
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed");
        }
    }

    #[test]
    fn the_committed_text_is_what_a_buffer_is_compared_against() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "committed");
        std::fs::write(dir.join("a.rs"), "one\ntwo\n").expect("a file");
        commit(&git, &dir);
        // Changed on disk. The result must still be the committed `HEAD`
        // contents.
        std::fs::write(dir.join("a.rs"), "one\nEDITED\n").expect("a file");

        let head = git
            .committed(&dir, Path::new("a.rs"))
            .expect("a committed file");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(head.as_deref(), Some("one\ntwo\n"));
    }

    #[test]
    fn staged_and_working_tree_comparisons_keep_the_index_boundary() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "comparisons");
        std::fs::write(dir.join("a.rs"), "committed\n").expect("a file");
        commit(&git, &dir);
        std::fs::write(dir.join("a.rs"), "staged\n").expect("a staged edit");
        git.apply(&dir, &Operation::Stage(PathBuf::from("a.rs")))
            .expect("a stage");
        std::fs::write(dir.join("a.rs"), "working\n").expect("a later edit");

        let staged = git
            .comparison(
                &dir,
                &ComparisonRequest {
                    path: PathBuf::from("a.rs"),
                    original: None,
                    kind: ComparisonKind::Staged,
                },
            )
            .expect("the staged comparison");
        let working = git
            .comparison(
                &dir,
                &ComparisonRequest {
                    path: PathBuf::from("a.rs"),
                    original: None,
                    kind: ComparisonKind::WorkingTree,
                },
            )
            .expect("the working-tree comparison");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(staged.original.as_deref(), Some("committed\n"));
        assert_eq!(staged.modified.as_deref(), Some("staged\n"));
        assert_eq!(working.original.as_deref(), Some("staged\n"));
        assert_eq!(working.modified.as_deref(), Some("working\n"));
    }

    #[cfg(unix)]
    #[test]
    fn a_working_tree_symlink_is_compared_as_the_target_git_tracks() {
        use std::os::unix::fs::symlink;

        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "symlink-comparison");
        symlink("target-one", dir.join("link")).expect("a link");
        commit(&git, &dir);
        std::fs::remove_file(dir.join("link")).expect("remove the old link");
        symlink("target-two", dir.join("link")).expect("a changed link");

        let comparison = git
            .comparison(
                &dir,
                &ComparisonRequest {
                    path: PathBuf::from("link"),
                    original: None,
                    kind: ComparisonKind::WorkingTree,
                },
            )
            .expect("a symlink comparison");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(comparison.original.as_deref(), Some("target-one"));
        assert_eq!(comparison.modified.as_deref(), Some("target-two"));
    }

    #[test]
    fn a_file_that_is_not_committed_yet_is_absent_rather_than_an_error() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "uncommitted");
        std::fs::write(dir.join("a.rs"), "one\n").expect("a file");
        commit(&git, &dir);
        std::fs::write(dir.join("new.rs"), "fresh\n").expect("a file");

        let new = git.committed(&dir, Path::new("new.rs"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            new,
            Ok(None),
            "a new file has no committed text, which is not a failure — every \
             line of it is an addition"
        );
    }

    #[test]
    fn an_unborn_branch_has_no_committed_text_for_anything() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "unborn");
        std::fs::write(dir.join("a.rs"), "one\n").expect("a file");

        let head = git.committed(&dir, Path::new("a.rs"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            head,
            Ok(None),
            "`git init` and nothing committed: there is no HEAD to read"
        );
    }

    #[test]
    fn a_path_means_the_same_file_however_deep_git_is_run() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "subdir");
        std::fs::create_dir_all(dir.join("sub")).expect("a directory");
        std::fs::write(dir.join("a.txt"), "ROOT\n").expect("a file");
        std::fs::write(dir.join("sub/a.txt"), "SUB\n").expect("a file");
        commit(&git, &dir);

        // The same repository-relative path, requested from two depths. The
        // result must not depend on the directory deco was started in.
        let from_root = git.committed(&dir, Path::new("sub/a.txt"));
        let from_sub = git.committed(&dir.join("sub"), Path::new("sub/a.txt"));
        // The root's file, requested from the subdirectory. `HEAD:./a.txt`
        // would resolve `./` against the working directory and find
        // `sub/a.txt` instead.
        let root_file_from_sub = git.committed(&dir.join("sub"), Path::new("a.txt"));
        let _ = std::fs::remove_dir_all(&dir);

        let text = |result: Result<Option<String>, ScmError>| result.expect("a committed file");
        assert_eq!(text(from_root).as_deref(), Some("SUB\n"));
        assert_eq!(
            text(from_sub).as_deref(),
            Some("SUB\n"),
            "one path, one file, whatever directory git was run in"
        );
        assert_eq!(
            text(root_file_from_sub).as_deref(),
            Some("ROOT\n"),
            "`a.txt` is the repository's, not the subdirectory's"
        );
    }

    #[test]
    fn the_repository_root_is_where_git_says_it_is() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "toplevel");
        std::fs::create_dir_all(dir.join("sub")).expect("a directory");
        std::fs::write(dir.join("a.txt"), "one\n").expect("a file");
        commit(&git, &dir);

        let found = git.root(&dir.join("sub"));
        let _ = std::fs::remove_dir_all(&dir);
        // Canonicalised on both sides: the temporary directory is a symlink on
        // macOS, and git reports the link target.
        let found = found.expect("a repository").canonicalize().ok();
        assert_eq!(found, dir.canonicalize().ok());
    }

    #[test]
    fn staging_and_committing_do_what_they_say() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "apply");
        std::fs::write(dir.join("a.rs"), "one\n").expect("a file");
        commit(&git, &dir);
        std::fs::write(dir.join("a.rs"), "two\n").expect("a file");
        std::fs::write(dir.join("new.rs"), "fresh\n").expect("a file");

        // Stage one file. The other stays untracked.
        git.apply(&dir, &Operation::Stage(PathBuf::from("a.rs")))
            .expect("a stage");
        let status = git.status(&dir).expect("a status");
        assert_eq!(status.staged(), 1);
        assert_eq!(status.untracked(), 1);

        // Unstage it again, leaving the working tree unchanged.
        git.apply(
            &dir,
            &Operation::Unstage {
                path: PathBuf::from("a.rs"),
                original: None,
            },
        )
        .expect("an unstage");
        let status = git.status(&dir).expect("a status");
        assert_eq!(status.staged(), 0);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).expect("still there"),
            "two\n",
            "unstaging touches the index, never the file"
        );

        // Stage everything, then commit.
        git.apply(&dir, &Operation::StageAll).expect("a stage");
        assert_eq!(git.status(&dir).expect("a status").staged(), 2);
        git.apply(&dir, &Operation::Commit("a message".to_owned()))
            .expect("a commit");

        let status = git.status(&dir).expect("a status");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status.is_clean(), "everything was recorded");
    }

    #[test]
    fn checkout_is_previewed_and_keeps_unrelated_local_work() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "checkout");
        std::fs::write(dir.join("a.rs"), "main\n").expect("a file");
        std::fs::write(dir.join("kept.rs"), "clean\n").expect("a file");
        commit(&git, &dir);
        let current = git
            .branches(&dir)
            .expect("branches")
            .into_iter()
            .find(|branch| branch.current)
            .expect("a current branch")
            .name;

        let made = std::process::Command::new(&git.program)
            .args(["branch", "feature/checkout"])
            .current_dir(&dir)
            .status()
            .expect("git branch");
        assert!(made.success());
        git.apply(&dir, &Operation::Checkout("feature/checkout".to_owned()))
            .expect("switch to the feature branch");
        std::fs::write(dir.join("a.rs"), "feature\n").expect("an edit");
        std::fs::write(dir.join("new.rs"), "new\n").expect("a file");
        commit(&git, &dir);
        git.apply(&dir, &Operation::Checkout(current.clone()))
            .expect("switch back");

        std::fs::write(dir.join("kept.rs"), "local\n").expect("an unstaged edit");
        std::fs::write(dir.join("note.txt"), "untracked\n").expect("an untracked file");
        let plan = git
            .checkout_plan(&dir, "feature/checkout")
            .expect("a preview");
        assert_eq!(plan.current, current);
        assert_eq!(plan.target, "feature/checkout");
        assert_eq!(plan.branch_changes, 2);
        assert_eq!((plan.staged, plan.unstaged, plan.untracked), (0, 1, 1));

        git.apply(&dir, &Operation::Checkout("feature/checkout".to_owned()))
            .expect("the unrelated local work is carried");
        assert_eq!(
            std::fs::read_to_string(dir.join("kept.rs")).expect("the local edit"),
            "local\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).expect("the branch version"),
            "feature\n"
        );
        let branches = git.branches(&dir).expect("branches after switching");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(branches
            .iter()
            .any(|branch| { branch.current && branch.name == "feature/checkout" }));
    }

    #[test]
    fn checkout_refuses_a_name_that_is_not_a_local_branch() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "checkout-invalid");
        std::fs::write(dir.join("a.rs"), "one\n").expect("a file");
        commit(&git, &dir);

        let result = git.apply(&dir, &Operation::Checkout("--detach".to_owned()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(result, Err(ScmError::Refused { ref message, .. }) if message.contains("not a local branch")),
            "an option-shaped target was refused before checkout: {result:?}"
        );
    }

    #[test]
    fn unstaging_a_rename_takes_both_halves_back() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "rename-unstage");
        std::fs::create_dir_all(dir.join("old")).expect("a directory");
        std::fs::create_dir_all(dir.join("new")).expect("a directory");
        // Long enough for git's rename detection to match reliably.
        std::fs::write(dir.join("old/name.rs"), "a line long enough to match\n").expect("a file");
        commit(&git, &dir);
        let moved = std::process::Command::new(&git.program)
            .args(["mv", "old/name.rs", "new/name.rs"])
            .current_dir(&dir)
            .status()
            .expect("git mv");
        assert!(moved.success());

        // Resetting only the new path would leave `old/name.rs` staged as a
        // deletion and `new/name.rs` untracked, so the next commit would still
        // delete the original file.
        git.apply(
            &dir,
            &Operation::Unstage {
                path: PathBuf::from("new/name.rs"),
                original: Some(PathBuf::from("old/name.rs")),
            },
        )
        .expect("an unstage");

        let status = git.status(&dir).expect("a status");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            status.staged(),
            0,
            "the index is back to what HEAD has: {:?}",
            status.entries
        );
    }

    #[test]
    fn unstaging_a_copy_leaves_its_independently_changed_source_staged() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "copy-unstage");
        std::fs::write(dir.join("source.rs"), "original\n").expect("a file");
        commit(&git, &dir);

        // Both the source's edit and a copy of that edit are staged. With copy
        // detection enabled git can report the latter with source.rs as its
        // original path, but that path is not the other half of a rename.
        std::fs::write(dir.join("source.rs"), "changed\n").expect("an edit");
        std::fs::copy(dir.join("source.rs"), dir.join("copy.rs")).expect("a copy");
        git.apply(&dir, &Operation::StageAll).expect("a stage");
        assert_eq!(git.status(&dir).expect("a status").staged(), 2);

        git.apply(
            &dir,
            &Operation::Unstage {
                path: PathBuf::from("copy.rs"),
                original: None,
            },
        )
        .expect("an unstage");

        let status = git.status(&dir).expect("a status");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            status.staged(),
            1,
            "only the selected copy leaves the index: {:?}",
            status.entries
        );
        assert!(
            status.entries.iter().any(|entry| {
                entry.path == Path::new("source.rs")
                    && matches!(
                        &entry.state,
                        crate::State::Tracked {
                            staged: crate::Change::Modified,
                            ..
                        }
                    )
            }),
            "the source's independent staged edit remains"
        );
    }

    #[test]
    fn unstaging_works_on_a_branch_with_no_commit_yet() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "unborn-unstage");
        std::fs::write(dir.join("a.rs"), "one\n").expect("a file");
        git.apply(&dir, &Operation::StageAll).expect("a stage");
        assert_eq!(git.status(&dir).expect("a status").staged(), 1);

        // `git reset HEAD` has no HEAD to reset against here, so this case uses
        // a different command instead of failing.
        let undone = git.apply(
            &dir,
            &Operation::Unstage {
                path: PathBuf::from("a.rs"),
                original: None,
            },
        );
        let status = git.status(&dir).expect("a status");
        let _ = std::fs::remove_dir_all(&dir);
        undone.expect("an unstage");
        assert_eq!(status.staged(), 0);
        assert_eq!(status.untracked(), 1, "and the file itself is still there");
    }

    #[test]
    fn a_change_to_a_path_that_could_escape_is_refused() {
        let git = Git::default();
        for bad in ["/etc/passwd", "../secrets.rs", "a/../../b.rs", ""] {
            let path = PathBuf::from(bad);
            assert!(
                matches!(
                    git.apply(Path::new("."), &Operation::Stage(path.clone())),
                    Err(ScmError::NotInWorkingTree(_))
                ),
                "staging {bad:?} was passed to git rather than refused"
            );
            assert!(matches!(
                git.apply(
                    Path::new("."),
                    &Operation::Unstage {
                        path,
                        original: None
                    }
                ),
                Err(ScmError::NotInWorkingTree(_))
            ));
        }
    }

    #[test]
    fn only_plain_relative_paths_stay_in_the_working_tree() {
        for good in ["a.rs", "src/main.rs", "./src/main.rs", "a..b/c"] {
            assert!(stays_in_working_tree(good), "{good:?}");
        }
        for bad in ["", "/etc/passwd", "../x", "a/../../b", "src/.."] {
            assert!(!stays_in_working_tree(bad), "{bad:?}");
        }
        // Windows forms: a drive, a drive-relative path, a UNC share, and `..`
        // written with the other separator. On Unix these are ordinary names.
        if cfg!(windows) {
            for bad in [r"C:\x", "C:x", r"\\server\share\x", r"a\..\..\b", r"\etc"] {
                assert!(!stays_in_working_tree(bad), "{bad:?}");
            }
        }
    }

    #[test]
    fn a_path_that_could_escape_the_working_tree_is_refused() {
        let git = Git::default();
        // These never reach git. `HEAD:/etc/passwd` and `HEAD:../secrets`
        // resolve against the working directory, so the caller would receive
        // another file's committed contents.
        for bad in ["/etc/passwd", "../secrets.rs", "a/../../b.rs", ""] {
            assert!(
                matches!(
                    git.committed(Path::new("."), Path::new(bad)),
                    Err(ScmError::NotInWorkingTree(_))
                ),
                "{bad:?} was passed to git rather than refused"
            );
        }
    }

    #[test]
    fn a_folder_outside_a_repository_says_so() {
        let Some(git) = git_or_skip() else { return };
        // The temporary directory is not inside deco's checkout. Nothing is
        // created in it, so the test does not depend on its contents.
        let outside = std::env::temp_dir();
        match git.status(&outside) {
            Err(ScmError::NotARepository(path)) => assert_eq!(path, outside),
            // `TMPDIR` is inside a repository. This is unusual but valid, so
            // the test is skipped.
            Ok(_) => eprintln!("skipped: {} is inside a working tree", outside.display()),
            Err(other) => panic!("expected a plain refusal, got {other}"),
        }
    }
}
