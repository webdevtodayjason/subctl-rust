//! Snapshot registry of every skill found under a root directory.
//!
//! At construction time we walk `<dir>/*/SKILL.md` — the Claude Code
//! convention also used by v3 (`components/skills/<name>/SKILL.md`).
//! Subdirectories beneath that level are ignored; a flat layout keeps
//! the operator's mental model simple and matches what the existing
//! skill catalog ships.
//!
//! Like [`evy_memory::PlaybookStore`], the registry is a snapshot — no
//! hot reload yet. Hot reload (via `notify`) is a Phase 5 concern when
//! the operator starts editing skills while the daemon is live.
//!
//! [`evy_memory::PlaybookStore`]: ../../evy_memory/struct.PlaybookStore.html

use std::fs;
use std::path::{Path, PathBuf};

use evy_core::Result;
use tracing::warn;

use crate::skill::Skill;

/// Read-only snapshot of every skill loaded under [`SkillRegistry::dir`].
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
    dir: PathBuf,
}

impl SkillRegistry {
    /// Walk `<dir>/*/SKILL.md` and parse each one.
    ///
    /// Non-directory entries are skipped silently. Sub-directories
    /// without a `SKILL.md` are skipped silently (they may be drafts or
    /// asset bundles). Files with malformed frontmatter return an error
    /// — operators want loud failures on broken YAML rather than a
    /// silently-missing skill.
    ///
    /// # Errors
    /// - [`evy_core::Error::Io`] when `dir` cannot be read.
    /// - [`evy_core::Error::InvalidMandate`] when any skill's
    ///   frontmatter fails to parse.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut skills = Vec::new();
        let entries = fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            // Each direct child must be a directory; skip files at the
            // top level (e.g. README.md, skills.sh).
            if !path.is_dir() {
                continue;
            }
            let skill_file = path.join("SKILL.md");
            if !skill_file.is_file() {
                // Some skill bundles ship without a SKILL.md (drafts,
                // doc-only directories). Warn but don't fail — the
                // operator's catalog has a couple of these and we want
                // load-on-startup to stay quiet on benign skips.
                warn!(
                    path = %skill_file.display(),
                    "skill directory has no SKILL.md, skipping",
                );
                continue;
            }
            let dir_name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_owned(),
                None => continue,
            };
            let raw = fs::read_to_string(&skill_file)?;
            let skill = Skill::from_raw(&dir_name, skill_file, &raw)?;
            skills.push(skill);
        }
        // Stable, alphabetised ordering — operators expect deterministic
        // listings on the dashboard and in CLI dumps.
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            skills,
            dir: dir.to_path_buf(),
        })
    }

    /// All skills, alphabetised by name.
    #[must_use]
    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    /// Exact-name lookup. Returns `None` when no skill matches.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Root directory this registry was loaded from. Surfaced for
    /// diagnostics ("Skills loaded from X") and tests.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Number of skills loaded.
    #[must_use]
    pub fn count(&self) -> usize {
        self.skills.len()
    }

    /// Render the skills catalog as a system-prompt index block.
    ///
    /// The format is the **Hermes-style** mandatory-skills block: a
    /// markdown header, load-bearing prose that instructs the model to
    /// emit a `skill_view(name)` tool call whenever a listed skill is
    /// even partially relevant, then a bullet list of `name: description`
    /// rows. See `crates/evy-thinking/src/anthropic.rs` for the
    /// consumer that injects this into the Anthropic `system` field and
    /// handles the `skill_view` tool round-trip.
    ///
    /// Returns the empty string when the registry is empty — a header
    /// with zero bullets would only confuse the model.
    ///
    /// ## Wording is load-bearing
    ///
    /// The instruction prose telling the LLM to call `skill_view` even
    /// when it *thinks* it can handle the task is what triggers reliable
    /// autoload (per the hermes-researcher findings under
    /// `.subctl/docs/hermes-compact-and-skills-findings.md` §2.5). Do
    /// not paraphrase this shorter without re-validating end-to-end —
    /// the model defaults to *not* loading skills when the instruction
    /// is soft.
    #[must_use]
    pub fn index_for_prompt(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(512 + self.skills.len() * 80);
        out.push_str(
            "## Skills (mandatory — load via skill_view when a skill applies)\n\
\n\
Before producing your response, scan the skills below. If any skill is \
even partially relevant to the current request, you MUST call the \
`skill_view` tool with the skill's name to load its full body — even \
if you believe you could handle the task without loading the skill. \
The skills below encode procedural knowledge you do not have by \
default. Load first, then respond.\n\
\n",
        );
        for skill in &self.skills {
            // Description is allowed to be empty by the loader; render a
            // bare bullet rather than `name: ` with a trailing colon, so
            // the format stays scannable for the model.
            if skill.description.is_empty() {
                out.push_str(&format!("- {}\n", skill.name));
            } else {
                out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_skill(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    const FOO: &str = r#"---
name: foo
description: foo skill
triggers: ["foo trigger"]
priority: 3
---

Foo body.
"#;

    const BAR: &str = r#"---
name: bar
description: bar skill
triggers: ["bar trigger"]
priority: 1
---

Bar body.
"#;

    #[test]
    fn loads_skills_alphabetised() {
        let dir = tempdir().unwrap();
        write_skill(dir.path(), "foo", FOO);
        write_skill(dir.path(), "bar", BAR);
        let reg = SkillRegistry::load(dir.path()).unwrap();
        assert_eq!(reg.count(), 2);
        assert_eq!(reg.list()[0].name, "bar");
        assert_eq!(reg.list()[1].name, "foo");
        assert_eq!(reg.dir(), dir.path());
    }

    #[test]
    fn find_returns_matching_skill() {
        let dir = tempdir().unwrap();
        write_skill(dir.path(), "foo", FOO);
        let reg = SkillRegistry::load(dir.path()).unwrap();
        assert!(reg.find("foo").is_some());
        assert!(reg.find("missing").is_none());
    }

    #[test]
    fn top_level_files_are_ignored() {
        let dir = tempdir().unwrap();
        write_skill(dir.path(), "foo", FOO);
        // A README at the catalog root — like components/skills/README.md.
        fs::write(dir.path().join("README.md"), "# catalog readme").unwrap();
        let reg = SkillRegistry::load(dir.path()).unwrap();
        assert_eq!(reg.count(), 1);
    }

    #[test]
    fn skill_dir_without_skill_md_is_skipped() {
        let dir = tempdir().unwrap();
        write_skill(dir.path(), "foo", FOO);
        // A subdirectory that has assets but no SKILL.md.
        fs::create_dir_all(dir.path().join("draft")).unwrap();
        fs::write(dir.path().join("draft").join("NOTES.md"), "wip").unwrap();
        let reg = SkillRegistry::load(dir.path()).unwrap();
        assert_eq!(reg.count(), 1);
        assert_eq!(reg.list()[0].name, "foo");
    }

    #[test]
    fn malformed_frontmatter_fails_load() {
        let dir = tempdir().unwrap();
        write_skill(dir.path(), "broken", "---\ntriggers: [\"x\n---\n\nbody\n");
        let err = SkillRegistry::load(dir.path()).unwrap_err();
        assert!(err.to_string().contains("yaml"), "got `{err}`");
    }

    #[test]
    fn index_for_prompt_empty_registry_returns_empty_string() {
        let dir = tempdir().unwrap();
        let reg = SkillRegistry::load(dir.path()).unwrap();
        assert_eq!(reg.count(), 0);
        assert_eq!(reg.index_for_prompt(), "");
    }

    #[test]
    fn index_for_prompt_contains_header_and_bullets() {
        let dir = tempdir().unwrap();
        write_skill(dir.path(), "foo", FOO);
        write_skill(dir.path(), "bar", BAR);
        let reg = SkillRegistry::load(dir.path()).unwrap();
        let idx = reg.index_for_prompt();
        assert!(idx.starts_with("## Skills (mandatory"));
        // Bullets sorted alphabetically because `load` sorts skills.
        assert!(idx.contains("- bar: bar skill"));
        assert!(idx.contains("- foo: foo skill"));
        // Load-bearing prose must be present.
        assert!(idx.contains("MUST call the `skill_view` tool"));
    }
}
