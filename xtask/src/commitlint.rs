//! Checking commit messages against Conventional Commits.
//!
//! Implemented here rather than using an external tool. The specification is
//! short, only a few rules are needed, and a Node package in a Rust
//! repository's CI would be a dependency used only to check a string. See the
//! repository's Dependencies section for the reasoning.
//!
//! The rules, and why each one fails the build:
//!
//! - **The header parses.** A header that does not match
//!   `<type>(<scope>)!: <description>` is not a conventional commit.
//! - **The type is in the known set.** A typo like `feats:` parses but is
//!   useless to tools that read the history.
//! - **There is a description**, it does not end in a full stop, and it is not
//!   capitalised. These conventions keep a list of subjects readable.
//! - **The header fits in 72 columns.** `git log --oneline` and GitHub views
//!   truncate longer headers.
//! - **A body is separated by a blank line.** Without it, `git log --format=%s`
//!   returns the first paragraph rather than the subject.
//!
//! Intentionally not checked: whether the scope is from a fixed list (the crate
//! names change), and the body's line length (a pasted error message or URL is
//! more useful unwrapped).

use std::fmt::Write as _;

/// The types this repository accepts, with what each is for.
///
/// The Conventional Commits types plus two widely used Angular additions. A table
/// rather than a list, so the error message can describe each type.
const TYPES: &[(&str, &str)] = &[
    ("feat", "a new capability for the user"),
    ("fix", "a bug fix"),
    ("docs", "documentation only"),
    ("style", "formatting, no behaviour change"),
    ("refactor", "neither fixes a bug nor adds a feature"),
    ("perf", "makes something faster"),
    ("test", "adds or corrects tests"),
    ("build", "the build system or dependencies"),
    ("ci", "CI configuration and scripts"),
    ("chore", "anything else with no production code change"),
    ("revert", "undoes an earlier commit"),
];

/// The longest header that `git log --oneline` and GitHub's views show without
/// truncation.
const MAX_HEADER: usize = 72;

/// What is wrong with one commit message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Which rule was broken.
    pub rule: &'static str,
    /// What to do about it.
    pub detail: String,
}

impl Problem {
    fn new(rule: &'static str, detail: impl Into<String>) -> Self {
        Self {
            rule,
            detail: detail.into(),
        }
    }
}

/// Checks one commit message, returning every problem with it.
///
/// Reports every problem rather than the first, so the message can be fixed in
/// one pass.
pub fn check(message: &str) -> Vec<Problem> {
    let mut problems = Vec::new();

    let mut lines = message.lines();
    let header = lines.next().unwrap_or("").trim_end();

    if header.is_empty() {
        problems.push(Problem::new("header", "the commit message is empty"));
        return problems;
    }

    // Git generates merge commit messages, and rewriting one would change
    // history that is already shared.
    if header.starts_with("Merge ") || header.starts_with("Revert \"") {
        return problems;
    }

    if header.chars().count() > MAX_HEADER {
        problems.push(Problem::new(
            "header-length",
            format!(
                "the header is {} characters; keep it within {MAX_HEADER} so it \
                 survives `git log --oneline`",
                header.chars().count()
            ),
        ));
    }

    match parse_header(header) {
        Ok(parsed) => problems.extend(check_parsed(&parsed)),
        Err(problem) => problems.push(problem),
    }

    // Without the blank line, `git log --format=%s` returns the first paragraph
    // rather than the subject.
    if let Some(second) = message.lines().nth(1) {
        if !second.trim().is_empty() {
            problems.push(Problem::new(
                "blank-line",
                "put a blank line between the header and the body",
            ));
        }
    }

    problems
}

/// The pieces of a conventional header.
#[derive(Debug, PartialEq, Eq)]
struct Header<'a> {
    kind: &'a str,
    scope: Option<&'a str>,
    breaking: bool,
    description: &'a str,
}

fn parse_header(header: &str) -> Result<Header<'_>, Problem> {
    // Split on the colon alone rather than on `": "`, so a header ending in a
    // bare colon is reported as an empty description. Reporting `feat(lsp):` as
    // an unparseable header would be correct but unhelpful, because the form is
    // right.
    let Some((prefix, rest)) = header.split_once(':') else {
        return Err(Problem::new(
            "header",
            format!(
                "expected `type(scope): description`, got {header:?}. Types: {}",
                TYPES
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    };

    // The specification requires one space after the colon. This is checked
    // separately from the split, so the message identifies which mistake was
    // made.
    let description = match rest.strip_prefix(' ') {
        Some(description) => description,
        None if rest.is_empty() => rest,
        None => {
            return Err(Problem::new(
                "header",
                format!("put a space after the colon: {header:?}"),
            ))
        }
    };

    let breaking = prefix.ends_with('!');
    let prefix = prefix.strip_suffix('!').unwrap_or(prefix);

    let (kind, scope) = match prefix.split_once('(') {
        Some((kind, rest)) => {
            let Some(scope) = rest.strip_suffix(')') else {
                return Err(Problem::new(
                    "scope",
                    format!("the scope in {header:?} is missing its closing bracket"),
                ));
            };
            (kind, Some(scope))
        }
        None => (prefix, None),
    };

    Ok(Header {
        kind,
        scope,
        breaking,
        description,
    })
}

fn check_parsed(header: &Header<'_>) -> Vec<Problem> {
    let mut problems = Vec::new();

    if !TYPES.iter().any(|(name, _)| *name == header.kind) {
        let mut detail = format!("`{}` is not a known type. Use one of:\n", header.kind);
        for (name, purpose) in TYPES {
            // `write!` to a String cannot fail, so the result is ignored.
            let _ = writeln!(detail, "    {name:<9} {purpose}");
        }
        problems.push(Problem::new("type", detail.trim_end()));
    }

    if let Some(scope) = header.scope {
        if scope.trim().is_empty() {
            problems.push(Problem::new(
                "scope",
                "an empty scope is worse than none — write `feat:` rather than `feat():`",
            ));
        }
    }

    let description = header.description.trim();
    if description.is_empty() {
        problems.push(Problem::new("description", "the description is empty"));
        return problems;
    }
    if description.ends_with('.') {
        problems.push(Problem::new(
            "description",
            "drop the trailing full stop: the subject is a title, not a sentence",
        ));
    }
    if description
        .chars()
        .next()
        .is_some_and(|c| c.is_uppercase() && c.is_alphabetic())
    {
        // Lowercase unless it is a proper noun, which this check cannot detect.
        // An acronym like `LSP` passes because its second character is also
        // uppercase.
        let second_is_upper = description
            .chars()
            .nth(1)
            .is_some_and(|c| c.is_uppercase() || !c.is_alphabetic());
        if !second_is_upper {
            problems.push(Problem::new(
                "description",
                format!("start the description lowercase: {description:?}"),
            ));
        }
    }

    // Not a failure. The flag is read here to show that it is parsed and
    // intentionally accepted.
    let _ = header.breaking;

    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(message: &str) -> Vec<&'static str> {
        check(message).into_iter().map(|p| p.rule).collect()
    }

    #[test]
    fn a_well_formed_message_passes() {
        assert!(check("feat(lsp): complete identifiers from the server").is_empty());
        assert!(check("fix: stop the hover box overflowing").is_empty());
    }

    #[test]
    fn a_body_after_a_blank_line_passes() {
        let message = "feat(lsp): add completion\n\nThe body explains why.\n";
        assert!(check(message).is_empty(), "{:?}", check(message));
    }

    #[test]
    fn a_bare_subject_with_no_type_is_refused() {
        // The message this check was added for.
        assert_eq!(rules("Completion"), vec!["header"]);
    }

    #[test]
    fn an_unknown_type_is_refused_with_the_list() {
        let problems = check("feats: add completion");
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].rule, "type");
        assert!(
            problems[0].detail.contains("feat"),
            "the advice lists types"
        );
        assert!(problems[0].detail.contains("refactor"));
    }

    #[test]
    fn a_breaking_change_marker_is_accepted() {
        assert!(check("feat(lsp)!: change the supervisor's signature").is_empty());
        assert!(check("feat!: change the supervisor's signature").is_empty());
    }

    #[test]
    fn an_unclosed_scope_is_refused() {
        assert_eq!(rules("feat(lsp: add completion"), vec!["scope"]);
    }

    #[test]
    fn an_empty_scope_is_refused() {
        // Omit the parentheses when no scope is specified: `feat:`.
        assert_eq!(rules("feat(): add completion"), vec!["scope"]);
    }

    #[test]
    fn a_missing_description_is_refused() {
        // Reported as an empty description rather than an unparseable header,
        // because the form is right and the other message would mislead the
        // author.
        assert_eq!(rules("feat(lsp): "), vec!["description"]);
        assert_eq!(rules("feat(lsp):"), vec!["description"]);
        assert_eq!(rules("fix:"), vec!["description"]);
    }

    #[test]
    fn a_missing_space_after_the_colon_is_refused() {
        // The specification requires exactly one.
        assert_eq!(rules("feat(lsp):add completion"), vec!["header"]);
    }

    #[test]
    fn a_trailing_full_stop_is_refused() {
        assert_eq!(
            rules("feat(lsp): add completion."),
            vec!["description"],
            "the subject is a title, not a sentence"
        );
    }

    #[test]
    fn a_capitalised_description_is_refused() {
        assert_eq!(rules("feat(lsp): Add completion"), vec!["description"]);
    }

    #[test]
    fn an_acronym_is_not_mistaken_for_a_capitalised_sentence() {
        // `LSP` and `URI` are correctly written in uppercase.
        assert!(check("fix(lsp): URI escaping for drive letters").is_empty());
        assert!(check("feat: LSP completion").is_empty());
    }

    #[test]
    fn a_long_header_is_refused() {
        let long = format!("feat(lsp): {}", "x".repeat(MAX_HEADER));
        assert!(rules(&long).contains(&"header-length"));
    }

    #[test]
    fn a_header_of_exactly_the_limit_passes() {
        let description = "x".repeat(MAX_HEADER - "feat(lsp): ".len());
        let message = format!("feat(lsp): {description}");
        assert_eq!(message.chars().count(), MAX_HEADER);
        assert!(check(&message).is_empty(), "{:?}", check(&message));
    }

    #[test]
    fn a_body_without_a_blank_line_is_refused() {
        // Without it, `git log --format=%s` returns the first paragraph.
        assert_eq!(
            rules("feat(lsp): add completion\nstraight into the body"),
            vec!["blank-line"]
        );
    }

    #[test]
    fn an_empty_message_is_refused_once() {
        assert_eq!(rules(""), vec!["header"]);
        assert_eq!(rules("\n\n"), vec!["header"]);
    }

    #[test]
    fn a_merge_commit_is_left_alone() {
        // Generated by git, and rewriting it would change shared history.
        assert!(check("Merge pull request #10 from sabas0ba/branch").is_empty());
        assert!(check("Revert \"feat(lsp): add completion\"").is_empty());
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        // All problems are reported so one rewrite can fix the message.
        let problems = check("Feats(): Add completion.\nno blank line");
        let reported: Vec<&str> = problems.iter().map(|p| p.rule).collect();
        assert!(reported.contains(&"type"), "{reported:?}");
        assert!(reported.contains(&"scope"), "{reported:?}");
        assert!(reported.contains(&"description"), "{reported:?}");
        assert!(reported.contains(&"blank-line"), "{reported:?}");
    }

    #[test]
    fn the_footers_this_repository_uses_do_not_trip_anything() {
        let message = "feat(lsp): add completion\n\nBody.\n\n\
                       Co-Authored-By: Someone <nobody@example.com>\n\
                       Claude-Session: https://example.com/session\n";
        assert!(check(message).is_empty(), "{:?}", check(message));
    }
}
