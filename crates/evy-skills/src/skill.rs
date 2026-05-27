//! A single skill loaded from disk.
//!
//! Parsing is done up-front at registry-load time; the [`Skill`] returned
//! to callers is a fully-typed snapshot, not a lazy reader. Frontmatter
//! is parsed via `gray_matter` (YAML engine) into a typed
//! [`FrontmatterShape`] — unlike `evy-memory::Playbook` we do not flatten
//! arbitrary keys into a string map. Skills have a fixed, well-known
//! shape (name / description / triggers / priority); anything else is
//! ignored.

use std::path::PathBuf;

use evy_core::Result;
use gray_matter::engine::YAML;
use gray_matter::{Matter, ParsedEntityStruct};
use serde::{Deserialize, Serialize};

use crate::error;

/// Typed frontmatter shape.
///
/// Every field is `default`able — missing keys parse cleanly. The
/// directory name supplies the `name` fallback at the call site
/// ([`Skill::from_raw`]), mirroring the v3 `fm.name || d.name` rule.
#[derive(Debug, Default, Deserialize)]
struct FrontmatterShape {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default)]
    priority: Option<i32>,
}

/// One skill, fully parsed and ready to route against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Canonical skill identifier. Comes from the frontmatter `name`
    /// field when present; falls back to the directory name otherwise.
    pub name: String,
    /// Absolute path of the `SKILL.md` file on disk.
    pub path: PathBuf,
    /// One-line description; empty when no frontmatter `description` is
    /// supplied.
    pub description: String,
    /// Substrings the router scans the situation string for. Empty when
    /// no `triggers` field is supplied.
    pub triggers: Vec<String>,
    /// Operator-supplied ranking nudge. Defaults to `0` when omitted.
    /// Higher = preferred.
    pub priority: i32,
    /// Markdown body of the skill, with the frontmatter block stripped.
    pub body: String,
}

impl Skill {
    /// Parse a skill from the raw file contents.
    ///
    /// `dir_name` is the parent directory name; it becomes the skill's
    /// `name` when frontmatter omits one. `path` is the absolute path on
    /// disk, surfaced via [`Skill::path`].
    ///
    /// # Errors
    /// Returns [`evy_core::Error::InvalidMandate`] when frontmatter is
    /// present but fails to parse as YAML in the expected shape.
    pub(crate) fn from_raw(dir_name: &str, path: PathBuf, raw: &str) -> Result<Self> {
        let matter = Matter::<YAML>::new();
        // Heuristic: a file that opens with `---\n` is asking to be
        // read as frontmatter. If `gray_matter` then refuses to parse
        // it, we treat that as malformed YAML rather than silently
        // pretending the whole file is body — operators want loud
        // failures on broken YAML rather than a quietly-defaulted
        // skill with an unexpected name.
        let looks_like_frontmatter = raw.starts_with("---\n") || raw.starts_with("---\r\n");
        let (fm, body) = match matter.parse_with_struct::<FrontmatterShape>(raw) {
            Some(ParsedEntityStruct {
                data,
                content,
                orig: _,
                matter: _,
                excerpt: _,
            }) => (data, content),
            None if looks_like_frontmatter => {
                return Err(error::bad_skill(
                    &path,
                    "malformed yaml frontmatter (unterminated block or bad syntax)",
                ));
            }
            None => (FrontmatterShape::default(), raw.to_owned()),
        };

        let name = fm
            .name
            .and_then(|s| {
                let t = s.trim().to_owned();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            })
            .unwrap_or_else(|| dir_name.to_owned());

        Ok(Self {
            name,
            path,
            description: fm.description.unwrap_or_default(),
            triggers: fm.triggers,
            priority: fm.priority.unwrap_or(0),
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path() -> PathBuf {
        PathBuf::from("/tmp/skills/example/SKILL.md")
    }

    #[test]
    fn parses_full_frontmatter() {
        let raw = r#"---
name: example
description: An example skill
triggers: ["foo", "bar"]
priority: 7
---

Body goes here.
"#;
        let s = Skill::from_raw("example", path(), raw).unwrap();
        assert_eq!(s.name, "example");
        assert_eq!(s.description, "An example skill");
        assert_eq!(s.triggers, vec!["foo", "bar"]);
        assert_eq!(s.priority, 7);
        assert!(s.body.contains("Body goes here."));
        assert!(!s.body.contains("---"), "frontmatter must be stripped");
    }

    #[test]
    fn missing_frontmatter_uses_dir_name_and_defaults() {
        let raw = "# Just a body\n\nNothing structured.\n";
        let s = Skill::from_raw("scratch", path(), raw).unwrap();
        assert_eq!(s.name, "scratch");
        assert_eq!(s.description, "");
        assert!(s.triggers.is_empty());
        assert_eq!(s.priority, 0);
        assert!(s.body.contains("Just a body"));
    }

    #[test]
    fn name_falls_back_to_dir_when_frontmatter_omits() {
        let raw = r#"---
description: No name field here
triggers: ["x"]
---

body
"#;
        let s = Skill::from_raw("from-dir", path(), raw).unwrap();
        assert_eq!(s.name, "from-dir");
        assert_eq!(s.description, "No name field here");
    }

    #[test]
    fn missing_triggers_field_yields_empty_vec() {
        let raw = r#"---
name: t
description: no triggers field
priority: 3
---

body
"#;
        let s = Skill::from_raw("t-dir", path(), raw).unwrap();
        assert!(s.triggers.is_empty());
        assert_eq!(s.priority, 3);
    }

    #[test]
    fn malformed_yaml_errors() {
        // Unbalanced bracket inside the list — serde_yaml refuses.
        let raw = "---\ntriggers: [\"unterminated\n---\n\nbody\n";
        let err = Skill::from_raw("bad", path(), raw).unwrap_err();
        assert!(
            err.to_string().contains("skill") && err.to_string().contains("yaml"),
            "got `{err}`"
        );
    }

    #[test]
    fn empty_name_string_falls_back_to_dir() {
        let raw = r#"---
name: ""
description: empty name
---

body
"#;
        let s = Skill::from_raw("fallback-dir", path(), raw).unwrap();
        assert_eq!(s.name, "fallback-dir");
    }
}
