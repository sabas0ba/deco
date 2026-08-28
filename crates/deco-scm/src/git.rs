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

use crate::change::Operation;
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
    /// A path that was not a plain name inside the working tree.
    ///
    /// Refused rather than passed to git: `HEAD:<path>` resolves an absolute
    /// or `..`-bearing path against the working directory, so a caller that
    /// handed one over would silently be shown a different file's contents.
    #[error("`{0}` is not a path inside the working tree")]
    NotInWorkingTree(String),
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
        // `--untracked-files=all` rather than git's default of `normal`, which
        // collapses a new directory into a single `? newdir/` record. The
        // count is documented as one per *file*, and under `normal` a folder
        // someone just added with a dozen files in it would read as `±1` — an
        // undercount in the quiet direction, on one of the commonest things a
        // person does.
        //
        // It is also named rather than left to the default because
        // `status.showUntrackedFiles` can change that default out from under
        // this, and what the bar counts should not depend on a setting deco
        // does not read.
        //
        // What it costs: `all` descends into untracked directories, so a
        // working tree with a large one that is *not* in `.gitignore` is
        // slower to ask. Ignored files are still left out — nothing here
        // passes `--ignored` — so the usual `target/` and `node_modules/` are
        // not what is being walked.
        let output = self.run(
            directory,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=all",
            ],
        )?;
        Ok(status::parse(&output)?)
    }

    /// Where the repository containing `directory` begins.
    ///
    /// Needed because every path git reports is relative to *this*, not to the
    /// folder deco was started in — and those differ whenever someone opens a
    /// subdirectory of a repository, which is an ordinary thing to do. Asking
    /// once and keeping the answer is what lets [`Git::committed`] and
    /// [`Status`] speak the same coordinates.
    pub fn root(&self, directory: &Path) -> Result<PathBuf, ScmError> {
        let output = self.run(directory, &["rev-parse", "--show-toplevel"])?;
        let root = output.trim_end_matches(['\n', '\r']);
        if root.is_empty() {
            return Err(ScmError::NotARepository(directory.to_path_buf()));
        }
        Ok(PathBuf::from(root))
    }

    /// The committed text of a file, to compare a buffer against.
    ///
    /// `path` is relative to the **repository root** — the same coordinates
    /// [`Status`] reports in, and what [`Git::root`] is for. Not relative to
    /// `directory`: `git show HEAD:a` reads the repository's `a` however deep
    /// in the tree it is run from, which is the property that makes one path
    /// mean one file. (`HEAD:./a` would be the other thing, resolved against
    /// the working directory — the two disagree the moment a workspace is a
    /// subdirectory, and a gutter drawn from the wrong blob looks exactly like
    /// a gutter drawn from the right one.)
    ///
    /// An absolute path or one containing `..` is refused rather than passed
    /// on, for the same reason: git would resolve it somewhere else entirely.
    ///
    /// `Ok(None)` when the file is not in `HEAD`: it is new, or on an unborn
    /// branch where nothing is. That is not an error, and the caller's answer
    /// to it is "every line is an addition" rather than "no marks".
    pub fn committed(&self, directory: &Path, path: &Path) -> Result<Option<String>, ScmError> {
        let Some(path) = path.to_str() else {
            // `HEAD:<path>` is a string to git, and a path that is not UTF-8
            // cannot be spelled in one. Nothing to show rather than something
            // wrong.
            return Ok(None);
        };
        if path.is_empty()
            || Path::new(path).is_absolute()
            || path.split('/').any(|part| part == "..")
        {
            return Err(ScmError::NotInWorkingTree(path.to_owned()));
        }

        // `--textconv` is deliberately *not* passed: a repository can configure
        // a filter that runs an arbitrary program to render a file, and the
        // gutter is not worth executing someone's `.gitattributes` for.
        match self.run(directory, &["show", &format!("HEAD:{path}")]) {
            Ok(text) => Ok(Some(text)),
            // The three ways git says "there is no committed text for this",
            // none of them a failure. Quoted from git 2.43 rather than guessed:
            //
            //   fatal: path 'new.rs' exists on disk, but not in 'HEAD'
            //   fatal: path 'nosuch.rs' does not exist in 'HEAD'
            //   fatal: invalid object name 'HEAD'.        (nothing committed)
            //
            // Matching English is why `LC_ALL=C` is set in `run`. Getting this
            // wrong in the safe direction costs a gutter; the unsafe direction
            // would be treating a real failure as an empty file and drawing
            // every line as an addition, so anything unrecognised stays an
            // error.
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

    /// Carries out a change to the repository.
    ///
    /// Every path is repository-relative and goes after `--`, so a file called
    /// `-f` or `HEAD` is a file rather than an option or a revision. The same
    /// check [`Git::committed`] makes is made first, for the same reason: git
    /// would resolve an absolute or `..`-bearing path somewhere else.
    pub fn apply(&self, directory: &Path, operation: &Operation) -> Result<(), ScmError> {
        let path = |path: &Path| -> Result<String, ScmError> {
            let text = path
                .to_str()
                .ok_or_else(|| ScmError::NotInWorkingTree(path.display().to_string()))?;
            if text.is_empty()
                || Path::new(text).is_absolute()
                || text.split('/').any(|part| part == "..")
            {
                return Err(ScmError::NotInWorkingTree(text.to_owned()));
            }
            Ok(text.to_owned())
        };
        match operation {
            Operation::Stage(one) => {
                self.run(directory, &["add", "--", &path(one)?])?;
            }
            // `git add -A` from the repository root rather than `.`, which
            // would only reach what is below the working directory — and the
            // view lists the whole repository.
            Operation::StageAll => {
                self.run(directory, &["add", "--all", "--"])?;
            }
            Operation::Unstage {
                path: one,
                original,
            } => {
                // Both halves of a staged rename, or it comes apart: see
                // `Operation::Unstage`.
                let mut names = vec![path(one)?];
                if let Some(original) = original {
                    names.push(path(original)?);
                }
                // `restore --staged` needs git 2.23. `reset` is older than
                // anything still installed, and on a branch with no commit yet
                // there is no `HEAD` to reset against — so that case takes the
                // one command that works there.
                let mut args: Vec<&str> = match self.has_commit(directory) {
                    true => vec!["reset", "--quiet", "HEAD", "--"],
                    false => vec!["rm", "--cached", "--quiet", "--"],
                };
                args.extend(names.iter().map(String::as_str));
                self.run(directory, &args)?;
            }
            // No `-a`: a plain commit records exactly the index, which is
            // exactly what the view was showing. (`--only` would be the way to
            // say that explicitly, but it means "these paths only" and refuses
            // when given none.)
            //
            // This runs the repository's hooks, which is the point of shelling
            // out rather than linking a library — a `pre-commit` that reformats
            // or refuses is the user's, and deco is not going to be the editor
            // that quietly skips it. Their stdin is closed and
            // `GIT_TERMINAL_PROMPT=0` is set, so one that decides to ask a
            // question fails rather than hanging with the editor holding its
            // pipes.
            Operation::Commit(message) => {
                self.run(directory, &["commit", "--message", message])?;
            }
        }
        Ok(())
    }

    /// Whether `HEAD` names a commit — false on a branch with nothing on it.
    fn has_commit(&self, directory: &Path) -> bool {
        self.run(directory, &["rev-parse", "--verify", "--quiet", "HEAD"])
            .is_ok_and(|out| !out.trim().is_empty())
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

    /// A fresh repository in a directory of its own, removed when the test
    /// ends. `git init` and nothing else: an unborn branch is a real state,
    /// and one worth exercising against a real git.
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
        // `? newdir/` record for the three inside it, which would make this
        // 2 — an undercount of exactly the kind the count is supposed to
        // rule out.
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

    /// Commits everything in `dir`, so there is a `HEAD` to read from.
    ///
    /// Identity is set on the repository rather than read from the machine:
    /// a contributor with no `user.email` configured would otherwise have this
    /// fail for a reason that is not their change.
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
        // Changed on disk *and* further in the buffer. Neither should reach
        // the answer: `HEAD` is what was committed.
        std::fs::write(dir.join("a.rs"), "one\nEDITED\n").expect("a file");

        let head = git
            .committed(&dir, Path::new("a.rs"))
            .expect("a committed file");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(head.as_deref(), Some("one\ntwo\n"));
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

        // The same repository-relative path, asked from two depths. Opening a
        // subdirectory of a repository is an ordinary thing to do, and the
        // answer must not depend on where deco happened to be started.
        let from_root = git.committed(&dir, Path::new("sub/a.txt"));
        let from_sub = git.committed(&dir.join("sub"), Path::new("sub/a.txt"));
        // And the root's own file, from inside the subdirectory — the case
        // that `HEAD:./a.txt` gets wrong, because `./` is resolved against the
        // working directory and would find `sub/a.txt` instead.
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
        // Canonicalised on both sides: a temporary directory is a symlink on
        // macOS, and git reports where the link goes.
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

        // Stage one file. The other stays where it was.
        git.apply(&dir, &Operation::Stage(PathBuf::from("a.rs")))
            .expect("a stage");
        let status = git.status(&dir).expect("a status");
        assert_eq!(status.staged(), 1);
        assert_eq!(status.untracked(), 1);

        // Unstage it again, leaving the working tree alone.
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

        // Everything, then a commit.
        git.apply(&dir, &Operation::StageAll).expect("a stage");
        assert_eq!(git.status(&dir).expect("a status").staged(), 2);
        git.apply(&dir, &Operation::Commit("a message".to_owned()))
            .expect("a commit");

        let status = git.status(&dir).expect("a status");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status.is_clean(), "everything was recorded");
    }

    #[test]
    fn unstaging_a_rename_takes_both_halves_back() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "rename-unstage");
        std::fs::create_dir_all(dir.join("old")).expect("a directory");
        std::fs::create_dir_all(dir.join("new")).expect("a directory");
        // Long enough that git's rename detection is in no doubt.
        std::fs::write(dir.join("old/name.rs"), "a line long enough to match\n").expect("a file");
        commit(&git, &dir);
        let moved = std::process::Command::new(&git.program)
            .args(["mv", "old/name.rs", "new/name.rs"])
            .current_dir(&dir)
            .status()
            .expect("git mv");
        assert!(moved.success());

        // Only the new path would leave `old/name.rs` staged as a *deletion*
        // and `new/name.rs` untracked — so a command reporting that it
        // unstaged the rename would have left a commit that still deletes the
        // original file.
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
    fn unstaging_works_on_a_branch_with_no_commit_yet() {
        let Some(git) = git_or_skip() else { return };
        let dir = scratch_repo(&git, "unborn-unstage");
        std::fs::write(dir.join("a.rs"), "one\n").expect("a file");
        git.apply(&dir, &Operation::StageAll).expect("a stage");
        assert_eq!(git.status(&dir).expect("a status").staged(), 1);

        // `git reset HEAD` has no HEAD to reset against here, which is why
        // this case takes a different command rather than reporting a failure
        // the user can do nothing about.
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
    fn a_path_that_could_escape_the_working_tree_is_refused() {
        let git = Git::default();
        // Never reaches git: `HEAD:/etc/passwd` and `HEAD:../secrets` resolve
        // against the working directory, so a caller handing one over would be
        // shown a different file and told it was this one's history.
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
