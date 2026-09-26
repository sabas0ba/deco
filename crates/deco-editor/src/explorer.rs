//! The file tree: its cached listings, the rows it shows, and the listings it
//! requests.
//!
//! # Fed, not reading
//!
//! There is no `read_dir` in this file. The core has no filesystem access: a
//! document receives its text and never reads a path. This allows the editable
//! surface to be tested without a filesystem. The tree follows the same rule.
//! It holds the directory contents it has been given, and when it needs to show
//! a directory it has no contents for, it requests them through
//! [`Explorer::wanted`] so that the frontend can read them.
//!
//! This design also makes the tree work on a remote workspace: `deco-remote`
//! answers the request over the connection instead of through `std::fs`, and
//! nothing here changes. Quick open already lists a remote workspace this way.
//!
//! # Bounded by the window
//!
//! A directory is read when it is first expanded, not before, so the cost
//! depends on the *visible* rows rather than on the workspace size.
//! [`Explorer::rows`] walks only the expanded parts, and the caller takes the
//! slice that fits the side bar.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One entry a directory listing reports.
///
/// Holds only what the tree draws and sorts by. Size, permissions and times are
/// omitted because nothing shows them, and storing them would require a policy
/// for how stale they may be.
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
    /// Whether it is expanded. Not meaningful for a file.
    pub expanded: bool,
    /// Whether the selection is on it.
    pub selected: bool,
}

/// The cached contents of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Contents {
    /// Requested, not yet received.
    Pending,
    /// Received listing.
    Known(Vec<Entry>),
}

/// The workspace tree.
///
/// Expansion and selection are stored here rather than in a frontend, because
/// both persist across redraws and neither is specific to a terminal. Two
/// frontends draw this tree from one shared model.
#[derive(Debug, Clone)]
pub struct Explorer {
    /// The workspace root. The root itself has no row. The tree shows what is
    /// *in* the workspace, and a root row that cannot be collapsed would waste a
    /// row.
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
    /// A path [`Explorer::reveal`] has not yet selected.
    ///
    /// Revealing a file expands the directories above it, and those are read
    /// one listing at a time, so the target row usually does not exist yet when
    /// `reveal` is called. Storing the path here lets [`Explorer::fill`] select
    /// the row when it appears. This is required to reveal the file deco was
    /// started with, because at that point the tree has no listings.
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
    /// Returns one directory at a time, and only directories that are shown. An
    /// expanded directory inside a collapsed one is not on screen, so it is not
    /// read yet. The caller answers with [`Explorer::fill`] and calls this
    /// again. A directory with expanded descendants therefore takes one round
    /// per level, so that no more is read than the window needs.
    pub fn wanted(&self) -> Option<PathBuf> {
        if matches!(self.listings.get(&self.root), Some(Contents::Pending)) {
            return Some(self.root.clone());
        }
        // `set_expanded` always adds an expanded directory to the map, so
        // `Pending` covers every requested and unanswered directory.
        self.rows()
            .into_iter()
            .find(|row| {
                row.expanded && matches!(self.listings.get(&row.path), Some(Contents::Pending))
            })
            .map(|row| row.path)
    }

    /// Records what `dir` contains.
    ///
    /// Entries are sorted here rather than kept in the caller's order, because
    /// `read_dir` returns no defined order and the tree should not reorder on
    /// each read. Directories come first, then entries are sorted
    /// case-insensitively by name, as in VS Code's explorer and common file
    /// managers.
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
    /// The tree cannot detect new files on disk because there is no file
    /// watcher, which would need separate platform-specific work. Refresh is
    /// implemented with this method.
    pub fn invalidate(&mut self, dir: &Path) {
        if self.listings.contains_key(dir) {
            self.listings.insert(dir.to_path_buf(), Contents::Pending);
        }
    }

    /// Forgets the contents of `prefix` and of everything under it.
    ///
    /// Used after a partially completed recursive delete. The directory itself
    /// may remain, so re-reading only its *parent* would find it again and keep
    /// its cached listing, and every expanded listing below it, describing files
    /// that no longer exist.
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

    /// Drops everything the tree remembers at or under `prefix`.
    ///
    /// Used for a path that has been removed, not only changed. Invalidating
    /// would keep the expansion state and the map entry, so a directory later
    /// created with the same name would appear already expanded and, until its
    /// listing arrived, would show the rows of the deleted directory.
    pub fn forget_under(&mut self, prefix: &Path) {
        self.listings.retain(|dir, _| !dir.starts_with(prefix));
        self.expanded.retain(|dir| !dir.starts_with(prefix));
        if self
            .revealing
            .as_deref()
            .is_some_and(|p| p.starts_with(prefix))
        {
            self.revealing = None;
        }
        self.clamp();
    }

    /// Moves what the tree remembers about `from` to `to`.
    ///
    /// A rename does not change what is *in* a directory, and a listing holds
    /// names rather than paths, so only the key needs to change. Re-keying
    /// keeps a renamed directory expanded with its rows, and leaves no state
    /// under the old name for a new entry with that name to inherit.
    pub fn rekey_under(&mut self, from: &Path, to: &Path) {
        let moved: Vec<PathBuf> = self
            .listings
            .keys()
            .filter(|dir| dir.starts_with(from))
            .cloned()
            .collect();
        for old in moved {
            let Ok(rest) = old.strip_prefix(from) else {
                continue;
            };
            let new = if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            };
            if let Some(contents) = self.listings.remove(&old) {
                self.listings.insert(new, contents);
            }
        }
        for dir in self.expanded.iter_mut() {
            if let Ok(rest) = dir.strip_prefix(from) {
                *dir = if rest.as_os_str().is_empty() {
                    to.to_path_buf()
                } else {
                    to.join(rest)
                };
            }
        }
        self.clamp();
    }

    /// Every path the tree knows about at or under `prefix`.
    ///
    /// Includes only what has been read (expanded directories and their
    /// entries), so the result is bounded by what has been shown rather than by
    /// the workspace size. A caller with filesystem access uses this to check
    /// whether any of these paths were removed.
    pub fn known_paths_under(&self, prefix: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (dir, contents) in &self.listings {
            if !dir.starts_with(prefix) {
                continue;
            }
            if dir != prefix {
                out.push(dir.clone());
            }
            if let Contents::Known(entries) = contents {
                out.extend(entries.iter().map(|entry| dir.join(&entry.name)));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Whether the root's listing has arrived.
    ///
    /// Distinguishes an empty workspace from one that has not been read yet.
    /// The side bar shows different text for each case.
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
    /// Right-arrow in VS Code's explorer does both, so repeated presses move
    /// down through the tree.
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
        // Move to the parent, the counterpart of `expand` stepping in.
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
    /// The directories are expanded whether or not their contents are known,
    /// and the selection is *stored* rather than applied, because the row for
    /// `path` usually does not exist until the requested listings arrive.
    /// [`Explorer::fill`] applies it when they do. This allows revealing a file
    /// the tree has no listing for, which is every file at startup.
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
    /// Cancels the reveal when the parent directory has been read and the path
    /// is not in it, for example because the file was deleted or is not
    /// reported by the listing. Otherwise an unsatisfiable reveal would remain
    /// pending and move the selection whenever a later listing arrives.
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
    /// Called by the code that knows the side bar's height.
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
    /// Collapsing a directory can remove the selected row. A selection past the
    /// end highlights nothing, which looks as if the tree lost focus.
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
        // No pending request: the root is loaded and nothing else is shown.
        assert_eq!(explorer.wanted(), None);

        explorer.select_next(); // src, Cargo.toml, README.md — dirs first.
        explorer.select_first();
        assert_eq!(explorer.selection().unwrap().name, "src");
        explorer.expand();

        // It is now shown and not loaded, so the tree requests it.
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
        // Loaded but not expanded: the thousand files add no rows.
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

        // The selected row is inside the directory being collapsed.
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
        // Both directories are expanded and requested, although neither
        // listing has arrived.
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

        // The cancelled reveal must not change the selection on a later
        // listing.
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
    fn a_stale_subtree_is_re_read_rather_than_shown_again() {
        // State after a failed rename: the source directory's listing is still
        // `Known`, so expanding it requests nothing and shows the old rows.
        let mut explorer = tree();
        explorer.select_first();
        explorer.expand();
        explorer.fill(Path::new("/w/src"), vec![Entry::file("old.rs")]);
        assert!(explorer.rows().iter().any(|r| r.name == "old.rs"));
        assert_eq!(explorer.wanted(), None, "nothing is outstanding");

        explorer.invalidate_under(Path::new("/w/src"));
        assert_eq!(
            explorer.wanted().as_deref(),
            Some(Path::new("/w/src")),
            "it has to be asked for again before its rows can be believed"
        );
        explorer.fill(Path::new("/w/src"), Vec::new());
        assert!(
            !explorer.rows().iter().any(|r| r.name == "old.rs"),
            "and the answer replaces them"
        );
        assert!(
            explorer
                .rows()
                .iter()
                .any(|r| r.name == "src" && r.expanded),
            "while the directory stays open — invalidating is not forgetting"
        );
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

        // Part of `src` was removed. Re-reading only its parent would find
        // `src` again and keep these listings for files that no longer exist.
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
    fn a_deleted_directory_leaves_nothing_for_its_name_to_inherit() {
        let mut explorer = tree();
        explorer.select_first(); // src
        explorer.expand();
        explorer.fill(Path::new("/w/src"), vec![Entry::file("old.rs")]);
        assert!(explorer.rows().iter().any(|r| r.name == "old.rs"));

        explorer.forget_under(Path::new("/w/src"));
        // The parent is re-read and a *new* `src` appears, empty.
        explorer.fill(Path::new("/w"), vec![Entry::dir("src")]);
        assert!(
            !explorer.rows().iter().any(|r| r.name == "old.rs"),
            "a directory created with the old name must not show the old one's rows"
        );
        let src = explorer
            .rows()
            .into_iter()
            .find(|r| r.name == "src")
            .expect("it is there");
        assert!(!src.expanded, "nor come back already open");
    }

    #[test]
    fn a_renamed_directory_keeps_its_rows_and_its_expansion() {
        let mut explorer = tree();
        explorer.select_first();
        explorer.expand();
        explorer.fill(Path::new("/w/src"), vec![Entry::file("main.rs")]);

        explorer.rekey_under(Path::new("/w/src"), Path::new("/w/source"));
        explorer.fill(
            Path::new("/w"),
            vec![Entry::dir("source"), Entry::file("Cargo.toml")],
        );

        let rows: Vec<_> = explorer.rows().into_iter().map(|r| r.name).collect();
        assert!(rows.contains(&"source".to_owned()));
        assert!(
            rows.contains(&"main.rs".to_owned()),
            "the contents came with it — a rename does not change them"
        );
        assert_eq!(explorer.wanted(), None, "and nothing needs re-reading");
    }

    #[test]
    fn what_the_tree_knows_is_bounded_by_what_was_read() {
        let mut explorer = tree();
        // Nothing expanded: the root's entries are all it knows.
        let known = explorer.known_paths_under(Path::new("/w"));
        assert!(known.contains(&PathBuf::from("/w/src")));
        assert!(known.contains(&PathBuf::from("/w/Cargo.toml")));
        // `starts_with` is component-based, so `/w/src` matches the prefix
        // `/w/src`. The check is whether any path *inside* it is known.
        assert!(
            !known
                .iter()
                .any(|p| p.parent() == Some(Path::new("/w/src"))),
            "a directory nobody opened has not been read, so nothing under it \
             is known"
        );

        explorer.select_first();
        explorer.expand();
        explorer.fill(Path::new("/w/src"), vec![Entry::file("main.rs")]);
        assert!(explorer
            .known_paths_under(Path::new("/w/src"))
            .contains(&PathBuf::from("/w/src/main.rs")));
    }

    #[test]
    fn a_refresh_asks_for_the_directory_again() {
        let mut explorer = tree();
        assert_eq!(explorer.wanted(), None);
        explorer.invalidate(Path::new("/w"));
        assert_eq!(explorer.wanted().as_deref(), Some(Path::new("/w")));
    }
}
