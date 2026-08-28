//! Which lines changed, for the gutter.
//!
//! Pure, like [`crate::status`]: two strings in, a list of hunks out. No
//! process, no filesystem — which matters more here than usual, because the
//! text being compared is the *buffer*, and a buffer is not on disk. Handing
//! git two files to compare would mean writing the unsaved text somewhere
//! first, on every keystroke that moved a line; and comparing the saved file
//! against `HEAD` would show the marks the file deserved a moment ago rather
//! than the ones the screen does.
//!
//! So git is asked only for the committed text of a path, and the comparison
//! happens here.
//!
//! # The algorithm, and why it has a limit
//!
//! Myers' greedy diff, which is what `git diff` itself uses. It runs in
//! O(*n* × *d*) where *d* is the number of edits — excellent when the answer is
//! small, which is the case a gutter exists for, and quadratic when it is not.
//! Two things keep that in hand:
//!
//! - **The common prefix and suffix come off first.** Typing on line 400 of a
//!   thousand-line file leaves a handful of lines in the middle to compare,
//!   whatever the file's size.
//! - **The search gives up at [`MAX_EDITS`].** A file replaced wholesale has an
//!   edit distance in the thousands and no gutter worth drawing; chasing it
//!   would cost seconds. Past the limit the middle collapses into one modified
//!   hunk and [`Diff::truncated`] says so, rather than the marks quietly being
//!   approximate.

use std::ops::Range;

/// How hard the diff will look before saying the file is simply different.
///
/// Two thousand edits is far more than a person makes between two commits and
/// far less than a generated file being regenerated. The cost of being wrong in
/// either direction is small: below it the marks are exact, above it they are
/// one block and honest about it.
pub const MAX_EDITS: usize = 2_000;

/// What the gutter should draw beside a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Lines that are not in the committed text at all.
    Added,
    /// Lines that are there but say something else.
    Modified,
    /// Lines that were removed. There is nothing left to mark, so this belongs
    /// to the line that now sits where they were.
    Deleted,
}

/// One run of lines that differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// What it replaces in the committed text. Empty for a pure addition.
    pub head: Range<usize>,
    /// What is there now. Empty for a pure deletion.
    pub working: Range<usize>,
}

impl Hunk {
    /// What to draw for it.
    ///
    /// A line that was replaced is *modified* rather than an addition sitting
    /// on a deletion. That is what VS Code shows and what a reader means: one
    /// line was edited, not two things happened to it.
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
    /// Carried rather than hidden: a caller that draws a whole file as modified
    /// should be able to say why, and a test should be able to tell the two
    /// cases apart.
    pub truncated: bool,
}

impl Diff {
    /// Nothing differs.
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

    /// What to draw beside `line` of the working text, if anything.
    ///
    /// A deletion belongs to the line that took the removed lines' place, so it
    /// answers for `hunk.working.start` even though that range is empty. When
    /// a deletion and a change meet at the same line the change wins: the line
    /// really is there and really is different, and saying only that something
    /// vanished above it would be the less useful half.
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
/// Lines keep their terminators, so a file that lost its final newline differs
/// from one that has it — which is a real change, and one a reviewer will see.
pub fn diff(head: &str, working: &str) -> Diff {
    let a: Vec<&str> = head.split_inclusive('\n').collect();
    let b: Vec<&str> = working.split_inclusive('\n').collect();

    // The common ends come off before anything expensive happens. This is what
    // makes an edit in a large file cost what the edit is worth rather than
    // what the file is.
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
    // One side gone entirely: no search can say anything the ends have not.
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
        // Past the limit. One block, and `truncated` so nobody mistakes it for
        // a file that really was rewritten line for line.
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
/// with `d` edits. A copy is kept per `d` so the path can be walked back
/// afterwards — which is the memory the limit is really bounding.
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
            // Down (an insertion) when the diagonal below has got further, or
            // when there is no diagonal above to come across from.
            let mut x = if k == -d || (k != d && v[at - 1] < v[at + 1]) {
                v[at + 1]
            } else {
                v[at - 1] + 1
            };
            let mut y = x - k;
            // Then as far along the diagonal as the lines agree — the "greedy"
            // part, and where the cost of a small edit stays small.
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

/// Turns the recorded search into the script that produced it.
///
/// Walked from the end because that is where the answer was found; the result
/// is reversed at the last moment.
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

    /// The marks a gutter would draw, line by line, as a string — `.` for a
    /// line with nothing beside it. Reads as the picture it describes, which
    /// is what these tests are actually about.
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
        // The thing a reader means by "I changed this line". Reported as an
        // addition sitting on a deletion it would draw two marks for one edit.
        assert_eq!(gutter("one\ntwo\nthree\n", "one\nTWO\nthree\n"), ".~..");
    }

    #[test]
    fn a_removed_line_marks_where_it_was() {
        // Nothing is left to draw beside, so the mark belongs to the line that
        // took its place.
        assert_eq!(gutter("one\ntwo\nthree\n", "one\nthree\n"), ".-.");
    }

    #[test]
    fn a_removal_at_the_end_still_has_somewhere_to_go() {
        assert_eq!(gutter("one\ntwo\n", "one\n"), ".-");
    }

    #[test]
    fn a_lost_final_newline_is_a_change() {
        // Real, and the kind of thing a reviewer sees in a diff and the author
        // did not mean to do.
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
        // Myers has no notion of a moved line, and neither does `git diff`.
        // Pinned so that a future change to say otherwise is a decision rather
        // than a surprise.
        let diff = diff("a\nb\nc\n", "b\nc\na\n");
        assert!(!diff.is_empty());
        assert!(!diff.truncated);
    }

    #[test]
    fn the_common_ends_are_not_searched() {
        // A thousand identical lines around a one-line edit. If the prefix and
        // suffix were part of the search this would be a very different cost,
        // and past `MAX_EDITS` it would come back truncated.
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
        // Every line different, far past the limit. The answer is one block,
        // and `truncated` is how a caller can tell that from a file that
        // really was rewritten line for line.
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
