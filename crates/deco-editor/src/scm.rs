//! The source-control view: what `git status` said, as a list you can act on.
//!
//! A side-bar tenant beside the [file tree](crate::explorer), and fed the same
//! way — handed a [`deco_scm::Status`] rather than running anything, because
//! the core has no filesystem and cannot spawn a process. What it adds is the
//! shape a person needs: the entries grouped by what you would *do* about
//! them, a selection that survives a refresh, and one row per thing that can
//! be staged.
//!
//! # One file, sometimes two rows
//!
//! A file can be staged *and* modified since. Git reports that as one entry
//! with two halves; the view shows it twice, under **Staged Changes** and
//! under **Changes**, because they are two different things you can act on —
//! unstaging the first and staging the second do opposite things to the same
//! file. VS Code splits it the same way, and a single row would make
//! `git.stage` ambiguous about which half it meant.
//!
//! The status bar's count does *not* do this: `±2` is how many files need
//! thinking about, and counting one file twice there would make the bar
//! disagree with itself.

use std::path::{Path, PathBuf};

use deco_scm::{Change, State, Status};

/// Where a row sits, and therefore what can be done to it.
///
/// The order is the order they are shown in, and it is the order of what has
/// to be dealt with first: a conflict blocks everything, staged changes are
/// what a commit would record, and untracked files are the ones git does not
/// know about yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// A merge left it unresolved. Nothing else matters until this does.
    Conflicts,
    /// The index against `HEAD`: what committing now would record.
    Staged,
    /// The working tree against the index: what `git.stage` would add.
    Changes,
    /// Not in the index at all.
    Untracked,
}

impl Group {
    /// The heading above it, in VS Code's words.
    pub fn title(self) -> &'static str {
        match self {
            Self::Conflicts => "Merge Changes",
            Self::Staged => "Staged Changes",
            Self::Changes => "Changes",
            Self::Untracked => "Untracked",
        }
    }
}

/// One line of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Where the file is, relative to the repository root.
    ///
    /// The same coordinates [`deco_scm::Status`] reports in and
    /// [`deco_scm::Git::committed`] answers about — so a row can be handed
    /// straight to git without a conversion nobody would remember to do.
    pub path: PathBuf,
    /// Which heading it sits under.
    pub group: Group,
    /// What happened to it, on this side.
    pub change: Change,
    /// Where it was, for a rename or a copy.
    pub original: Option<PathBuf>,
}

impl Row {
    /// The single letter git uses, for the column beside the name.
    ///
    /// `U` for a conflict, which is git's own letter for an unmerged path, and
    /// `?` for something untracked — neither of which is a [`Change`], because
    /// neither is a thing that happened to a tracked file.
    pub fn letter(&self) -> char {
        match (self.group, self.change) {
            (Group::Conflicts, _) => 'U',
            (Group::Untracked, _) => '?',
            (_, Change::None) => ' ',
            (_, Change::Modified) => 'M',
            (_, Change::TypeChanged) => 'T',
            (_, Change::Added) => 'A',
            (_, Change::Deleted) => 'D',
            (_, Change::Renamed) => 'R',
            (_, Change::Copied) => 'C',
        }
    }

    /// The file's own name, for the row.
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// The directory it is in, for the dimmer second column. `None` at the
    /// repository root, where there is nothing to say.
    pub fn directory(&self) -> Option<String> {
        let parent = self.path.parent()?;
        (!parent.as_os_str().is_empty()).then(|| parent.display().to_string())
    }
}

/// The view: rows, and which one is selected.
///
/// Only files are rows. The headings a renderer draws between them are derived
/// from [`Row::group`] as it walks the list, which keeps the selection from
/// ever landing on something that cannot be staged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceControl {
    rows: Vec<Row>,
    selected: usize,
}

impl SourceControl {
    /// Builds the view from what git said.
    pub fn from_status(status: &Status) -> Self {
        let mut view = Self::default();
        view.refresh(status);
        view
    }

    /// Replaces the rows with a fresh status, keeping the selection where it
    /// can.
    ///
    /// By path *and* group, not by index: a refresh reorders the list whenever
    /// something is staged, and a selection that stayed on row 3 would land on
    /// a different file than the one the user was looking at — which is how a
    /// `git.stage` ends up staging the wrong thing.
    pub fn refresh(&mut self, status: &Status) {
        let was = self.selection().map(|row| (row.path.clone(), row.group));
        self.rows = rows_of(status);
        self.selected = was
            .and_then(|(path, group)| {
                self.rows
                    .iter()
                    .position(|row| row.path == path && row.group == group)
            })
            // The file it was on is gone — staged, committed, reverted. The
            // index is kept rather than reset to the top, so the selection
            // lands next to where the user was working.
            .unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1)));
    }

    /// Every row, in the order they are shown.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Nothing to show: a clean working tree.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The row the keyboard is on.
    pub fn selection(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Which row is selected, for a renderer that draws it differently.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Moves down, stopping at the end rather than wrapping.
    ///
    /// The tree does the same. Wrapping in a list you are stepping through to
    /// stage things means an unnoticed jump from the last file to the first,
    /// and the next keystroke acting on the wrong one.
    pub fn select_next(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    /// Moves up, stopping at the top.
    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// To the first row.
    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    /// To the last row.
    pub fn select_last(&mut self) {
        self.selected = self.rows.len().saturating_sub(1);
    }

    /// Puts the selection on a particular file, if it is listed.
    ///
    /// The first row for that path, whichever group it is in — a caller naming
    /// a file means the file, and picking the staged half over the unstaged
    /// one is not a decision it has made.
    pub fn reveal(&mut self, path: &Path) -> bool {
        match self.rows.iter().position(|row| row.path == path) {
            Some(at) => {
                self.selected = at;
                true
            }
            None => false,
        }
    }

    /// How many rows sit under each heading, in display order.
    ///
    /// For a renderer that wants to draw `Changes (3)` the way VS Code does,
    /// without walking the list itself.
    pub fn groups(&self) -> Vec<(Group, usize)> {
        let mut out: Vec<(Group, usize)> = Vec::new();
        for row in &self.rows {
            match out.last_mut() {
                Some((group, count)) if *group == row.group => *count += 1,
                _ => out.push((row.group, 1)),
            }
        }
        out
    }
}

/// Turns git's entries into rows, in display order.
fn rows_of(status: &Status) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for entry in &status.entries {
        match &entry.state {
            State::Conflicted => rows.push(Row {
                path: entry.path.clone(),
                group: Group::Conflicts,
                change: Change::None,
                original: entry.original.clone(),
            }),
            State::Untracked => rows.push(Row {
                path: entry.path.clone(),
                group: Group::Untracked,
                change: Change::None,
                original: None,
            }),
            State::Tracked { staged, worktree } => {
                // Both halves, when both have something to say. See the module
                // docs: they are two different things to act on.
                if !staged.is_none() {
                    rows.push(Row {
                        path: entry.path.clone(),
                        group: Group::Staged,
                        change: *staged,
                        original: entry.original.clone(),
                    });
                }
                if !worktree.is_none() {
                    rows.push(Row {
                        path: entry.path.clone(),
                        group: Group::Changes,
                        change: *worktree,
                        // A rename is recorded in the *index*; the working-tree
                        // half of the same entry is about the file at its new
                        // name, and carrying the old one there would label it
                        // as a move that this half did not make.
                        original: None,
                    });
                }
            }
        }
    }
    // By group first, then by path, so the list is stable across refreshes and
    // reads in the order the headings do. `sort_by_key` is stable, so files
    // keep git's order within a group when their paths tie — which they cannot,
    // but relying on that would be one more thing to be wrong about.
    rows.sort_by(|one, two| {
        one.group
            .cmp(&two.group)
            .then_with(|| one.path.cmp(&two.path))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds what `git status --porcelain=v2 --branch -z` writes.
    fn status(entries: &[&str]) -> Status {
        let mut out = String::from("# branch.oid 1c9d4e5\0# branch.head main\0");
        for entry in entries {
            out.push_str(entry);
            out.push('\0');
        }
        deco_scm::parse(&out).expect("git's own format")
    }

    const STAGED: &str = "1 M. N... 100644 100644 100644 aaaaaaa bbbbbbb staged.rs";
    const UNSTAGED: &str = "1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb work.rs";
    const BOTH: &str = "1 MM N... 100644 100644 100644 aaaaaaa bbbbbbb both.rs";
    const UNTRACKED: &str = "? new.rs";
    const CONFLICT: &str = "u UU N... 100644 100644 100644 100644 aaaaaaa bbbbbbb ccccccc clash.rs";

    #[test]
    fn a_clean_tree_has_nothing_in_it() {
        let view = SourceControl::from_status(&status(&[]));
        assert!(view.is_empty());
        assert_eq!(view.selection(), None);
    }

    #[test]
    fn a_file_staged_and_modified_since_is_two_rows() {
        let view = SourceControl::from_status(&status(&[BOTH]));
        let rows: Vec<(Group, char)> = view
            .rows()
            .iter()
            .map(|row| (row.group, row.letter()))
            .collect();
        assert_eq!(
            rows,
            vec![(Group::Staged, 'M'), (Group::Changes, 'M')],
            "unstaging one and staging the other do opposite things to the \
             same file, so they cannot be one row"
        );
    }

    #[test]
    fn the_groups_come_in_the_order_they_have_to_be_dealt_with() {
        let view = SourceControl::from_status(&status(&[UNTRACKED, UNSTAGED, CONFLICT, STAGED]));
        assert_eq!(
            view.groups(),
            vec![
                (Group::Conflicts, 1),
                (Group::Staged, 1),
                (Group::Changes, 1),
                (Group::Untracked, 1),
            ],
            "a conflict blocks everything else, so it is first whatever order \
             git listed things in"
        );
    }

    #[test]
    fn the_selection_follows_the_file_rather_than_the_row_number() {
        let mut view = SourceControl::from_status(&status(&[STAGED, UNSTAGED, UNTRACKED]));
        // Rows: staged.rs (Staged), work.rs (Changes), new.rs (Untracked).
        view.select_next();
        assert_eq!(view.selection().unwrap().path, PathBuf::from("work.rs"));

        // `staged.rs` gets committed, so the list shortens from the top. A
        // selection that stayed on row 1 would now be on `new.rs`, and the
        // next `git.stage` would act on a file the user never looked at.
        view.refresh(&status(&[UNSTAGED, UNTRACKED]));
        assert_eq!(view.selection().unwrap().path, PathBuf::from("work.rs"));
    }

    #[test]
    fn staging_the_selected_file_keeps_the_selection_on_it() {
        let mut view = SourceControl::from_status(&status(&[UNSTAGED]));
        assert_eq!(view.selection().unwrap().group, Group::Changes);

        // The same file, now staged. It has moved to a different heading, and
        // the selection follows it there rather than to whatever took its old
        // place.
        view.refresh(&status(&[
            "1 M. N... 100644 100644 100644 aaaaaaa bbbbbbb work.rs",
        ]));
        let selected = view.selection().expect("still listed");
        assert_eq!(selected.path, PathBuf::from("work.rs"));
        assert_eq!(selected.group, Group::Staged);
    }

    #[test]
    fn a_file_that_leaves_the_list_does_not_send_the_selection_to_the_top() {
        let mut view = SourceControl::from_status(&status(&[STAGED, UNSTAGED, UNTRACKED]));
        view.select_last();
        assert_eq!(view.selection().unwrap().path, PathBuf::from("new.rs"));

        // `new.rs` is committed away. The selection lands beside where the
        // user was, not back at the beginning of a list they had scrolled.
        view.refresh(&status(&[STAGED, UNSTAGED]));
        assert_eq!(view.selection().unwrap().path, PathBuf::from("work.rs"));
    }

    #[test]
    fn moving_stops_at_the_ends_rather_than_wrapping() {
        let mut view = SourceControl::from_status(&status(&[STAGED, UNSTAGED]));
        view.select_previous();
        assert_eq!(view.selected_index(), 0, "already at the top");
        view.select_next();
        view.select_next();
        assert_eq!(
            view.selected_index(),
            1,
            "a wrap here would put the next keystroke on the wrong file"
        );
    }

    #[test]
    fn a_row_names_its_file_and_the_directory_it_is_in() {
        let view = SourceControl::from_status(&status(&[
            "1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb src/parse/lexer.rs",
        ]));
        let row = view.selection().expect("one row");
        assert_eq!(row.name(), "lexer.rs");
        assert_eq!(row.directory().as_deref(), Some("src/parse"));
    }

    #[test]
    fn a_file_at_the_root_has_no_directory_to_show() {
        let view = SourceControl::from_status(&status(&[UNSTAGED]));
        assert_eq!(view.selection().unwrap().directory(), None);
    }

    #[test]
    fn a_conflict_and_an_untracked_file_get_gits_own_letters() {
        let view = SourceControl::from_status(&status(&[CONFLICT, UNTRACKED]));
        let letters: Vec<char> = view.rows().iter().map(Row::letter).collect();
        assert_eq!(letters, vec!['U', '?']);
    }

    #[test]
    fn a_rename_carries_its_old_name_on_the_staged_half_only() {
        // Git records a rename in the index. The working-tree half of the same
        // entry is about the file at its *new* name, so labelling it as a move
        // would say the working tree did something it did not.
        let view = SourceControl::from_status(&status(&[
            "2 RM N... 100644 100644 100644 aaaaaaa bbbbbbb R100 new/name.rs",
            "old/name.rs",
        ]));
        let staged = &view.rows()[0];
        let changed = &view.rows()[1];
        assert_eq!(staged.group, Group::Staged);
        assert_eq!(staged.original, Some(PathBuf::from("old/name.rs")));
        assert_eq!(changed.group, Group::Changes);
        assert_eq!(changed.original, None);
    }

    #[test]
    fn revealing_a_file_finds_it_whichever_half_is_listed_first() {
        let mut view = SourceControl::from_status(&status(&[STAGED, BOTH]));
        assert!(view.reveal(Path::new("both.rs")));
        let selected = view.selection().unwrap();
        assert_eq!(selected.path, PathBuf::from("both.rs"));
        assert_eq!(
            selected.group,
            Group::Staged,
            "the first row for that file, since naming a file is not a choice \
             between its halves"
        );
        assert!(!view.reveal(Path::new("nothing.rs")));
    }
}
