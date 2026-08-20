//! Turning a tag into the body of its release.
//!
//! A release's notes are the section `CHANGELOG.md` already has for that
//! version, rather than a second description written in the release UI. Two
//! descriptions of one release drift, and the one in the repository is the one
//! a person reads at the commit they are standing on.
//!
//! The tag is the source of the version: `v0.1.0` finds the `## 0.1.0` heading.
//! A tag with no section is an error rather than an empty release — a release
//! that says nothing is worse than a failed workflow, because the workflow can
//! be run again and a published release cannot be unpublished.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// The version a tag names: `v0.1.0` and `0.1.0` both mean `0.1.0`.
pub fn version_of(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// The changelog section for `version`, without its heading.
///
/// Everything from that version's heading down to the next `##` of the same
/// level, trimmed. A nested `###` belongs to the section and is kept.
pub fn section_for(changelog: &str, version: &str) -> Option<String> {
    let heading = format!("## {version}");
    let start = changelog.lines().position(|line| line.trim() == heading)?;
    let body: Vec<&str> = changelog
        .lines()
        .skip(start + 1)
        .take_while(|line| !line.trim_start().starts_with("## "))
        .collect();
    let body = body.join("\n").trim().to_owned();
    (!body.is_empty()).then_some(body)
}

/// Writes the notes for `tag` to `out`.
pub fn run(root: &Path, tag: &str, out: &Path) -> Result<()> {
    let path = root.join("CHANGELOG.md");
    let changelog =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let version = version_of(tag);
    let Some(section) = section_for(&changelog, version) else {
        bail!(
            "CHANGELOG.md has no `## {version}` section, so {tag} would be released with no \
             notes. Add one, or tag a version that has one."
        );
    };

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(out, format!("{section}\n"))
        .with_context(|| format!("writing {}", out.display()))?;
    println!("{}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGELOG: &str = "\
# Changelog

## 0.2.0

Newer things.

### A subsection

Which belongs to 0.2.0.

## 0.1.0

The first release.

## 0.0.1

Older.
";

    #[test]
    fn a_tag_names_its_version_with_or_without_the_v() {
        assert_eq!(version_of("v0.1.0"), "0.1.0");
        assert_eq!(version_of("0.1.0"), "0.1.0");
        // A `v` inside the version is not a prefix to strip.
        assert_eq!(version_of("v1.0.0-rc.1"), "1.0.0-rc.1");
    }

    #[test]
    fn a_section_stops_at_the_next_release_and_keeps_its_own_subsections() {
        let section = section_for(CHANGELOG, "0.2.0").expect("a section");
        assert!(section.starts_with("Newer things."), "{section}");
        assert!(section.contains("### A subsection"), "{section}");
        // The next release's heading is where it ends, and its body is not
        // dragged in: notes that quietly include the previous release describe
        // work that was already announced.
        assert!(!section.contains("The first release"), "{section}");

        let section = section_for(CHANGELOG, "0.1.0").expect("a section");
        assert_eq!(section, "The first release.");
    }

    #[test]
    fn a_version_with_no_section_is_nothing_rather_than_something_empty() {
        assert!(section_for(CHANGELOG, "9.9.9").is_none());
        // Present but empty is also nothing: a heading with no text under it
        // would produce a release that says nothing.
        assert!(section_for("## 0.3.0\n\n## 0.2.0\nx\n", "0.3.0").is_none());
    }

    #[test]
    fn the_repositorys_own_changelog_has_a_section_for_its_own_version() {
        // The check that matters at release time, run on every build rather than
        // discovered by a workflow that has already started.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the repository root");
        let changelog = std::fs::read_to_string(root.join("CHANGELOG.md")).expect("a changelog");
        let version = env!("CARGO_PKG_VERSION");
        assert!(
            section_for(&changelog, version).is_some(),
            "CHANGELOG.md has no `## {version}` section, so tagging v{version} would fail"
        );
    }
}
