//! Integration test for the playbook store.
//!
//! Creates a temporary directory with three markdown playbooks and
//! asserts that `PlaybookStore::load` parses their frontmatter, isolates
//! their bodies, and serves `find` / `matching_trigger` lookups.

use std::fs;

use evy_memory::PlaybookStore;
use tempfile::tempdir;

const BROWNFIELD: &str = r#"---
name: brownfield-first-day
description: Checklist for the first day of any brownfield project
triggers: ["new brownfield project", "inheriting a codebase"]
last_reviewed: 2026-05-26
---

# Brownfield First Day Checklist

1. Read the README.
2. Run the tests.
3. Identify the top three risks.
"#;

const RATE_LIMIT: &str = r#"---
name: drain-rate-limited-worker
description: Pause traffic, surface the rate-limit window, re-route or wait.
triggers: ["rate limit hit", "429 from provider"]
---

# Drain rate-limited worker

When a provider returns 429:

1. Pause new dispatches against that provider.
2. Surface the retry window to the operator.
3. Re-route eligible mandates to a healthy provider.
"#;

const VERSION_BUMP: &str = r#"---
name: cutover-friendly-version-bump
triggers: ["release", "version bump", "tag"]
---

# Cutover-friendly version bump

Smaller releases over batched ones — see operator preference.
"#;

#[test]
fn loads_three_playbooks_with_frontmatter() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("brownfield-first-day.md"), BROWNFIELD).unwrap();
    fs::write(dir.path().join("drain-rate-limited-worker.md"), RATE_LIMIT).unwrap();
    fs::write(
        dir.path().join("cutover-friendly-version-bump.md"),
        VERSION_BUMP,
    )
    .unwrap();

    let store = PlaybookStore::load(dir.path()).expect("load");
    assert_eq!(store.count(), 3, "exactly three .md files loaded");

    let bf = store
        .find("brownfield-first-day")
        .expect("brownfield found");
    assert_eq!(bf.triggers.len(), 2);
    assert!(bf.body.contains("Brownfield First Day Checklist"));
    assert!(!bf.body.starts_with("---"), "frontmatter must be stripped");
    assert_eq!(
        bf.frontmatter.get("description").map(String::as_str),
        Some("Checklist for the first day of any brownfield project")
    );
}

#[test]
fn matching_trigger_surfaces_relevant_playbook() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("drain-rate-limited-worker.md"), RATE_LIMIT).unwrap();
    fs::write(
        dir.path().join("cutover-friendly-version-bump.md"),
        VERSION_BUMP,
    )
    .unwrap();

    let store = PlaybookStore::load(dir.path()).unwrap();

    let rate = store.matching_trigger("worker just hit a 429 from provider claude");
    assert_eq!(rate.len(), 1);
    assert_eq!(rate[0].name, "drain-rate-limited-worker");

    let release = store.matching_trigger("operator says: shall we tag v3.3.5?");
    assert_eq!(release.len(), 1);
    assert_eq!(release[0].name, "cutover-friendly-version-bump");

    let none = store.matching_trigger("nothing matches this query at all");
    assert!(none.is_empty());
}

#[test]
fn empty_directory_loads_without_error() {
    let dir = tempdir().unwrap();
    let store = PlaybookStore::load(dir.path()).unwrap();
    assert_eq!(store.count(), 0);
    assert!(store.list().is_empty());
    assert!(store.find("anything").is_none());
}
