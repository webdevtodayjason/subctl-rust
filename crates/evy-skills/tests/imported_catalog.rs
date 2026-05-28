//! Integration test — load the in-repo imported Hermes skill catalog
//! and pin a handful of operator-critical skills by name.
//!
//! The catalog lives at `<workspace_root>/skills/` and ships in the
//! repo (see `crates/evy-skills/IMPORTED_SKILLS.md`). This test
//! protects against silent regressions when the catalog is touched
//! (skills removed, frontmatter rewritten so `name` no longer matches
//! the directory, malformed YAML accidentally introduced).
//!
//! `CARGO_MANIFEST_DIR` resolves to `crates/evy-skills/`, so the
//! catalog sits two levels up.

use std::path::PathBuf;

use evy_skills::SkillRegistry;

/// Resolve the workspace-root `skills/` directory from the crate manifest dir.
fn skills_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("skills")
}

/// The catalog must contain *at least* this many skills. Pinned to the
/// imported floor (94 at landing); see `IMPORTED_SKILLS.md`. Lower bound
/// rather than equality so future additions don't break the test.
const MIN_SKILLS: usize = 90;

/// Operator-critical skills the daemon should always be able to find.
/// Each entry must match a frontmatter `name:` field in the catalog.
const REQUIRED_SKILLS: &[&str] = &[
    "test-driven-development",
    "systematic-debugging",
    "plan",
    "writing-plans",
    "subagent-driven-development",
    "hermes-agent",
];

#[test]
fn imported_catalog_loads_and_contains_required_skills() {
    let dir = skills_dir();
    assert!(
        dir.is_dir(),
        "imported skills/ catalog missing at {}",
        dir.display(),
    );

    let registry = SkillRegistry::load(&dir).expect("registry loads imported catalog");

    assert!(
        registry.count() >= MIN_SKILLS,
        "expected at least {} skills, loaded {}",
        MIN_SKILLS,
        registry.count(),
    );

    for name in REQUIRED_SKILLS {
        assert!(
            registry.find(name).is_some(),
            "required skill `{name}` missing from imported catalog",
        );
    }
}

#[test]
fn imported_catalog_skills_are_alphabetised() {
    let registry = SkillRegistry::load(&skills_dir()).expect("registry loads");
    let names: Vec<&str> = registry.list().iter().map(|s| s.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "registry must return skills alphabetised");
}
