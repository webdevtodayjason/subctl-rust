//! Skill router — match a worker situation against the registry.
//!
//! Phase 4 keeps the matcher dumb-simple on purpose: case-insensitive
//! substring scan against each skill's `triggers` list, plus a fallback
//! sweep over the `description`. The scoring formula is:
//!
//! ```text
//! score = (1.0 if any trigger matches) + (0.5 if description matches) + priority / 10
//! ```
//!
//! Skills with no match (no trigger AND no description hit) are
//! excluded from the result, even when their `priority > 0` — the
//! "relevance" reading of `route()` means "at least one explicit
//! signal", not "everything we have, sorted by priority".
//!
//! Results are sorted descending by score with the skill name as a
//! deterministic tie-breaker (alphabetical).
//!
//! // TODO: Phase 5 — swap the substring matcher for a semantic
//! // retriever. The intended path is to keep this trait-flat (`route`
//! // stays sync, takes `&str`, returns `Vec<RoutedSkill>`) but to gain
//! // a `route_async` companion that hits an embeddings service. The
//! // `tokio` and `async-trait` dependencies are already declared in
//! // `Cargo.toml` for that work.

use std::sync::Arc;

use crate::registry::SkillRegistry;
use crate::skill::Skill;

/// A skill returned by [`SkillRouter::route`], together with diagnostic
/// metadata about *why* it matched.
#[derive(Debug, Clone)]
pub struct RoutedSkill {
    /// The matched skill (cloned out of the registry — callers usually
    /// hand the body to a worker prompt and don't want to wrestle with
    /// a borrow lifetime).
    pub skill: Skill,
    /// Composite score per the formula in the module docs.
    pub match_score: f32,
    /// The first trigger string that matched, if any. `None` when the
    /// match came purely from the description.
    pub matched_trigger: Option<String>,
}

/// Router over a [`SkillRegistry`].
///
/// The registry is held behind an `Arc` so the router can be cloned
/// cheaply and stashed in multiple workers / spawn pipelines without
/// re-reading from disk.
#[derive(Debug, Clone)]
pub struct SkillRouter {
    registry: Arc<SkillRegistry>,
}

impl SkillRouter {
    /// Construct a router over an already-loaded registry.
    #[must_use]
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }

    /// Return every skill that matched `situation`, sorted by relevance
    /// (descending score, then ascending name for determinism).
    ///
    /// Matching is case-insensitive substring containment. Skills with
    /// no trigger hit AND no description hit are excluded; pure-priority
    /// listings should use [`SkillRegistry::list`] directly.
    #[must_use]
    pub fn route(&self, situation: &str) -> Vec<RoutedSkill> {
        let haystack = situation.to_lowercase();
        let mut hits: Vec<RoutedSkill> = Vec::new();

        for skill in self.registry.list() {
            let matched_trigger = skill
                .triggers
                .iter()
                .find(|t| {
                    let needle = t.to_lowercase();
                    !needle.is_empty() && haystack.contains(&needle)
                })
                .cloned();
            let desc_lower = skill.description.to_lowercase();
            let description_matches = !desc_lower.is_empty()
                && desc_lower.split_whitespace().any(|tok| {
                    // 3-char floor on description tokens keeps stopword
                    // noise out ("a"/"an"/"to" matching everything).
                    tok.len() >= 3 && haystack.contains(tok)
                });

            if matched_trigger.is_none() && !description_matches {
                continue;
            }

            let trigger_component = if matched_trigger.is_some() { 1.0 } else { 0.0 };
            let description_component = if description_matches { 0.5 } else { 0.0 };
            let priority_component = f32::from(i16::try_from(skill.priority).unwrap_or(0)) / 10.0;
            let score = trigger_component + description_component + priority_component;

            hits.push(RoutedSkill {
                skill: skill.clone(),
                match_score: score,
                matched_trigger,
            });
        }

        // Sort by score DESC, then name ASC for a stable tie-break.
        // f32 has no total order; `total_cmp` is the right choice
        // because all our scores are finite non-NaN reals.
        hits.sort_by(|a, b| {
            b.match_score
                .total_cmp(&a.match_score)
                .then_with(|| a.skill.name.cmp(&b.skill.name))
        });
        hits
    }

    /// Borrow the underlying registry — useful when a caller wants to
    /// iterate the full catalog without re-creating it.
    #[must_use]
    pub fn registry(&self) -> &SkillRegistry {
        &self.registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn write_skill(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    fn registry_with(skills: &[(&str, &str)]) -> Arc<SkillRegistry> {
        let dir = tempdir().unwrap();
        for (name, body) in skills {
            write_skill(dir.path(), name, body);
        }
        Arc::new(SkillRegistry::load(dir.path()).unwrap())
    }

    #[test]
    fn returns_empty_for_no_matches() {
        let reg = registry_with(&[(
            "foo",
            "---\nname: foo\ndescription: about widgets\ntriggers: [\"widget\"]\npriority: 5\n---\n\nbody\n",
        )]);
        let router = SkillRouter::new(reg);
        let hits = router.route("nothing related at all");
        assert!(hits.is_empty());
    }

    #[test]
    fn trigger_match_scores_above_description_match() {
        let trigger_skill = "---\nname: trigger-skill\ndescription: irrelevant\ntriggers: [\"rate limit\"]\npriority: 0\n---\n\nbody\n";
        let desc_skill = "---\nname: desc-skill\ndescription: rate limit handling helper\ntriggers: [\"unrelated\"]\npriority: 0\n---\n\nbody\n";
        let reg = registry_with(&[("trigger-skill", trigger_skill), ("desc-skill", desc_skill)]);
        let router = SkillRouter::new(reg);
        let hits = router.route("we just hit a rate limit on provider X");
        // Both should match (trigger-skill via trigger, desc-skill via
        // description), but trigger-skill scores higher.
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].skill.name, "trigger-skill");
        assert!(hits[0].match_score > hits[1].match_score);
    }

    #[test]
    fn priority_breaks_ties() {
        let high = "---\nname: high\ndescription: handles widgets\ntriggers: [\"widget\"]\npriority: 8\n---\n\nbody\n";
        let low = "---\nname: low\ndescription: handles widgets\ntriggers: [\"widget\"]\npriority: 1\n---\n\nbody\n";
        let reg = registry_with(&[("high", high), ("low", low)]);
        let router = SkillRouter::new(reg);
        let hits = router.route("widget broken");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].skill.name, "high");
        // (1.0 + 0 + 0.8) vs (1.0 + 0 + 0.1)
        assert!((hits[0].match_score - 1.8).abs() < f32::EPSILON);
        assert!((hits[1].match_score - 1.1).abs() < f32::EPSILON);
    }

    #[test]
    fn match_is_case_insensitive() {
        let body = "---\nname: rl\ndescription: x\ntriggers: [\"rate limit hit\"]\npriority: 0\n---\n\nbody\n";
        let reg = registry_with(&[("rl", body)]);
        let router = SkillRouter::new(reg);
        let hits = router.route("RATE LIMIT HIT on shard 3");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matched_trigger.as_deref(), Some("rate limit hit"));
    }

    #[test]
    fn matched_trigger_is_first_match() {
        let body = "---\nname: multi\ndescription: x\ntriggers: [\"first match\", \"second match\"]\npriority: 0\n---\n\nbody\n";
        let reg = registry_with(&[("multi", body)]);
        let router = SkillRouter::new(reg);
        let hits = router.route("we see a first match AND a second match");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matched_trigger.as_deref(), Some("first match"));
    }

    #[test]
    fn description_only_match_yields_no_matched_trigger() {
        let body = "---\nname: d\ndescription: orange marmalade brewery\ntriggers: [\"unused\"]\npriority: 0\n---\n\nbody\n";
        let reg = registry_with(&[("d", body)]);
        let router = SkillRouter::new(reg);
        let hits = router.route("operator: what about orange?");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].matched_trigger.is_none());
        // 0.0 + 0.5 + 0.0 priority
        assert!((hits[0].match_score - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn ties_break_alphabetically() {
        let a =
            "---\nname: alpha\ndescription: x\ntriggers: [\"widget\"]\npriority: 0\n---\n\nbody\n";
        let b =
            "---\nname: beta\ndescription: x\ntriggers: [\"widget\"]\npriority: 0\n---\n\nbody\n";
        let reg = registry_with(&[("alpha", a), ("beta", b)]);
        let router = SkillRouter::new(reg);
        let hits = router.route("widget");
        assert_eq!(hits[0].skill.name, "alpha");
        assert_eq!(hits[1].skill.name, "beta");
    }
}
