//! What `git status --porcelain=v2` said, and the parser that reads it.
//!
//! Nothing here spawns anything. The parser takes a string and returns a
//! [`Status`], so every shape git can produce — detached head, an unborn
//! branch, a rename, a merge conflict, a path with a space in it — is a test
//! with a string literal in it rather than a repository CI has to build.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Output that was not the format `--porcelain=v2` promises.
///
/// Kept as one variant with a description rather than a case per field: this
/// is a contract git has kept since 2.11, so a mismatch means the assumption
/// that `git` is git is wrong, and the only useful thing to do is say which
/// line was not understood.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`git status` said something this does not understand: {0}")]
pub struct Malformed(pub String);

/// Where `HEAD` is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Head {
    /// On a branch, with at least one commit on it.
    Branch(String),
    /// On a branch that does not exist yet: `git init` and nothing committed.
    ///
    /// Worth its own variant rather than folding into [`Head::Branch`] because
    /// there is no commit to compare against — every tracked-file question has
    /// the answer "there are no tracked files", and a caller that offers to
    /// show a diff should not.
    Unborn(String),
    /// Not on a branch. The full commit id; shorten it for display with
    /// [`Head::label`].
    Detached(String),
}

impl Head {
    /// What to put in a status bar.
    ///
    /// A detached head is shortened to seven characters, which is what `git`
    /// itself abbreviates to by default. Not a prefix of the *branch* name,
    /// which is the user's word and is shown whole.
    pub fn label(&self) -> String {
        match self {
            Self::Branch(name) | Self::Unborn(name) => name.clone(),
            Self::Detached(commit) => commit.chars().take(7).collect(),
        }
    }
}

/// The branch this one is set to track, and how far apart they are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// Its name, as git prints it — `origin/main`.
    pub name: String,
    /// Commits here that are not there.
    pub ahead: usize,
    /// Commits there that are not here.
    pub behind: usize,
}

/// What happened to one file, on one side.
///
/// `git` reports two of these per tracked entry: what the index has staged
/// relative to `HEAD`, and what the working tree has relative to the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    /// Nothing on this side. Git's `.`.
    None,
    /// Contents differ.
    Modified,
    /// A regular file became a symlink, or the like. Git's `T`.
    ///
    /// Distinct from [`Change::Modified`] because the contents may be
    /// identical and the thing is still not what it was.
    TypeChanged,
    /// New to the index.
    Added,
    /// Gone.
    Deleted,
    /// Moved, with the old name in [`FileStatus::original`].
    Renamed,
    /// Copied from the path in [`FileStatus::original`].
    Copied,
}

impl Change {
    /// Reads one half of git's `XY` field.
    fn from_code(code: char) -> Option<Self> {
        Some(match code {
            '.' => Self::None,
            'M' => Self::Modified,
            'T' => Self::TypeChanged,
            'A' => Self::Added,
            'D' => Self::Deleted,
            'R' => Self::Renamed,
            'C' => Self::Copied,
            _ => return None,
        })
    }

    /// Whether this side has anything to report.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Why a file is in the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Git is tracking it and something differs.
    ///
    /// At least one of the two is not [`Change::None`] — an entry with nothing
    /// on either side is not reported at all.
    Tracked {
        /// The index against `HEAD`: what a commit would record.
        staged: Change,
        /// The working tree against the index: what a `git add` would stage.
        worktree: Change,
    },
    /// Not in the index at all.
    Untracked,
    /// A merge left it with conflicts, and neither side is the answer.
    Conflicted,
}

/// One line of `git status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStatus {
    /// Where it is now, relative to the repository root.
    pub path: PathBuf,
    /// Where it was, for a rename or a copy.
    pub original: Option<PathBuf>,
    /// Why it is listed.
    pub state: State,
}

/// Everything one `git status` run reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// Where `HEAD` is.
    pub head: Head,
    /// The commit `HEAD` names, or `None` on a branch with nothing on it yet.
    ///
    /// Kept even for a named branch, where it is not shown anywhere: it is the
    /// only thing here that changes when someone commits, and a caller holding
    /// the *committed text* of a file needs to know when to throw that away.
    /// Without it a commit made in a terminal would leave every gutter drawing
    /// against the wrong version until the file was closed.
    pub commit: Option<String>,
    /// The tracked branch, when there is one.
    pub upstream: Option<Upstream>,
    /// Every file git had something to say about.
    ///
    /// Ignored files are not in here: the run asks for the default, which
    /// leaves them out, and a list dominated by `target/` would be useless.
    pub entries: Vec<FileStatus>,
}

impl Status {
    /// How many files differ from `HEAD` in any way, untracked ones included.
    ///
    /// One per *file*, not one per side: a file that is both staged and
    /// modified since is one thing the user has to think about, and counting
    /// it twice would make the status bar disagree with the list.
    pub fn changed(&self) -> usize {
        self.entries.len()
    }

    /// How many have something staged.
    pub fn staged(&self) -> usize {
        self.entries
            .iter()
            .filter(
                |entry| matches!(&entry.state, State::Tracked { staged, .. } if !staged.is_none()),
            )
            .count()
    }

    /// How many git has never been told about.
    pub fn untracked(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.state == State::Untracked)
            .count()
    }

    /// How many a merge left unresolved.
    pub fn conflicted(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.state == State::Conflicted)
            .count()
    }

    /// Nothing to commit and nothing lying around.
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    /// The one-line form for a status bar.
    ///
    /// Lives here rather than in a renderer because it is a decision about
    /// what to say, not about how to paint it. The GPU frontend has no status
    /// bar yet; when it grows one, this is what it will show, and the two will
    /// not have to be kept in step by hand.
    ///
    /// Markers rather than words, and each omitted at zero — the same bargain
    /// the problem tallies make, for the same reason: a permanent `0 changed`
    /// is noise, and the absence of the marker is the signal.
    ///
    /// - `±4` — four files differ from `HEAD`.
    /// - `↑2 ↓1` — two commits to push, one to pull.
    /// - `!2` — two files a merge left conflicted, which is the one thing here
    ///   that has to be dealt with before anything else works.
    pub fn summary(&self) -> String {
        let mut out = self.head.label();
        if !self.is_clean() {
            out.push_str(&format!(" ±{}", self.changed()));
        }
        if let Some(upstream) = &self.upstream {
            if upstream.ahead > 0 {
                out.push_str(&format!(" ↑{}", upstream.ahead));
            }
            if upstream.behind > 0 {
                out.push_str(&format!(" ↓{}", upstream.behind));
            }
        }
        let conflicted = self.conflicted();
        if conflicted > 0 {
            out.push_str(&format!(" !{conflicted}"));
        }
        out
    }
}

/// Reads `git status --porcelain=v2 --branch -z`.
///
/// `-z` is not an optimisation. Without it git C-quotes any path with a space,
/// a quote or a non-ASCII byte in it, and a rename's two paths are separated by
/// a tab that a path may legally contain — so a parser would have to undo git's
/// quoting exactly, and get it wrong for the files most likely to expose it.
/// With `-z` every field ends at a NUL and there is no quoting at all.
///
/// The records, from git's documentation:
///
/// ```text
/// # branch.oid <commit> | (initial)
/// # branch.head <branch> | (detached)
/// # branch.upstream <upstream>
/// # branch.ab +<ahead> -<behind>
/// 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
/// 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\0<origPath>
/// u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
/// ? <path>
/// ! <path>
/// ```
pub fn parse(output: &str) -> Result<Status, Malformed> {
    // A NUL after the last record means the final split is empty; a run with
    // no records at all is the empty string. Neither is a record.
    let mut records = output.split('\0').filter(|record| !record.is_empty());

    let mut oid = None;
    let mut head = None;
    let mut upstream_name = None;
    let mut ahead_behind = None;
    let mut entries = Vec::new();

    while let Some(record) = records.next() {
        // Split off the tag rather than matching whole prefixes: `?` and `!`
        // carry a path that may start with anything at all.
        let (tag, rest) = match record.split_once(' ') {
            Some(split) => split,
            // A record with no space is not one of the five shapes.
            None => return Err(Malformed(record.to_owned())),
        };
        match tag {
            "#" => {
                let (key, value) = rest
                    .split_once(' ')
                    .ok_or_else(|| Malformed(record.into()))?;
                match key {
                    "branch.oid" => oid = Some(value.to_owned()),
                    "branch.head" => head = Some(value.to_owned()),
                    "branch.upstream" => upstream_name = Some(value.to_owned()),
                    "branch.ab" => ahead_behind = Some(parse_ab(value, record)?),
                    // `# stash <n>` and anything git adds later. Skipping is
                    // deliberate: a new header must not turn a working status
                    // bar into an error message.
                    _ => {}
                }
            }
            "1" => entries.push(tracked(rest, record, 7)?),
            "2" => {
                let mut entry = tracked(rest, record, 8)?;
                // The one record that spans two: git writes the new path, a
                // NUL, then the old one.
                let original = records.next().ok_or_else(|| Malformed(record.into()))?;
                entry.original = Some(PathBuf::from(original));
                entries.push(entry);
            }
            "u" => {
                // Ten fields before the path, and the two-letter code says
                // *how* it conflicted, which the list does not use: a file
                // needing a human is a file needing a human.
                let path = field_after(rest, 9).ok_or_else(|| Malformed(record.into()))?;
                entries.push(FileStatus {
                    path: PathBuf::from(path),
                    original: None,
                    state: State::Conflicted,
                });
            }
            "?" => entries.push(FileStatus {
                path: PathBuf::from(rest),
                original: None,
                state: State::Untracked,
            }),
            // Ignored files are only reported when they are asked for, and
            // they are not asked for. Handled rather than rejected so that
            // turning the flag on later is a change of one call site.
            "!" => {}
            _ => return Err(Malformed(record.to_owned())),
        }
    }

    let oid = oid.ok_or_else(|| Malformed("no `# branch.oid` header".into()))?;
    let head = head.ok_or_else(|| Malformed("no `# branch.head` header".into()))?;
    // `(initial)` is git saying there is no commit, not a commit called that.
    let commit = (oid != "(initial)").then(|| oid.clone());
    let head = if head == "(detached)" {
        Head::Detached(oid)
    } else if commit.is_none() {
        Head::Unborn(head)
    } else {
        Head::Branch(head)
    };

    // `branch.ab` is only written when there is an upstream, so a name without
    // counts means git said nothing about the distance — zero is the honest
    // reading, and it is what git itself reports for a branch level with its
    // upstream.
    let upstream = upstream_name.map(|name| {
        let (ahead, behind) = ahead_behind.unwrap_or((0, 0));
        Upstream {
            name,
            ahead,
            behind,
        }
    });

    Ok(Status {
        head,
        commit,
        upstream,
        entries,
    })
}

/// Reads `+<ahead> -<behind>`.
fn parse_ab(value: &str, record: &str) -> Result<(usize, usize), Malformed> {
    let (ahead, behind) = value
        .split_once(' ')
        .ok_or_else(|| Malformed(record.to_owned()))?;
    let ahead = ahead
        .strip_prefix('+')
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| Malformed(record.to_owned()))?;
    let behind = behind
        .strip_prefix('-')
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| Malformed(record.to_owned()))?;
    Ok((ahead, behind))
}

/// A `1` or `2` record: the same leading fields, a different count of them.
///
/// `before` is how many fields come between the tag and the path — seven for
/// an ordinary change, eight for a rename or copy, whose extra field is the
/// similarity score.
fn tracked(rest: &str, record: &str, before: usize) -> Result<FileStatus, Malformed> {
    let xy = rest.split(' ').next().unwrap_or_default();
    let mut codes = xy.chars();
    let staged = codes
        .next()
        .and_then(Change::from_code)
        .ok_or_else(|| Malformed(record.to_owned()))?;
    let worktree = codes
        .next()
        .and_then(Change::from_code)
        .ok_or_else(|| Malformed(record.to_owned()))?;
    if codes.next().is_some() {
        return Err(Malformed(record.to_owned()));
    }
    let path = field_after(rest, before).ok_or_else(|| Malformed(record.to_owned()))?;
    Ok(FileStatus {
        path: PathBuf::from(path),
        original: None,
        state: State::Tracked { staged, worktree },
    })
}

/// Everything after the first `count` space-separated fields.
///
/// The path is the rest of the record rather than the next field: with `-z`
/// nothing quotes it, so a name with spaces in it arrives with its spaces. An
/// empty tail is `None` — a record that names no path is malformed, not a
/// record about a file called "".
fn field_after(rest: &str, count: usize) -> Option<&str> {
    let mut remaining = rest;
    for _ in 0..count {
        remaining = remaining.split_once(' ')?.1;
    }
    (!remaining.is_empty()).then_some(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the NUL-terminated form git actually writes, so the tests read
    /// as the records they are rather than as escape sequences.
    fn output(records: &[&str]) -> String {
        records
            .iter()
            .map(|record| format!("{record}\0"))
            .collect::<String>()
    }

    #[test]
    fn a_clean_branch_with_an_upstream() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +0 -0",
        ]))
        .expect("git's own format");

        assert_eq!(status.head, Head::Branch("main".into()));
        assert_eq!(
            status.upstream,
            Some(Upstream {
                name: "origin/main".into(),
                ahead: 0,
                behind: 0,
            })
        );
        assert!(status.is_clean());
        assert_eq!(
            status.summary(),
            "main",
            "nothing to say is said with nothing"
        );
    }

    #[test]
    fn ahead_and_behind_reach_the_summary() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
        ]))
        .expect("git's own format");
        assert_eq!(status.summary(), "main ↑2 ↓1");
    }

    #[test]
    fn a_detached_head_is_shown_as_a_short_commit() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d",
            "# branch.head (detached)",
        ]))
        .expect("git's own format");

        assert_eq!(
            status.head,
            Head::Detached("1c9d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d".into()),
            "the whole id is kept; shortening is a display decision"
        );
        assert_eq!(status.summary(), "1c9d4e5");
        assert_eq!(status.upstream, None);
    }

    #[test]
    fn a_branch_with_no_commit_yet_is_not_a_branch_with_one() {
        let status = parse(&output(&[
            "# branch.oid (initial)",
            "# branch.head main",
            "? README.md",
        ]))
        .expect("git's own format");

        assert_eq!(
            status.head,
            Head::Unborn("main".into()),
            "there is no commit to diff against, and a caller may need to know"
        );
        assert_eq!(status.summary(), "main ±1");
    }

    #[test]
    fn both_halves_of_a_tracked_change_are_kept() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5",
            "# branch.head main",
            "1 M. N... 100644 100644 100644 aaaaaaa bbbbbbb staged.rs",
            "1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb unstaged.rs",
            "1 MM N... 100644 100644 100644 aaaaaaa bbbbbbb both.rs",
            "1 D. N... 100644 000000 000000 aaaaaaa 0000000 gone.rs",
        ]))
        .expect("git's own format");

        let states: Vec<&State> = status.entries.iter().map(|entry| &entry.state).collect();
        assert_eq!(
            states,
            vec![
                &State::Tracked {
                    staged: Change::Modified,
                    worktree: Change::None
                },
                &State::Tracked {
                    staged: Change::None,
                    worktree: Change::Modified
                },
                &State::Tracked {
                    staged: Change::Modified,
                    worktree: Change::Modified
                },
                &State::Tracked {
                    staged: Change::Deleted,
                    worktree: Change::None
                },
            ]
        );
        assert_eq!(status.staged(), 3);
        assert_eq!(
            status.changed(),
            4,
            "`both.rs` is one file to think about, not two"
        );
    }

    #[test]
    fn a_rename_carries_the_name_it_had() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5",
            "# branch.head main",
            "2 R. N... 100644 100644 100644 aaaaaaa bbbbbbb R100 new/name.rs",
            "old/name.rs",
            "1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb after.rs",
        ]))
        .expect("git's own format");

        assert_eq!(
            status.entries.len(),
            2,
            "the old path is a field, not a file"
        );
        assert_eq!(status.entries[0].path, PathBuf::from("new/name.rs"));
        assert_eq!(
            status.entries[0].original,
            Some(PathBuf::from("old/name.rs"))
        );
        assert_eq!(
            status.entries[1].path,
            PathBuf::from("after.rs"),
            "and the record after a rename is read as a record"
        );
    }

    #[test]
    fn a_conflict_is_neither_staged_nor_unstaged() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5",
            "# branch.head main",
            "u UU N... 100644 100644 100644 100644 aaaaaaa bbbbbbb ccccccc clash.rs",
        ]))
        .expect("git's own format");

        assert_eq!(status.entries[0].state, State::Conflicted);
        assert_eq!(status.entries[0].path, PathBuf::from("clash.rs"));
        assert_eq!(status.staged(), 0);
        assert_eq!(status.conflicted(), 1);
        assert_eq!(
            status.summary(),
            "main ±1 !1",
            "a conflict is the one thing that has to be dealt with first"
        );
    }

    #[test]
    fn a_path_with_spaces_survives() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5",
            "# branch.head main",
            "1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb src/my notes.md",
            "? another file.txt",
            "2 R. N... 100644 100644 100644 aaaaaaa bbbbbbb R100 to there.rs",
            "from here.rs",
        ]))
        .expect("git's own format");

        assert_eq!(status.entries[0].path, PathBuf::from("src/my notes.md"));
        assert_eq!(status.entries[1].path, PathBuf::from("another file.txt"));
        assert_eq!(status.entries[2].path, PathBuf::from("to there.rs"));
        assert_eq!(
            status.entries[2].original,
            Some(PathBuf::from("from here.rs"))
        );
    }

    #[test]
    fn ignored_files_are_dropped_rather_than_refused() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5",
            "# branch.head main",
            "! target/debug/deco",
            "? new.rs",
        ]))
        .expect("git's own format");
        assert_eq!(status.changed(), 1, "`target/` is not news");
    }

    #[test]
    fn a_header_this_does_not_know_is_ignored() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5",
            "# branch.head main",
            "# stash 3",
        ]))
        .expect("an unknown header must not break a working status bar");
        assert_eq!(status.head, Head::Branch("main".into()));
    }

    #[test]
    fn output_that_is_not_gits_is_refused_rather_than_guessed() {
        for bad in [
            // No headers at all: something answered, but not `git status`.
            "? only.rs\0",
            // A tag with no space after it.
            "# branch.oid 1c9d4e5\0# branch.head main\0x\0",
            // A code that is not one of git's.
            "# branch.oid 1c9d4e5\0# branch.head main\0\
             1 ZZ N... 100644 100644 100644 aaaaaaa bbbbbbb odd.rs\0",
            // Too few fields before the path.
            "# branch.oid 1c9d4e5\0# branch.head main\0\
             1 .M N... 100644 aaaaaaa short.rs\0",
        ] {
            assert!(
                parse(bad).is_err(),
                "{bad:?} was read as a status rather than refused"
            );
        }
    }

    #[test]
    fn an_upstream_without_counts_is_level_rather_than_unknown() {
        let status = parse(&output(&[
            "# branch.oid 1c9d4e5",
            "# branch.head main",
            "# branch.upstream origin/main",
        ]))
        .expect("git's own format");
        let upstream = status.upstream.expect("there is one");
        assert_eq!((upstream.ahead, upstream.behind), (0, 0));
    }
}
