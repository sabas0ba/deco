//! Which lines changed, for the gutter.
//!
//! Pure, like [`crate::status`]: two strings in, a list of hunks out. It uses no
//! process and no filesystem. This matters here because the compared text is
//! the editor buffer, which is not on disk. Using git to compare two files
//! would require writing the unsaved text to disk on every keystroke that
//! changes a line. Comparing the saved file against `HEAD` would show stale
//! marks that do not match the screen.
//!
//! Git is therefore used only to obtain the committed text of a path, and the
//! comparison happens here.
//!
//! # The algorithm, and why it has a limit
//!
//! Myers' greedy diff, the algorithm `git diff` uses. It runs in O(*n* × *d*),
//! where *d* is the number of edits. This is fast when there are few edits, the
//! usual case for a gutter, and quadratic when there are many. Two measures
//! bound the cost:
//!
//! - **The common prefix and suffix are removed first.** Typing on line 400 of
//!   a thousand-line file leaves only a few lines in the middle to compare,
//!   regardless of file size.
//! - **The search stops at [`MAX_EDITS`].** A file replaced wholesale has an
//!   edit distance in the thousands, and the gutter marks are not useful; the
//!   full search would take seconds. Past the limit, the middle becomes one
//!   modified hunk and [`Diff::truncated`] is set, so the approximation is
//!   visible to callers.

use std::ops::Range;

/// Maximum edit distance searched before returning an approximate diff.
///
/// If the search exceeds this limit, the unmatched middle is returned as one
/// modified hunk and `Diff::truncated` is set. Common prefix and suffix lines
/// remain excluded from that hunk.
pub const MAX_EDITS: usize = 2_000;

/// What the gutter should draw beside a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Lines that are not in the committed text.
    Added,
    /// Lines that exist in both texts with different content.
    Modified,
    /// Lines that were removed. Removed lines cannot be marked, so the mark is
    /// placed on the line that now occupies their position.
    Deleted,
}

/// One run of lines that differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The replaced range in the committed text. Empty for a pure addition.
    pub head: Range<usize>,
    /// The current range in the working text. Empty for a pure deletion.
    pub working: Range<usize>,
}

impl Hunk {
    /// The gutter mark for this hunk.
    ///
    /// A replaced line is marked as modified rather than as an addition plus a
    /// deletion. This matches VS Code and represents one edit as one mark.
    pub fn mark(&self) -> Mark {
        match (self.head.is_empty(), self.working.is_empty()) {
            (true, _) => Mark::Added,
            (false, true) => Mark::Deleted,
            (false, false) => Mark::Modified,
        }
    }
}

/// Every difference between the committed text and the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// In order, by position in the working text.
    pub hunks: Vec<Hunk>,
    /// Whether the search gave up and collapsed the middle into one hunk.
    ///
    /// Exposed so that a caller drawing a whole region as modified can explain
    /// why, and a test can distinguish this case from a real rewrite.
    pub truncated: bool,
}

impl Diff {
    /// A diff with no differences.
    fn same() -> Self {
        Self {
            hunks: Vec::new(),
            truncated: false,
        }
    }

    /// Whether the buffer matches what was committed.
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// The mark for `line` of the working text, if any.
    ///
    /// A deletion is reported at `hunk.working.start`, the line that took the
    /// removed lines' position, even though that range is empty. When a
    /// deletion and a change fall on the same line, the change takes precedence
    /// because it describes the line itself.
    pub fn mark_at(&self, line: usize) -> Option<Mark> {
        let mut deleted = None;
        for hunk in &self.hunks {
            if hunk.working.contains(&line) {
                return Some(hunk.mark());
            }
            if hunk.working.is_empty() && hunk.working.start == line {
                deleted = Some(Mark::Deleted);
            }
        }
        deleted
    }
}

/// Compares the committed text with the buffer.
///
/// Lines keep their terminators, so a file without its final newline differs
/// from one with it. This is a real change that also appears in a review diff.
pub fn diff(head: &str, working: &str) -> Diff {
    let a: Vec<&str> = head.split_inclusive('\n').collect();
    let b: Vec<&str> = working.split_inclusive('\n').collect();

    // Remove the common prefix and suffix before the expensive search, so the
    // cost depends on the size of the edit rather than the size of the file.
    let prefix = a
        .iter()
        .zip(b.iter())
        .take_while(|(one, two)| one == two)
        .count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(one, two)| one == two)
        .count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

    if a_mid.is_empty() && b_mid.is_empty() {
        return Diff::same();
    }
    // One side is empty, so the result is one hunk and no search is needed.
    if a_mid.is_empty() || b_mid.is_empty() {
        return Diff {
            hunks: vec![Hunk {
                head: prefix..prefix + a_mid.len(),
                working: prefix..prefix + b_mid.len(),
            }],
            truncated: false,
        };
    }

    match myers(a_mid, b_mid) {
        Some(script) => Diff {
            hunks: hunks(&script, prefix),
            truncated: false,
        },
        // Past the limit: one hunk, with `truncated` set to distinguish it from
        // a file that was rewritten line by line.
        None => Diff {
            hunks: vec![Hunk {
                head: prefix..prefix + a_mid.len(),
                working: prefix..prefix + b_mid.len(),
            }],
            truncated: true,
        },
    }
}

/// One step of the edit script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The line is in both.
    Same,
    /// The line is only in the committed text.
    Removed,
    /// The line is only in the buffer.
    Added,
}

/// Groups an edit script into runs of lines that differ.
fn hunks(script: &[Step], offset: usize) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    let (mut head, mut working) = (offset, offset);
    let mut current: Option<Hunk> = None;
    for step in script {
        match step {
            Step::Same => {
                if let Some(hunk) = current.take() {
                    out.push(hunk);
                }
                head += 1;
                working += 1;
            }
            Step::Removed => {
                let hunk = current.get_or_insert(Hunk {
                    head: head..head,
                    working: working..working,
                });
                head += 1;
                hunk.head.end = head;
            }
            Step::Added => {
                let hunk = current.get_or_insert(Hunk {
                    head: head..head,
                    working: working..working,
                });
                working += 1;
                hunk.working.end = working;
            }
        }
    }
    out.extend(current);
    out
}

/// Myers' greedy diff. `None` once the edit distance passes [`MAX_EDITS`].
///
/// The `v` array holds, for each diagonal `k = x - y`, the furthest `x` reached
/// with `d` edits. A copy is kept for each `d` so the path can be traced back
/// afterwards. The limit mainly bounds this memory.
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Step>> {
    let (n, m) = (a.len(), b.len());
    let limit = MAX_EDITS.min(n + m);
    let offset = limit as isize;
    let width = 2 * limit + 1;

    let mut v = vec![0isize; width];
    let mut trace: Vec<Vec<isize>> = Vec::with_capacity(limit + 1);

    for d in 0..=limit {
        trace.push(v.clone());
        let d = d as isize;
        let mut k = -d;
        while k <= d {
            let at = (k + offset) as usize;
            // Move down (an insertion) when the diagonal below has reached
            // further, or when there is no diagonal above to move across from.
            let mut x = if k == -d || (k != d && v[at - 1] < v[at + 1]) {
                v[at + 1]
            } else {
                v[at - 1] + 1
            };
            let mut y = x - k;
            // Then follow the diagonal while the lines match. This is the
            // greedy step, and it keeps the cost of a small edit low.
            while (x as usize) < n && (y as usize) < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[at] = x;
            if x as usize >= n && y as usize >= m {
                return Some(walk_back(&trace, offset, n, m));
            }
            k += 2;
        }
    }
    None
}

/// Reconstructs the edit script from the recorded search.
///
/// The trace is walked backwards from the end point, and the result is reversed
/// at the end.
fn walk_back(trace: &[Vec<isize>], offset: isize, n: usize, m: usize) -> Vec<Step> {
    let mut script = Vec::new();
    let (mut x, mut y) = (n as isize, m as isize);

    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let k = x - y;
        let at = (k + offset) as usize;
        let previous = if k == -d || (k != d && v[at - 1] < v[at + 1]) {
            k + 1
        } else {
            k - 1
        };
        let previous_x = v[(previous + offset) as usize];
        let previous_y = previous_x - previous;

        while x > previous_x && y > previous_y {
            script.push(Step::Same);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == previous_x {
                script.push(Step::Added);
                y -= 1;
            } else {
                script.push(Step::Removed);
                x -= 1;
            }
        }
    }

    script.reverse();
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gutter marks, one character per line, with `.` for an unmarked
    /// line. This makes the expected output easy to read in the tests.
    fn gutter(head: &str, working: &str) -> String {
        let diff = diff(head, working);
        let lines = working.split_inclusive('\n').count();
        // One past the last line, so a deletion at the very end is visible.
        (0..=lines)
            .map(|line| match diff.mark_at(line) {
                Some(Mark::Added) => '+',
                Some(Mark::Modified) => '~',
                Some(Mark::Deleted) => '-',
                None => '.',
            })
            .collect()
    }

    #[test]
    fn an_unchanged_file_has_nothing_beside_it() {
        let text = "one\ntwo\nthree\n";
        assert!(diff(text, text).is_empty());
        assert_eq!(gutter(text, text), "....");
    }

    #[test]
    fn an_inserted_line_is_an_addition() {
        assert_eq!(gutter("one\ntwo\n", "one\nnew\ntwo\n"), ".+..");
    }

    #[test]
    fn a_replaced_line_is_modified_rather_than_both() {
        // Reporting an addition plus a deletion would draw two marks for one
        // edited line.
        assert_eq!(gutter("one\ntwo\nthree\n", "one\nTWO\nthree\n"), ".~..");
    }

    #[test]
    fn a_removed_line_marks_where_it_was() {
        // The removed line cannot be marked, so the mark goes on the line that
        // took its place.
        assert_eq!(gutter("one\ntwo\nthree\n", "one\nthree\n"), ".-.");
    }

    #[test]
    fn a_removal_at_the_end_still_has_somewhere_to_go() {
        assert_eq!(gutter("one\ntwo\n", "one\n"), ".-");
    }

    #[test]
    fn a_lost_final_newline_is_a_change() {
        // A real change that appears in a review diff and is often
        // unintentional.
        assert_eq!(gutter("one\ntwo\n", "one\ntwo"), ".~.");
    }

    #[test]
    fn an_empty_file_that_gained_everything_is_all_addition() {
        assert_eq!(gutter("", "one\ntwo\n"), "++.");
    }

    #[test]
    fn a_file_emptied_marks_its_first_line() {
        assert_eq!(gutter("one\ntwo\n", ""), "-");
    }

    #[test]
    fn edits_far_apart_stay_apart() {
        let head = "a\nb\nc\nd\ne\nf\ng\n";
        let working = "a\nB\nc\nd\ne\nF\ng\n";
        assert_eq!(gutter(head, working), ".~...~..");
    }

    #[test]
    fn a_move_reads_as_a_removal_and_an_addition() {
        // Neither Myers nor `git diff` detects moved lines. This test records
        // that behaviour so that changing it is a deliberate decision.
        let diff = diff("a\nb\nc\n", "b\nc\na\n");
        assert!(!diff.is_empty());
        assert!(!diff.truncated);
    }

    #[test]
    fn the_common_ends_are_not_searched() {
        // A thousand identical lines around a one-line edit. If the prefix and
        // suffix were searched, the cost would be much higher, and past
        // `MAX_EDITS` the result would be truncated.
        let mut head = String::new();
        let mut working = String::new();
        for n in 0..4_000 {
            head.push_str(&format!("line {n}\n"));
            working.push_str(&format!("line {n}\n"));
        }
        head.push_str("before\n");
        working.push_str("after\n");

        let diff = diff(&head, &working);
        assert!(!diff.truncated, "the ends should never have been searched");
        assert_eq!(
            diff.hunks,
            vec![Hunk {
                head: 4_000..4_001,
                working: 4_000..4_001,
            }]
        );
    }

    #[test]
    fn a_file_replaced_wholesale_says_so_rather_than_taking_forever() {
        // Every line differs, far past the limit. The result is one hunk, and
        // `truncated` distinguishes it from a file rewritten line by line.
        let head: String = (0..MAX_EDITS + 500).map(|n| format!("old {n}\n")).collect();
        let working: String = (0..MAX_EDITS + 500).map(|n| format!("new {n}\n")).collect();

        let diff = diff(&head, &working);
        assert!(diff.truncated, "the search should have given up");
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].mark(), Mark::Modified);
    }

    #[test]
    fn hunks_come_back_in_order() {
        let head = "a\nb\nc\nd\ne\n";
        let working = "a\nX\nc\nd\nY\nZ\n";
        let diff = diff(head, working);
        let starts: Vec<usize> = diff.hunks.iter().map(|hunk| hunk.working.start).collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted, "a renderer walks these alongside the rows");
    }
}
