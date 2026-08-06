//! Diagnostics pushed by a server, and deciding which of them still apply.
//!
//! A server publishes diagnostics for a whole document at a time: each
//! `textDocument/publishDiagnostics` replaces everything previously known about
//! that URI, rather than adding to it. An empty list is therefore meaningful —
//! it is how a server says "the errors are gone" — and dropping it as
//! uninteresting leaves stale red squiggles on screen forever.
//!
//! The subtler problem is ordering. Analysis takes time, so a result computed
//! against version 4 can arrive after the user has typed their way to version
//! 7. Its ranges refer to text that no longer exists, and showing it puts
//! errors under the wrong characters. Servers that support it stamp the
//! publication with the version it was computed from, which is why the client
//! asks for `versionSupport`; [`DiagnosticStore::publish`] uses it to discard
//! what arrives late.

use std::collections::HashMap;

use deco_core::position::{Position, Range};
use serde::{Deserialize, Serialize};

use crate::uri::Uri;

/// How serious a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Severity {
    /// Something is wrong. Ordered first so that the worst problem on a line
    /// is the one whose colour the line gets.
    #[default]
    Error,
    /// Something is suspicious.
    Warning,
    /// Something is worth knowing.
    Information,
    /// A gentle suggestion, usually rendered as a faint underline.
    Hint,
}

impl Severity {
    /// Reads the protocol's numeric encoding.
    ///
    /// An unknown or absent value becomes [`Severity::Error`], following the
    /// specification: a server that does not classify its diagnostics is
    /// reporting problems, and under-reporting severity would hide them.
    pub fn from_number(value: Option<i64>) -> Self {
        match value {
            Some(2) => Self::Warning,
            Some(3) => Self::Information,
            Some(4) => Self::Hint,
            _ => Self::Error,
        }
    }

    /// The protocol's numeric encoding.
    pub fn as_number(self) -> i64 {
        match self {
            Self::Error => 1,
            Self::Warning => 2,
            Self::Information => 3,
            Self::Hint => 4,
        }
    }
}

/// One problem reported against a range of a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Where the problem is.
    pub range: Range,
    /// How serious it is.
    #[serde(skip)]
    pub severity: Severity,
    /// The server's identifier for this class of problem, e.g. `E0308`. Either
    /// a string or a number on the wire.
    #[serde(skip)]
    pub code: Option<String>,
    /// Which tool produced it, e.g. `rustc` or `clippy`. Servers that front
    /// several tools use this to say which one spoke.
    #[serde(skip)]
    pub source: Option<String>,
    /// The text to show.
    pub message: String,
}

impl Diagnostic {
    /// Reads one diagnostic from a `publishDiagnostics` array.
    ///
    /// Returns `None` only when there is no usable range, since a diagnostic
    /// that cannot be placed cannot be drawn. Everything else degrades: a
    /// missing message becomes empty, an unknown severity becomes an error.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            range: read_range(value.get("range")?)?,
            severity: Severity::from_number(value.get("severity").and_then(|v| v.as_i64())),
            // `code` is `integer | string` in the protocol, and both appear:
            // rust-analyzer sends strings, several others send numbers.
            code: value.get("code").and_then(|code| match code {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            }),
            source: value
                .get("source")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            message: value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// Whether this diagnostic covers a position, treating its range as
    /// half-open — except for an empty range, which still has to be hoverable.
    pub fn contains(&self, position: Position) -> bool {
        if self.range.is_empty() {
            return position == self.range.start;
        }
        position >= self.range.start && position < self.range.end
    }

    /// A one-line rendering, e.g. `rustc[E0308]: mismatched types`.
    pub fn label(&self) -> String {
        match (&self.source, &self.code) {
            (Some(source), Some(code)) => format!("{source}[{code}]: {}", self.message),
            (Some(source), None) => format!("{source}: {}", self.message),
            (None, Some(code)) => format!("[{code}]: {}", self.message),
            (None, None) => self.message.clone(),
        }
    }
}

fn read_range(value: &serde_json::Value) -> Option<Range> {
    Some(Range::new(
        read_position(value.get("start")?)?,
        read_position(value.get("end")?)?,
    ))
}

fn read_position(value: &serde_json::Value) -> Option<Position> {
    // Negative or absent coordinates are clamped to zero rather than rejected:
    // a server that miscounts should cost one misplaced squiggle, not the whole
    // document's diagnostics.
    let line = value.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
    let character = value.get("character").and_then(|v| v.as_i64()).unwrap_or(0);
    Some(Position::new(
        line.clamp(0, u32::MAX as i64) as u32,
        character.clamp(0, u32::MAX as i64) as u32,
    ))
}

/// What a publication did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// The document's diagnostics were replaced.
    Replaced {
        /// How many are now shown.
        count: usize,
    },
    /// The publication was computed against an older version of the document
    /// and was discarded.
    Stale {
        /// The version the server used.
        published: i32,
        /// The version the editor has.
        current: i32,
    },
}

/// Every diagnostic one server has published, keyed by document.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticStore {
    by_uri: HashMap<Uri, Vec<Diagnostic>>,
}

impl DiagnosticStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a `publishDiagnostics` payload.
    ///
    /// `current_version` is the version the editor holds for that document, or
    /// `None` if it is not tracking one. A publication stamped with an older
    /// version is discarded; one with no stamp is trusted, because a server
    /// that does not report versions gives nothing better to go on.
    pub fn publish(
        &mut self,
        uri: Uri,
        version: Option<i32>,
        diagnostics: Vec<Diagnostic>,
        current_version: Option<i32>,
    ) -> Published {
        if let (Some(published), Some(current)) = (version, current_version) {
            if published < current {
                return Published::Stale { published, current };
            }
        }
        let count = diagnostics.len();
        if diagnostics.is_empty() {
            // Dropping the key rather than storing an empty vector, so that
            // `documents()` lists only what actually has problems.
            self.by_uri.remove(&uri);
        } else {
            self.by_uri.insert(uri, diagnostics);
        }
        Published::Replaced { count }
    }

    /// The diagnostics for a document, in the order the server sent them.
    pub fn for_uri(&self, uri: &Uri) -> &[Diagnostic] {
        self.by_uri.get(uri).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The diagnostics for a document, sorted by position and then by severity.
    ///
    /// Servers order their output however they like — often by the order
    /// analysis finished — which makes "the next error" unstable if used
    /// directly for navigation.
    pub fn sorted_for_uri(&self, uri: &Uri) -> Vec<&Diagnostic> {
        let mut sorted: Vec<&Diagnostic> = self.for_uri(uri).iter().collect();
        sorted.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then(a.severity.cmp(&b.severity))
                .then_with(|| a.message.cmp(&b.message))
        });
        sorted
    }

    /// The diagnostics covering a position, worst first.
    pub fn at(&self, uri: &Uri, position: Position) -> Vec<&Diagnostic> {
        let mut hits: Vec<&Diagnostic> = self
            .for_uri(uri)
            .iter()
            .filter(|d| d.contains(position))
            .collect();
        hits.sort_by_key(|d| d.severity);
        hits
    }

    /// How many diagnostics of each severity a document has.
    pub fn counts(&self, uri: &Uri) -> Counts {
        let mut counts = Counts::default();
        for diagnostic in self.for_uri(uri) {
            match diagnostic.severity {
                Severity::Error => counts.errors += 1,
                Severity::Warning => counts.warnings += 1,
                Severity::Information => counts.information += 1,
                Severity::Hint => counts.hints += 1,
            }
        }
        counts
    }

    /// Every document that currently has diagnostics.
    pub fn documents(&self) -> impl Iterator<Item = &Uri> {
        self.by_uri.keys()
    }

    /// Forgets a document's diagnostics, as when it is closed.
    pub fn clear(&mut self, uri: &Uri) {
        self.by_uri.remove(uri);
    }

    /// Forgets everything, as when a server exits.
    ///
    /// A dead server's diagnostics are not merely stale, they are unowned:
    /// nothing will ever correct or retract them.
    pub fn clear_all(&mut self) {
        self.by_uri.clear();
    }
}

/// A tally by severity, for a status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// Number of errors.
    pub errors: usize,
    /// Number of warnings.
    pub warnings: usize,
    /// Number of informational diagnostics.
    pub information: usize,
    /// Number of hints.
    pub hints: usize,
}

impl Counts {
    /// Whether there is nothing to report.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The total across every severity.
    pub fn total(&self) -> usize {
        self.errors + self.warnings + self.information + self.hints
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uri() -> Uri {
        Uri::from_string("file:///w/main.rs")
    }

    fn diagnostic(line: u32, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 0), Position::new(line, 5)),
            severity,
            code: None,
            source: None,
            message: message.into(),
        }
    }

    #[test]
    fn a_diagnostic_is_read_from_the_protocol_shape() {
        let parsed = Diagnostic::from_json(&json!({
            "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}},
            "severity": 2,
            "code": "E0308",
            "source": "rustc",
            "message": "mismatched types",
        }))
        .unwrap();

        assert_eq!(
            parsed.range,
            Range::new(Position::new(3, 4), Position::new(3, 9))
        );
        assert_eq!(parsed.severity, Severity::Warning);
        assert_eq!(parsed.code.as_deref(), Some("E0308"));
        assert_eq!(parsed.label(), "rustc[E0308]: mismatched types");
    }

    #[test]
    fn a_numeric_code_is_accepted_as_well_as_a_string() {
        // The protocol allows either, and both appear in the wild.
        let parsed = Diagnostic::from_json(&json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "code": 2304,
            "message": "cannot find name",
        }))
        .unwrap();
        assert_eq!(parsed.code.as_deref(), Some("2304"));
    }

    #[test]
    fn an_absent_severity_is_an_error() {
        // Per the specification. Guessing lower would hide real problems.
        assert_eq!(Severity::from_number(None), Severity::Error);
        assert_eq!(Severity::from_number(Some(99)), Severity::Error);
    }

    #[test]
    fn every_severity_round_trips_through_its_number() {
        for severity in [
            Severity::Error,
            Severity::Warning,
            Severity::Information,
            Severity::Hint,
        ] {
            assert_eq!(Severity::from_number(Some(severity.as_number())), severity);
        }
    }

    #[test]
    fn severities_order_worst_first() {
        let mut severities = vec![Severity::Hint, Severity::Error, Severity::Warning];
        severities.sort();
        assert_eq!(
            severities,
            vec![Severity::Error, Severity::Warning, Severity::Hint]
        );
    }

    #[test]
    fn a_diagnostic_without_a_range_is_dropped() {
        // It cannot be drawn anywhere, and placing it at the origin would put
        // an unrelated error on the first line.
        assert!(Diagnostic::from_json(&json!({"message": "somewhere"})).is_none());
    }

    #[test]
    fn a_missing_message_degrades_rather_than_dropping_the_diagnostic() {
        let parsed = Diagnostic::from_json(&json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}
        }))
        .unwrap();
        assert_eq!(parsed.message, "");
        assert_eq!(parsed.severity, Severity::Error);
    }

    #[test]
    fn a_negative_coordinate_is_clamped_rather_than_rejected() {
        let parsed = Diagnostic::from_json(&json!({
            "range": {"start": {"line": -1, "character": -5}, "end": {"line": 0, "character": 1}},
            "message": "x",
        }))
        .unwrap();
        assert_eq!(parsed.range.start, Position::ZERO);
    }

    #[test]
    fn publishing_replaces_rather_than_appends() {
        // The protocol is replace-per-document. Appending would double every
        // error the second time a file is analysed.
        let mut store = DiagnosticStore::new();
        store.publish(uri(), None, vec![diagnostic(0, Severity::Error, "a")], None);
        store.publish(uri(), None, vec![diagnostic(1, Severity::Error, "b")], None);

        assert_eq!(store.for_uri(&uri()).len(), 1);
        assert_eq!(store.for_uri(&uri())[0].message, "b");
    }

    #[test]
    fn an_empty_publication_clears_the_document() {
        // This is how a server says the errors are fixed. Ignoring it because
        // the list is empty leaves stale squiggles on screen permanently.
        let mut store = DiagnosticStore::new();
        store.publish(uri(), None, vec![diagnostic(0, Severity::Error, "a")], None);
        assert_eq!(
            store.publish(uri(), None, Vec::new(), None),
            Published::Replaced { count: 0 }
        );
        assert!(store.for_uri(&uri()).is_empty());
        assert_eq!(store.documents().count(), 0);
    }

    #[test]
    fn a_publication_from_an_older_version_is_discarded() {
        // Analysis of version 4 arriving after the user reached version 7:
        // its ranges point at text that no longer exists.
        let mut store = DiagnosticStore::new();
        store.publish(
            uri(),
            Some(7),
            vec![diagnostic(0, Severity::Error, "fresh")],
            Some(7),
        );

        assert_eq!(
            store.publish(
                uri(),
                Some(4),
                vec![diagnostic(9, Severity::Error, "stale")],
                Some(7)
            ),
            Published::Stale {
                published: 4,
                current: 7
            }
        );
        assert_eq!(
            store.for_uri(&uri())[0].message,
            "fresh",
            "the stale publication must not overwrite the current one"
        );
    }

    #[test]
    fn a_publication_for_the_current_version_is_applied() {
        let mut store = DiagnosticStore::new();
        assert_eq!(
            store.publish(
                uri(),
                Some(3),
                vec![diagnostic(0, Severity::Error, "a")],
                Some(3)
            ),
            Published::Replaced { count: 1 }
        );
    }

    #[test]
    fn an_unversioned_publication_is_trusted() {
        // A server that does not stamp versions offers nothing better to go on,
        // and discarding its output would mean showing no diagnostics at all.
        let mut store = DiagnosticStore::new();
        assert_eq!(
            store.publish(
                uri(),
                None,
                vec![diagnostic(0, Severity::Error, "a")],
                Some(9)
            ),
            Published::Replaced { count: 1 }
        );
    }

    #[test]
    fn diagnostics_sort_by_position_then_severity() {
        // Servers emit in whatever order analysis finished, which makes
        // "go to next error" jump around without this.
        let mut store = DiagnosticStore::new();
        store.publish(
            uri(),
            None,
            vec![
                diagnostic(5, Severity::Warning, "later"),
                diagnostic(1, Severity::Hint, "hint on 1"),
                diagnostic(1, Severity::Error, "error on 1"),
            ],
            None,
        );

        let sorted: Vec<&str> = store
            .sorted_for_uri(&uri())
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(sorted, vec!["error on 1", "hint on 1", "later"]);
    }

    #[test]
    fn the_diagnostics_under_the_cursor_come_back_worst_first() {
        let mut store = DiagnosticStore::new();
        store.publish(
            uri(),
            None,
            vec![
                diagnostic(2, Severity::Hint, "hint"),
                diagnostic(2, Severity::Error, "error"),
                diagnostic(8, Severity::Error, "elsewhere"),
            ],
            None,
        );

        let hits = store.at(&uri(), Position::new(2, 3));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].message, "error");
    }

    #[test]
    fn a_range_end_is_exclusive() {
        // Otherwise the diagnostic for `foo` also lights up the character after
        // it, and two adjacent diagnostics both claim the boundary.
        let d = diagnostic(0, Severity::Error, "x"); // characters 0..5
        assert!(d.contains(Position::new(0, 4)));
        assert!(!d.contains(Position::new(0, 5)));
        assert!(!d.contains(Position::new(1, 0)));
    }

    #[test]
    fn an_empty_range_is_still_reachable() {
        // Servers report "expected a semicolon here" as a zero-width range. A
        // strict half-open test would make it impossible to hover.
        let d = Diagnostic {
            range: Range::empty(Position::new(3, 7)),
            severity: Severity::Error,
            code: None,
            source: None,
            message: "expected `;`".into(),
        };
        assert!(d.contains(Position::new(3, 7)));
        assert!(!d.contains(Position::new(3, 8)));
    }

    #[test]
    fn counts_tally_by_severity() {
        let mut store = DiagnosticStore::new();
        store.publish(
            uri(),
            None,
            vec![
                diagnostic(0, Severity::Error, "a"),
                diagnostic(1, Severity::Error, "b"),
                diagnostic(2, Severity::Warning, "c"),
                diagnostic(3, Severity::Hint, "d"),
            ],
            None,
        );

        let counts = store.counts(&uri());
        assert_eq!(counts.errors, 2);
        assert_eq!(counts.warnings, 1);
        assert_eq!(counts.hints, 1);
        assert_eq!(counts.total(), 4);
        assert!(!counts.is_empty());
        assert!(store.counts(&Uri::from_string("file:///other")).is_empty());
    }

    #[test]
    fn documents_are_independent_and_clearable() {
        let mut store = DiagnosticStore::new();
        let a = Uri::from_string("file:///w/a.rs");
        let b = Uri::from_string("file:///w/b.rs");
        store.publish(
            a.clone(),
            None,
            vec![diagnostic(0, Severity::Error, "a")],
            None,
        );
        store.publish(
            b.clone(),
            None,
            vec![diagnostic(0, Severity::Error, "b")],
            None,
        );

        store.clear(&a);
        assert!(store.for_uri(&a).is_empty());
        assert_eq!(store.for_uri(&b).len(), 1);

        // A dead server's diagnostics are unowned: nothing will ever retract
        // them, so they go with it.
        store.clear_all();
        assert_eq!(store.documents().count(), 0);
    }

    #[test]
    fn a_label_degrades_when_the_server_omits_fields() {
        let mut d = diagnostic(0, Severity::Error, "boom");
        assert_eq!(d.label(), "boom");
        d.code = Some("E1".into());
        assert_eq!(d.label(), "[E1]: boom");
        d.source = Some("rustc".into());
        assert_eq!(d.label(), "rustc[E1]: boom");
        d.code = None;
        assert_eq!(d.label(), "rustc: boom");
    }
}
