//! Integration test — load a small fixture catalog from a tempdir,
//! route a few realistic operator situations, and pin the ordering.
//!
//! Lives outside `src/` so it exercises only the crate's public surface
//! ([`SkillRegistry::load`], [`SkillRouter::new`], [`SkillRouter::route`]).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use evy_skills::{SkillRegistry, SkillRouter};
use tempfile::tempdir;

fn write_skill(root: &Path, name: &str, body: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), body).unwrap();
}

const RATE_LIMIT: &str = r#"---
name: rate-limit-recovery
description: How to drain and reroute a worker that hit a 429
triggers: ["rate limit", "429", "throttled"]
priority: 8
---

# Steps

1. Mark the provider degraded
2. Reroute pending workers
3. Backoff
"#;

const BROWNFIELD: &str = r#"---
name: brownfield-first-day
description: Checklist for the first day of any brownfield project
triggers: ["new brownfield project", "inheriting a codebase"]
priority: 5
---

# Checklist

1. Read the README
2. Run the tests
"#;

const TMUX: &str = r#"---
name: tmux-pane-debug
description: Diagnose missing or detached tmux panes
triggers: ["tmux pane missing", "detached pane"]
priority: 3
---

# Steps

1. tmux list-panes
2. tmux respawn-pane
"#;

const NO_FRONTMATTER: &str = r#"# Just a body

No structured metadata at all.
"#;

const NO_TRIGGERS: &str = r#"---
name: docs-only
description: docs-only catalog entry, has no triggers field
priority: 2
---

# Documentation

Pure reference, never routed by trigger.
"#;

fn build_router() -> SkillRouter {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write_skill(&root, "rate-limit-recovery", RATE_LIMIT);
    write_skill(&root, "brownfield-first-day", BROWNFIELD);
    write_skill(&root, "tmux-pane-debug", TMUX);
    write_skill(&root, "scratch", NO_FRONTMATTER);
    write_skill(&root, "docs-only", NO_TRIGGERS);

    let registry = Arc::new(SkillRegistry::load(&root).unwrap());
    // Keep the tempdir alive for the registry's lifetime by leaking it.
    // (Tests are short-lived; this is acceptable.)
    std::mem::forget(dir);
    SkillRouter::new(registry)
}

#[test]
fn loads_all_five_fixtures() {
    let router = build_router();
    assert_eq!(router.registry().count(), 5);
    // Alphabetical: brownfield, docs-only, rate-limit, scratch, tmux
    let names: Vec<_> = router
        .registry()
        .list()
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert_eq!(
        names,
        vec![
            "brownfield-first-day",
            "docs-only",
            "rate-limit-recovery",
            "scratch",
            "tmux-pane-debug",
        ]
    );
}

#[test]
fn routes_rate_limit_situation() {
    let router = build_router();
    let hits = router.route("worker hit a 429 on Claude provider, we're rate limited");
    // rate-limit-recovery: trigger match (1.0) + desc match (0.5) + 0.8 priority
    // = 2.3 — top hit by a wide margin.
    assert!(!hits.is_empty());
    assert_eq!(hits[0].skill.name, "rate-limit-recovery");
    assert!(hits[0].matched_trigger.is_some());
    assert!(hits[0].match_score > 2.0);
}

#[test]
fn routes_brownfield_situation() {
    let router = build_router();
    let hits = router.route("operator says: I'm inheriting a codebase from a contractor");
    let names: Vec<_> = hits.iter().map(|h| h.skill.name.clone()).collect();
    assert!(
        names.contains(&"brownfield-first-day".to_owned()),
        "got `{names:?}`"
    );
    assert_eq!(hits[0].skill.name, "brownfield-first-day");
}

#[test]
fn routes_tmux_situation() {
    let router = build_router();
    let hits = router.route("I see a detached pane that won't come back");
    assert_eq!(hits[0].skill.name, "tmux-pane-debug");
    assert_eq!(hits[0].matched_trigger.as_deref(), Some("detached pane"));
}

#[test]
fn unmatched_situation_returns_empty() {
    let router = build_router();
    let hits = router.route("operator wants to discuss avocado prices");
    assert!(hits.is_empty(), "got `{:?}`", hits);
}

#[test]
fn find_by_name_works_across_catalog() {
    let router = build_router();
    assert!(router.registry().find("rate-limit-recovery").is_some());
    assert!(router.registry().find("scratch").is_some());
    assert!(router.registry().find("does-not-exist").is_none());
}

#[test]
fn skills_without_triggers_or_descriptions_dont_pollute_results() {
    let router = build_router();
    let hits = router.route("rate limit");
    // `scratch` has no frontmatter, `docs-only` has no triggers but
    // does have a description — `description` doesn't mention "rate
    // limit", so neither should appear.
    for h in &hits {
        assert_ne!(h.skill.name, "scratch");
        assert_ne!(h.skill.name, "docs-only");
    }
}

#[test]
fn priority_and_description_combine_in_ranking() {
    let router = build_router();
    // Situation that triggers all three "real" skills. Description
    // overlap is intentional so the scoring breakdown stays observable:
    //
    //   rate-limit-recovery — trigger "rate limit" + desc word "hit"
    //                         + priority 8 → 1.0 + 0.5 + 0.8 = 2.3
    //   tmux-pane-debug     — trigger "detached pane" + desc word
    //                         "detached" + priority 3 → 1.0 + 0.5 + 0.3 = 1.8
    //   brownfield-first-day — trigger "inheriting a codebase" only,
    //                         no desc overlap + priority 5 → 1.5
    let hits =
        router.route("rate limit hit during inheriting a codebase from a detached pane scenario");
    let names: Vec<_> = hits.iter().map(|h| h.skill.name.clone()).collect();
    assert_eq!(
        names,
        vec![
            "rate-limit-recovery",
            "tmux-pane-debug",
            "brownfield-first-day"
        ],
        "actual scores: {:?}",
        hits.iter()
            .map(|h| (h.skill.name.clone(), h.match_score))
            .collect::<Vec<_>>()
    );
    assert!((hits[0].match_score - 2.3).abs() < 1e-5);
    assert!((hits[1].match_score - 1.8).abs() < 1e-5);
    assert!((hits[2].match_score - 1.5).abs() < 1e-5);
}
