//! The problems view: every diagnostic a server has published, for every file.
//!
//! The session's `diagnostics` field holds the problems of the document on
//! screen, which the gutter and `F8` use. This list holds them for the whole
//! workspace, including files that are not open, because a server reports on
//! files the user has not opened, and the point of the view is to find those.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use deco_lsp::Diagnostic;

/// One row of the problems view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Row<'a> {
    /// The file the problem is in.
    pub path: &'a Path,
    /// The problem.
    pub diagnostic: &'a Diagnostic,
}

/// Every published diagnostic, by file, and the row the selection is on.
#[derive(Debug, Default)]
pub struct Markers {
    /// Ordered by path, so the view lists files in a stable order rather than
    /// in the order a server happened to publish them.
    by_path: BTreeMap<PathBuf, Vec<Diagnostic>>,
    selected: usize,
}

impl Markers {
    /// Replaces the problems of one file, as `publishDiagnostics` does. An
    /// empty list removes the file from the view.
    pub fn set(&mut self, path: PathBuf, mut diagnostics: Vec<Diagnostic>) {
        if diagnostics.is_empty() {
            self.by_path.remove(&path);
        } else {
            // In file order, which is also the order `F8` visits them in.
            diagnostics.sort_by_key(|d| (d.range.start.line, d.range.start.character, d.severity));
            self.by_path.insert(path, diagnostics);
        }
        self.clamp();
    }

    /// Removes every problem, when the server that published them stops.
    pub fn clear(&mut self) {
        self.by_path.clear();
        self.selected = 0;
    }

    /// The rows, file by file and in file order within a file.
    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> {
        self.by_path.iter().flat_map(|(path, diagnostics)| {
            diagnostics.iter().map(move |diagnostic| Row {
                path: path.as_path(),
                diagnostic,
            })
        })
    }

    /// How many problems there are in all.
    pub fn len(&self) -> usize {
        self.by_path.values().map(Vec::len).sum()
    }

    /// Whether there are no problems.
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// The index of the selected row.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The selected row, if there are any rows.
    pub fn selection(&self) -> Option<Row<'_>> {
        self.rows().nth(self.selected)
    }

    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.len().saturating_sub(1));
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.len().saturating_sub(1);
    }

    /// Keeps the selection on a row after the list shrinks.
    fn clamp(&mut self) {
        self.selected = self.selected.min(self.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deco_core::position::{Position, Range};
    use deco_lsp::Severity;

    fn problem(line: u32, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 0), Position::new(line, 1)),
            severity,
            code: None,
            source: None,
            message: message.to_owned(),
        }
    }

    fn messages(markers: &Markers) -> Vec<String> {
        markers
            .rows()
            .map(|row| format!("{}:{}", row.path.display(), row.diagnostic.message))
            .collect()
    }

    #[test]
    fn files_are_listed_by_path_and_problems_by_position() {
        let mut markers = Markers::default();
        markers.set(
            "/w/b.rs".into(),
            vec![
                problem(9, Severity::Warning, "late"),
                problem(1, Severity::Error, "early"),
            ],
        );
        markers.set("/w/a.rs".into(), vec![problem(3, Severity::Hint, "a")]);
        assert_eq!(
            messages(&markers),
            ["/w/a.rs:a", "/w/b.rs:early", "/w/b.rs:late"]
        );
        assert_eq!(markers.len(), 3);
    }

    #[test]
    fn a_new_publication_replaces_a_file_s_problems_and_an_empty_one_removes_them() {
        let mut markers = Markers::default();
        markers.set("/w/a.rs".into(), vec![problem(0, Severity::Error, "old")]);
        markers.set("/w/a.rs".into(), vec![problem(0, Severity::Error, "new")]);
        assert_eq!(messages(&markers), ["/w/a.rs:new"]);
        markers.set("/w/a.rs".into(), Vec::new());
        assert!(markers.is_empty());
    }

    #[test]
    fn the_selection_stays_on_a_row_when_the_list_shrinks() {
        let mut markers = Markers::default();
        markers.set(
            "/w/a.rs".into(),
            (0..3)
                .map(|line| problem(line, Severity::Error, "x"))
                .collect(),
        );
        markers.select_last();
        assert_eq!(markers.selected(), 2);
        markers.set("/w/a.rs".into(), vec![problem(0, Severity::Error, "x")]);
        assert_eq!(markers.selected(), 0);
        markers.select_next();
        assert_eq!(markers.selected(), 0, "no row past the end");
        markers.clear();
        assert!(markers.selection().is_none());
    }
}
