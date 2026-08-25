//! The file tree: what it holds, what it shows, and what it has to ask for.
//!
//! # Fed, not reading
//!
//! There is no `read_dir` in this file. The core has no filesystem — a document
//! is handed its text, never a path to read — and that is what lets the whole
//! editable surface be tested without one. The tree keeps that: it holds
//! whatever it has been *told* a directory contains, and when it is asked to
//! show a directory it has not been told about it says so, through
//! [`Explorer::wanted`], for whoever does have a filesystem to answer.
//!
//! That is not a purity exercise. The same property is what makes the tree work
//! on a remote workspace: `deco-remote` answers the request over the connection
//! instead of `std::fs`, and nothing here changes. Quick open already lists a
//! remote workspace this way.
//!
//! # Bounded by the window
//!
//! A directory is read when it is first expanded, not before, so a workspace
//! costs what its *visible* rows cost rather than what it contains.
//! [`Explorer::rows`] walks the expanded parts only, and the caller takes the
//! slice that fits the side bar.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One entry a directory listing reports.
///
/// Only what the tree draws and sorts by. Size, permissions and times are
/// deliberately absent: nothing shows them, and carrying them would mean
/// deciding how stale they are allowed to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's own name, not its path.
    pub name: String,
    /// Whether it can be expanded.
    pub is_dir: bool,
}

impl Entry {
    /// A file.
    pub fn file(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            is_dir: false,
        }
    }

    /// A directory.
    pub fn dir(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            is_dir: true,
        }
    }
}

/// One row of the tree as it is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The full path, for opening it or asking about it.
    pub path: PathBuf,
    /// What to show.
    pub name: String,
    /// How far in, in tree levels. The root's children are at zero.
    pub depth: usize,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Whether it is expanded — meaningless for a file.
    pub expanded: bool,
    /// Whether the selection is on it.
    pub selected: bool,
}

/// What a directory's contents are, as far as the tree knows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Contents {
    /// Asked for, not yet answered.
    Pending,
    /// Answered, and this is what is in it.
    Known(Vec<Entry>),
}

/// The workspace tree.
///
/// Expansion and selection live here rather than in a frontend, because both
/// survive a redraw and neither is a property of a terminal. Two frontends draw
/// this; only one model decides what it says.
#[derive(Debug, Clone)]
pub struct Explorer {
    /// The workspace root. Its own name is not a row — the tree is what is *in*
    /// the workspace, and a single root row that can never be collapsed is a row
    /// spent on nothing.
    root: PathBuf,
    /// Directory contents, keyed by path. A directory absent from here has never
    /// been expanded.
    listings: BTreeMap<PathBuf, Contents>,
    /// Which directories are open.
    expanded: Vec<PathBuf>,
    /// The selected row, as an index into [`Explorer::rows`].
    selected: usize,
    /// The first visible row, so a long tree can be scrolled.
    scroll: usize,
    /// A path [`Explorer::reveal`] is still trying to land on.
    ///
    /// Revealing a file means opening the directories above it, and those are
    /// read one listing at a time — so at the moment `reveal` is called the row
    /// it wants usually does not exist yet. Holding the path here lets
    /// [`Explorer::fill`] put the selection on it when it appears, which is what
    /// makes revealing the file deco was started with work: the tree has never
    /// seen any of it.
    revealing: Option<PathBuf>,
}

impl Explorer {
    /// An explorer rooted at `root`, with the root's own listing outstanding.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut listings = BTreeMap::new();
        listings.insert(root.clone(), Contents::Pending);
        Self {
            root,
            listings,
            expanded: Vec::new(),
            selected: 0,
            scroll: 0,
            revealing: None,
        }
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A directory whose contents the tree needs and does not have.
    ///
    /// One at a time, and only ones that are actually being shown: an expanded
    /// directory inside a collapsed one is not on screen, so reading it now
    /// would be work for a row nobody can see. The caller answers with
    /// [`Explorer::fill`] and asks again — a directory whose children are
    /// themselves expanded takes as many turns as it has levels, which is the
    /// price of never reading more than the window needs.
    pub fn wanted(&self) -> Option<PathBuf> {
        if matches!(self.listings.get(&self.root), Some(Contents::Pending)) {
            return Some(self.root.clone());
        }
        // An expanded directory is always in the map — `set_expanded` puts it
        // there — so `Pending` is the whole of "asked for and unanswered".
        self.rows()
            .into_iter()
            .find(|row| {
                row.expanded && matches!(self.listings.get(&row.path), Some(Contents::Pending))
            })
            .map(|row| row.path)
    }

    /// Records what `dir` contains.
    ///
    /// Entries are sorted here rather than trusted from the caller: `read_dir`
    /// gives no order, and a tree that reshuffles when it is re-read is one you
    /// cannot learn. Directories come first and then case-insensitively by name,
    /// which is what VS Code's explorer does and what every file manager has
    /// trained people to expect.
    pub fn fill(&mut self, dir: &Path, mut entries: Vec<Entry>) {
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.listings
            .insert(dir.to_path_buf(), Contents::Known(entries));
        self.land_reveal();
        self.clamp();
    }

    /// Forgets `dir`'s contents so it is read again when next shown.
    ///
    /// The tree has no way to notice a file appearing on disk — there is no
    /// watcher, and adding one is its own piece of work with its own failure
    /// modes on every platform. This is what a refresh is built out of.
    pub fn invalidate(&mut self, dir: &Path) {
        if self.listings.contains_key(dir) {
            self.listings.insert(dir.to_path_buf(), Contents::Pending);
        }
    }

    /// Forgets the contents of `prefix` and of everything under it.
    ///
    /// What a half-finished recursive delete needs: the directory itself may
    /// survive, so re-reading its *parent* rediscovers it and leaves its own
    /// cached listing — and every expanded listing below that — describing files
    /// that are gone.
    pub fn invalidate_under(&mut self, prefix: &Path) {
        let stale: Vec<PathBuf> = self
            .listings
            .keys()
            .filter(|dir| dir.starts_with(prefix))
            .cloned()
            .collect();
        for dir in stale {
            self.listings.insert(dir, Contents::Pending);
        }
    }

    /// Whether the root's listing has arrived.
    ///
    /// The difference between "this workspace is empty" and "nobody has read it
    /// yet" — which is a sentence the side bar shows, so it has to be askable.
    pub fn loaded(&self) -> bool {
        matches!(self.listings.get(&self.root), Some(Contents::Known(_)))
    }

    /// The rows to draw, from the first visible one, at most `height` of them.
    pub fn visible(&self, height: usize) -> Vec<Row> {
        let rows = self.rows();
        rows.into_iter().skip(self.scroll).take(height).collect()
    }

    /// Every row the tree currently shows, in order.
    pub fn rows(&self) -> Vec<Row> {
        self.rows_inner(usize::MAX)
    }

    /// How many rows there are, for a caller that only needs the count.
    pub fn len(&self) -> usize {
        self.rows().len()
    }

    /// Whether the tree shows nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The selected row, if there is one.
    pub fn selection(&self) -> Option<Row> {
        self.rows().into_iter().find(|row| row.selected)
    }

    /// The first visible row's index.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Moves the selection down one row.
    pub fn select_next(&mut self) {
        let len = self.len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected + 1).min(len - 1);
    }

    /// Moves the selection up one row.
    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Puts the selection on the first row.
    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    /// Puts the selection on the last row.
    pub fn select_last(&mut self) {
        self.selected = self.len().saturating_sub(1);
    }

    /// Opens the selected directory, or moves into it if it is already open.
    ///
    /// Right-arrow in VS Code's explorer does both, and the second is what makes
    /// arrowing through a tree feel like one gesture rather than two.
    pub fn expand(&mut self) {
        let Some(row) = self.selection() else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if row.expanded {
            self.select_next();
        } else {
            self.set_expanded(&row.path, true);
        }
    }

    /// Closes the selected directory, or moves to its parent if it is a file or
    /// already closed.
    pub fn collapse(&mut self) {
        let Some(row) = self.selection() else {
            return;
        };
        if row.is_dir && row.expanded {
            self.set_expanded(&row.path, false);
            return;
        }
        // Up to the parent — the other half of the one-gesture rule.
        let Some(parent) = row.path.parent() else {
            return;
        };
        if parent == self.root {
            return;
        }
        if let Some(index) = self.rows().iter().position(|r| r.path == parent) {
            self.selected = index;
        }
    }

    /// Toggles the selected directory.
    pub fn toggle(&mut self) {
        let Some(row) = self.selection() else {
            return;
        };
        if row.is_dir {
            self.set_expanded(&row.path, !row.expanded);
        }
    }

    /// Opens every directory on the way to `path` and selects it.
    ///
    /// The directories are expanded whether or not their contents are known, and
    /// the selection is *remembered* rather than applied: the row for `path`
    /// usually does not exist yet, because the listings that would produce it
    /// have only just been asked for. [`Explorer::fill`] lands it as they
    /// arrive. That is what lets this be called for a file the tree has never
    /// seen — which is every file, at startup.
    pub fn reveal(&mut self, path: &Path) {
        if path.strip_prefix(&self.root).is_err() {
            return;
        }
        let Some(parent) = path.parent() else {
            return;
        };
        // Every directory between the root and the file, outermost first.
        let mut dirs = Vec::new();
        let mut at = Some(parent);
        while let Some(dir) = at {
            if dir == self.root {
                break;
            }
            dirs.push(dir.to_path_buf());
            at = dir.parent();
        }
        for dir in dirs.iter().rev() {
            self.set_expanded(dir, true);
        }
        self.revealing = Some(path.to_path_buf());
        self.land_reveal();
    }

    /// Puts the selection on the revealed path once its row exists.
    ///
    /// Gives up when the directory that would hold it has been read and it is
    /// not there — a file that was deleted, or one outside what the listing
    /// reports. Without that, a reveal nobody can satisfy would sit there and
    /// steal the selection from every later listing.
    fn land_reveal(&mut self) {
        let Some(path) = self.revealing.clone() else {
            return;
        };
        if let Some(index) = self.rows().iter().position(|row| row.path == path) {
            self.selected = index;
            self.revealing = None;
            return;
        }
        let holds_it = path
            .parent()
            .is_some_and(|parent| matches!(self.listings.get(parent), Some(Contents::Known(_))));
        if holds_it {
            self.revealing = None;
        }
    }

    /// Scrolls so the selection is within `height` rows of the top.
    ///
    /// Called by whoever knows how tall the side bar is, which is not this.
    pub fn scroll_into_view(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
    }

    /// Opens or closes `dir`, requesting its contents the first time.
    fn set_expanded(&mut self, dir: &Path, open: bool) {
        let at = self.expanded.iter().position(|p| p == dir);
        match (open, at) {
            (true, None) => {
                self.expanded.push(dir.to_path_buf());
                self.listings
                    .entry(dir.to_path_buf())
                    .or_insert(Contents::Pending);
            }
            (false, Some(index)) => {
                self.expanded.remove(index);
            }
            _ => {}
        }
        self.clamp();
    }

    /// Whether `dir` is open.
    fn is_expanded(&self, dir: &Path) -> bool {
        self.expanded.iter().any(|p| p == dir)
    }

    /// Walks the expanded tree, marking the selected row.
    fn rows_inner(&self, limit: usize) -> Vec<Row> {
        let mut rows = Vec::new();
        self.walk(&self.root, 0, limit, &mut rows);
        if let Some(row) = rows.get_mut(self.selected) {
            row.selected = true;
        }
        rows
    }

    /// Appends `dir`'s visible rows, depth-first.
    fn walk(&self, dir: &Path, depth: usize, limit: usize, rows: &mut Vec<Row>) {
        let Some(Contents::Known(entries)) = self.listings.get(dir) else {
            return;
        };
        for entry in entries {
            if rows.len() >= limit {
                return;
            }
            let path = dir.join(&entry.name);
            let expanded = entry.is_dir && self.is_expanded(&path);
            rows.push(Row {
                path: path.clone(),
                name: entry.name.clone(),
                depth,
                is_dir: entry.is_dir,
                expanded,
                selected: false,
            });
            if expanded {
                self.walk(&path, depth + 1, limit, rows);
            }
        }
    }

    /// Keeps the selection on a row that exists.
    ///
    /// Collapsing a directory can take the selected row away with it, and a
    /// selection pointing past the end draws nothing highlighted — which reads
    /// as the tree having lost focus rather than as a row having gone.
    fn clamp(&mut self) {
        let len = self.len();
        if len == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        self.selected = self.selected.min(len - 1);
        self.scroll = self.scroll.min(self.selected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree with `src/` and two files at the root, nothing expanded.
    fn tree() -> Explorer {
        let mut explorer = Explorer::new("/w");
        explorer.fill(
            Path::new("/w"),
            vec![
                Entry::file("README.md"),
                Entry::dir("src"),
                Entry::file("Cargo.toml"),
            ],
        );
        explorer
    }

    #[test]
    fn a_directory_is_not_read_until_it_is_opened() {
        let mut explorer = tree();
        // Nothing outstanding: the root arrived and nothing else is showing.
        assert_eq!(explorer.wanted(), None);

        explorer.select_next(); // Cargo.toml, README.md, src — dirs first.
        explorer.select_first();
        assert_eq!(explorer.selection().unwrap().name, "src");
        explorer.expand();

        // Now it is on screen and unknown, so the tree asks.
        assert_eq!(explorer.wanted().as_deref(), Some(Path::new("/w/src")));
    }

    #[test]
    fn directories_come_first_then_case_insensitively_by_name() {
        let explorer = tree();
        let names: Vec<_> = explorer.rows().into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["src", "Cargo.toml", "README.md"]);
    }

    #[test]
    fn an_unopened_directory_contributes_one_row_however_much_it_holds() {
        let mut explorer = tree();
        explorer.fill(
            Path::new("/w/src"),
            (0..1000)
                .map(|i| Entry::file(&format!("f{i}.rs")))
                .collect(),
        );
        // Known but not expanded: the thousand files cost nothing.
        assert_eq!(explorer.len(), 3);
        explorer.select_first();
        explorer.expand();
        assert_eq!(explorer.len(), 1003);
    }

    #[test]
    fn collapsing_a_directory_does_not_strand_the_selection() {
        let mut explorer = tree();
        explorer.fill(Path::new("/w/src"), vec![Entry::file("main.rs")]);
        explorer.select_first();
        explorer.expand();
        explorer.select_next();
        assert_eq!(explorer.selection().unwrap().name, "main.rs");

        // The selected row is inside what is being closed.
        explorer.select_previous();
        explorer.collapse();
        let selection = explorer.selection().expect("a row is still selected");
        assert_eq!(selection.name, "src");
        assert!(explorer.rows().iter().any(|r| r.selected));
    }

    #[test]
    fn right_on_an_open_directory_steps_into_it() {
        let mut explorer = tree();
        explorer.fill(Path::new("/w/src"), vec![Entry::file("main.rs")]);
        explorer.select_first();
        explorer.expand(); // opens
        explorer.expand(); // steps in
        assert_eq!(explorer.selection().unwrap().name, "main.rs");
    }

    #[test]
    fn left_on_a_file_goes_to_its_directory() {
        let mut explorer = tree();
        explorer.fill(Path::new("/w/src"), vec![Entry::file("main.rs")]);
        explorer.select_first();
        explorer.expand();
        explorer.select_next();
        explorer.collapse();
        assert_eq!(explorer.selection().unwrap().name, "src");
    }

    #[test]
    fn revealing_a_file_opens_every_directory_above_it() {
        let mut explorer = tree();
        explorer.reveal(Path::new("/w/src/deep/main.rs"));
        // Both directories are open and both have been asked for, even though
        // neither has been answered.
        assert_eq!(explorer.wanted().as_deref(), Some(Path::new("/w/src")));

        explorer.fill(Path::new("/w/src"), vec![Entry::dir("deep")]);
        assert_eq!(explorer.wanted().as_deref(), Some(Path::new("/w/src/deep")));
        explorer.fill(Path::new("/w/src/deep"), vec![Entry::file("main.rs")]);

        assert_eq!(explorer.wanted(), None);
        assert_eq!(
            explorer.selection().map(|r| r.path),
            Some(PathBuf::from("/w/src/deep/main.rs")),
            "the selection lands once the rows for it exist"
        );
    }

    #[test]
    fn a_reveal_that_cannot_be_satisfied_is_given_up_on() {
        let mut explorer = tree();
        explorer.reveal(Path::new("/w/src/gone.rs"));
        // The directory arrives and the file is not in it.
        explorer.fill(Path::new("/w/src"), vec![Entry::file("main.rs")]);
        assert_eq!(explorer.selection().unwrap().name, "src");

        // A later listing must not be hijacked by the abandoned reveal.
        explorer.select_next();
        let before = explorer.selection().unwrap().path;
        explorer.fill(
            Path::new("/w"),
            vec![Entry::dir("src"), Entry::file("a.rs")],
        );
        assert_eq!(explorer.selection().unwrap().path, before);
    }

    #[test]
    fn revealing_something_outside_the_workspace_does_nothing() {
        let mut explorer = tree();
        let before = explorer.rows();
        explorer.reveal(Path::new("/elsewhere/main.rs"));
        assert_eq!(explorer.rows(), before);
    }

    #[test]
    fn scrolling_follows_the_selection_in_both_directions() {
        let mut explorer = Explorer::new("/w");
        explorer.fill(
            Path::new("/w"),
            (0..50)
                .map(|i| Entry::file(&format!("f{i:02}.rs")))
                .collect(),
        );
        explorer.select_last();
        explorer.scroll_into_view(10);
        assert_eq!(explorer.scroll(), 40);
        assert_eq!(explorer.visible(10).len(), 10);

        explorer.select_first();
        explorer.scroll_into_view(10);
        assert_eq!(explorer.scroll(), 0);
    }

    #[test]
    fn an_empty_workspace_is_not_the_same_as_an_unread_one() {
        let mut explorer = Explorer::new("/w");
        assert!(!explorer.loaded(), "nobody has read it yet");
        explorer.fill(Path::new("/w"), Vec::new());
        assert!(explorer.loaded(), "read, and it is empty");
        assert!(explorer.is_empty());
        assert_eq!(explorer.wanted(), None);
    }

    #[test]
    fn a_refresh_can_reach_a_whole_subtree() {
        let mut explorer = tree();
        explorer.select_first();
        explorer.expand();
        explorer.fill(Path::new("/w/src"), vec![Entry::dir("deep")]);
        explorer.select_next();
        explorer.expand();
        explorer.fill(Path::new("/w/src/deep"), vec![Entry::file("main.rs")]);
        assert_eq!(explorer.wanted(), None);

        // Something removed part of `src`. Re-reading only its parent would
        // rediscover `src` and leave these listings describing files that have
        // gone.
        explorer.invalidate_under(Path::new("/w/src"));
        assert_eq!(explorer.wanted().as_deref(), Some(Path::new("/w/src")));
        explorer.fill(Path::new("/w/src"), vec![Entry::dir("deep")]);
        assert_eq!(
            explorer.wanted().as_deref(),
            Some(Path::new("/w/src/deep")),
            "the expanded listing below it is stale too"
        );
    }

    #[test]
    fn a_refresh_asks_for_the_directory_again() {
        let mut explorer = tree();
        assert_eq!(explorer.wanted(), None);
        explorer.invalidate(Path::new("/w"));
        assert_eq!(explorer.wanted().as_deref(), Some(Path::new("/w")));
    }
}
