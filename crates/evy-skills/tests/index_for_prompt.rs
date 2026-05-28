//! Integration tests for [`SkillRegistry::index_for_prompt`].
//!
//! The format of this block is part of the public contract — the
//! `evy-thinking::AnthropicBackend` injects the output verbatim into
//! the Anthropic `system` field. Any drift in the header / prose breaks
//! the LLM-driven skill autoload because the wording is load-bearing
//! (see registry docstring).

use std::fs;
use std::path::Path;

use evy_skills::SkillRegistry;
use tempfile::tempdir;

fn write_skill(root: &Path, name: &str, body: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), body).unwrap();
}

const ALPHA: &str = r#"---
name: alpha
description: Alpha description
priority: 1
---

# Alpha
body
"#;

const BETA: &str = r#"---
name: beta
description: Beta description
priority: 1
---

# Beta
body
"#;

const NO_DESC: &str = r#"---
name: nameless-desc
---

# Body only
nothing structured.
"#;

#[test]
fn empty_registry_yields_empty_string() {
    let dir = tempdir().unwrap();
    let reg = SkillRegistry::load(dir.path()).unwrap();
    assert!(reg.index_for_prompt().is_empty());
}

#[test]
fn populated_registry_renders_header_prose_and_bullets() {
    let dir = tempdir().unwrap();
    write_skill(dir.path(), "alpha", ALPHA);
    write_skill(dir.path(), "beta", BETA);
    let reg = SkillRegistry::load(dir.path()).unwrap();

    let idx = reg.index_for_prompt();

    // Header — exact wording, the operator pinned this in the spec and
    // it appears in the report-back to team-lead.
    assert!(
        idx.starts_with("## Skills (mandatory — load via skill_view when a skill applies)\n"),
        "header drift: `{}`",
        &idx[..idx.len().min(120)],
    );

    // Load-bearing prose — Hermes findings §2.5 say the model defaults
    // to NOT loading skills unless explicitly instructed. Pin the verbs.
    assert!(idx.contains("MUST call the `skill_view` tool"));
    assert!(idx.contains("Load first, then respond."));

    // Bullets — alphabetised by registry load order, "name: description".
    assert!(idx.contains("- alpha: Alpha description"));
    assert!(idx.contains("- beta: Beta description"));
    let alpha_pos = idx.find("- alpha:").unwrap();
    let beta_pos = idx.find("- beta:").unwrap();
    assert!(
        alpha_pos < beta_pos,
        "alpha must precede beta (alphabetical registry order)",
    );
}

#[test]
fn skill_with_empty_description_renders_bare_bullet() {
    let dir = tempdir().unwrap();
    write_skill(dir.path(), "nameless-desc", NO_DESC);
    let reg = SkillRegistry::load(dir.path()).unwrap();
    let idx = reg.index_for_prompt();
    // No trailing colon when there's nothing to describe.
    assert!(idx.contains("- nameless-desc\n"));
    assert!(!idx.contains("- nameless-desc:"));
}

#[test]
fn index_is_stable_across_calls() {
    // Pure function over a snapshot registry — calling it twice must
    // produce byte-identical output. Operators rely on this for prompt
    // caching in the AnthropicBackend.
    let dir = tempdir().unwrap();
    write_skill(dir.path(), "alpha", ALPHA);
    write_skill(dir.path(), "beta", BETA);
    let reg = SkillRegistry::load(dir.path()).unwrap();
    let a = reg.index_for_prompt();
    let b = reg.index_for_prompt();
    assert_eq!(a, b);
}
