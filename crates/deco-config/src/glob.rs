//! The small glob dialect VS Code uses in `files.exclude`, `files.watcherExclude`
//! and an extension's `workspaceContains` activation event.
//!
//! Here rather than in `deco-ext` because `files.exclude` is a setting and this
//! crate owns settings — and because the quick-open file walk needs the same
//! dialect. One implementation, two callers.
//!
//! Supports `?`, `*` (within one path segment) and `**` (across segments).
//! Brace alternation and character classes are not implemented.

/// Whether `path` matches `pattern`.
///
/// Both are treated as `/`-separated, so callers on Windows should convert
/// backslashes first.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    match_segments(&pattern, &path)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            // `**` matches zero or more segments, so try every split point.
            if rest.is_empty() {
                return true;
            }
            (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]))
        }
        Some((&head, rest)) => match path.split_first() {
            Some((&first, tail)) if match_segment(head, first) => match_segments(rest, tail),
            _ => false,
        },
    }
}

/// Matches one path segment against one pattern segment.
fn match_segment(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();

    // Iterative backtracking: `star` remembers where to resume if the rest of
    // the pattern fails, which keeps this linear for realistic patterns rather
    // than exponential.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if let Some(star_pos) = star {
            pi = star_pos + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_paths_match_exactly() {
        assert!(matches("src/main.rs", "src/main.rs"));
        assert!(!matches("src/main.rs", "src/lib.rs"));
        assert!(!matches("src/main.rs", "src/main.rs.bak"));
    }

    #[test]
    fn star_matches_within_one_segment() {
        assert!(matches("src/*.rs", "src/main.rs"));
        assert!(matches("*.rs", "main.rs"));
        assert!(!matches("src/*.rs", "src/sub/main.rs"));
    }

    #[test]
    fn question_mark_matches_one_character() {
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("a?c", "abbc"));
    }

    #[test]
    fn double_star_crosses_segments() {
        assert!(matches("**/Cargo.toml", "Cargo.toml"));
        assert!(matches("**/Cargo.toml", "crates/deco/Cargo.toml"));
        assert!(matches("src/**/mod.rs", "src/a/b/mod.rs"));
        assert!(matches("src/**/mod.rs", "src/mod.rs"));
        assert!(!matches("src/**/mod.rs", "other/a/mod.rs"));
    }

    #[test]
    fn a_trailing_double_star_matches_everything_below() {
        assert!(matches("node_modules/**", "node_modules/a/b/c.js"));
        assert!(matches("node_modules/**", "node_modules"));
    }

    #[test]
    fn stars_combine_with_literals() {
        assert!(matches("**/*.test.ts", "src/a/b.test.ts"));
        assert!(!matches("**/*.test.ts", "src/a/b.ts"));
    }

    #[test]
    fn multiple_stars_in_one_segment_backtrack_correctly() {
        assert!(matches("*a*b*", "xxayybzz"));
        assert!(!matches("*a*b*c", "xxayybzz"));
        assert!(matches("*.*.*", "a.b.c"));
    }

    #[test]
    fn a_bare_star_does_not_match_across_segments() {
        assert!(matches("*", "file.txt"));
        assert!(!matches("*", "dir/file.txt"));
    }

    #[test]
    fn patterns_are_anchored_at_both_ends() {
        assert!(!matches("src", "src/main.rs"));
        assert!(!matches("main.rs", "src/main.rs"));
    }
}
