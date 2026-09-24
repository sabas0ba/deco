//! The `git status --porcelain=v2` data model and its parser.
//!
//! Nothing here spawns a process. The parser takes a string and returns a
//! [`Status`], so every output form git can produce (a detached head, an unborn
//! branch, a rename, a merge conflict, a path with a space) can be tested with a
//! string literal instead of a repository built in CI.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Output that was not the format `--porcelain=v2` promises.
///
/// A single type with a description rather than one case per field. Git has
/// kept this format stable since 2.11, so a mismatch means the program is not a
/// compatible git, and the only useful information is which line was not
/// understood.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`git status` said something this does not understand: {0}")]
pub struct Malformed(pub String);

/// The state of `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Head {
    /// On a branch with at least one commit.
    Branch(String),
    /// On a branch that does not exist yet: `git init` with nothing committed.
    ///
    /// A separate variant from [`Head::Branch`] because there is no commit to
    /// compare against. There are no tracked files, and a caller should not
    /// offer to show a diff.
    Unborn(String),
    /// Not on a branch. The full commit id; shorten it for display with
    /// [`Head::label`].
    Detached(String),
}

impl Head {
    /// The label for a status bar.
    ///
    /// A detached head is shortened to seven characters, git's default
    /// abbreviation length. Branch names are chosen by the user and are shown
    /// in full.
    pub fn label(&self) -> String {
        match self {
            Self::Branch(name) | Self::Unborn(name) => name.clone(),
            Self::Detached(commit) => commit.chars().take(7).collect(),
        }
    }
}

/// The upstream branch this branch tracks, and the distance between them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// Its name as git prints it, such as `origin/main`.
    pub name: String,
    /// Commits on this branch that are not on the upstream.
    pub ahead: usize,
    /// Commits on the upstream that are not on this branch.
    pub behind: usize,
}

/// The change to one file on one side.
///
/// `git` reports two of these per tracked entry: the index relative to `HEAD`
/// (staged), and the working tree relative to the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    /// No change on this side. Git's `.`.
    None,
    /// Contents differ.
    Modified,
    /// The file type changed, for example a regular file became a symlink.
    /// Git's `T`.
    ///
    /// Distinct from [`Change::Modified`] because the contents may be
    /// identical while the type differs.
    TypeChanged,
    /// New to the index.
    Added,
    /// Removed.
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

    /// Whether this side has no change.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Why a file is in the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Tracked by git, with a change on at least one side.
    ///
    /// At least one of the two is not [`Change::None`]; git does not report
    /// entries with no change on either side.
    Tracked {
        /// The index against `HEAD`: what a commit would record.
        staged: Change,
        /// The working tree against the index: what a `git add` would stage.
        worktree: Change,
    },
    /// Not in the index.
    Untracked,
    /// A merge left the file with unresolved conflicts.
    Conflicted,
}

/// One entry of `git status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStatus {
    /// The current path, relative to the repository root.
    pub path: PathBuf,
    /// The previous path, for a rename or a copy.
    pub original: Option<PathBuf>,
    /// The reason the file is listed.
    pub state: State,
}

/// Everything one `git status` run reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The state of `HEAD`.
    pub head: Head,
    /// The commit `HEAD` names, or `None` on a branch with no commits yet.
    ///
    /// Kept even for a named branch, where it is not displayed. It is the only
    /// field that changes on commit, and a caller caching a file's committed
    /// text uses it to invalidate that cache. Without it, a commit made in a
    /// terminal would leave gutters comparing against the old version until
    /// the file was closed.
    pub commit: Option<String>,
    /// The upstream branch, if any.
    pub upstream: Option<Upstream>,
    /// Every file git reported.
    ///
    /// Ignored files are excluded: the run uses git's default, which omits
    /// them, and a list dominated by `target/` would not be useful.
    pub entries: Vec<FileStatus>,
}

impl Status {
    /// How many files differ from `HEAD` in any way, including untracked files.
    ///
    /// Counted per file, not per side. A file that is staged and modified again
    /// counts once, so the status bar agrees with the list.
    pub fn changed(&self) -> usize {
        self.entries.len()
    }

    /// How many entries have a staged change.
    pub fn staged(&self) -> usize {
        self.entries
            .iter()
            .filter(
                |entry| matches!(&entry.state, State::Tracked { staged, .. } if !staged.is_none()),
            )
            .count()
    }

    /// Tracked paths whose working-tree side differs from the index.
    pub fn unstaged(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| match &entry.state {
                State::Tracked { worktree, .. } => !worktree.is_none(),
                State::Untracked | State::Conflicted => false,
            })
            .count()
    }

    /// How many entries are untracked.
    pub fn untracked(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.state == State::Untracked)
            .count()
    }

    /// How many entries have unresolved merge conflicts.
    pub fn conflicted(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.state == State::Conflicted)
            .count()
    }

    /// Whether the status contains no changed or untracked entries.
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    /// The one-line form for a status bar.
    ///
    /// Defined here rather than in a renderer because it decides the content,
    /// not the presentation. The GPU frontend has no status bar yet; when it
    /// adds one, it will show this string, so the two do not need to be kept in
    /// sync manually.
    ///
    /// Uses markers rather than words, and omits each marker at zero, like the
    /// problem counts. A permanent `0 changed` would be noise; the absence of
    /// a marker is the signal.
    ///
    /// - `±4`: four files differ from `HEAD`.
    /// - `↑2 ↓1`: two commits to push, one to pull.
    /// - `!2`: two files with merge conflicts, which must be resolved before
    ///   other operations work.
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
/// `-z` is required for correctness. Without it, git C-quotes any path
/// containing a space, a quote or a non-ASCII byte, and separates a rename's
/// two paths with a tab, which a path may also contain. A parser would have to
/// reverse git's quoting exactly. With `-z`, every field ends at a NUL and
/// nothing is quoted.
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
        // are followed by a path that may start with any character.
        let (tag, rest) = match record.split_once(' ') {
            Some(split) => split,
            // A record with no space matches none of the five record types.
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
                    // `# stash <n>` and any header git adds later. These are
                    // skipped so that a new header does not cause an error.
                    _ => {}
                }
            }
            "1" => entries.push(tracked(rest, record, 7)?),
            "2" => {
                let mut entry = tracked(rest, record, 8)?;
                // The only record that spans two NUL-separated fields: git
                // writes the new path, a NUL, then the old path.
                let original = records.next().ok_or_else(|| Malformed(record.into()))?;
                entry.original = Some(PathBuf::from(original));
                entries.push(entry);
            }
            "u" => {
                // Ten fields before the path. The two-letter code describes the
                // type of conflict, which the list does not use: every
                // conflicted file needs manual resolution.
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
            // Ignored files are reported only when requested, and they are not
            // requested. They are skipped rather than rejected so that enabling
            // the flag later requires changing only one call site.
            "!" => {}
            _ => return Err(Malformed(record.to_owned())),
        }
    }

    let oid = oid.ok_or_else(|| Malformed("no `# branch.oid` header".into()))?;
    let head = head.ok_or_else(|| Malformed("no `# branch.head` header".into()))?;
    // `(initial)` means there is no commit; it is not a commit id.
    let commit = (oid != "(initial)").then(|| oid.clone());
    let head = if head == "(detached)" {
        Head::Detached(oid)
    } else if commit.is_none() {
        Head::Unborn(head)
    } else {
        Head::Branch(head)
    };

    // If Git supplies an upstream name without ahead/behind counts, use zero
    // for both counts. This does not distinguish missing counts from equality.
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

/// Parses a `1` or `2` record, which share their leading fields but differ in
/// field count.
///
/// `before` is the number of fields between the tag and the path: seven for an
/// ordinary change, eight for a rename or copy, whose extra field is the
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
/// it is not quoted, so a name containing spaces keeps them. An empty tail is
/// `None`, because a record without a path is malformed.
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

    /// Builds the NUL-terminated form git writes, so the tests list readable
    /// records instead of escape sequences.
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
            // No headers: the output is not from `git status`.
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
