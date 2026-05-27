//! `evy-skills` — worker-side procedural knowledge for Evy v4.
//!
//! A **skill** is a markdown file with YAML frontmatter that captures
//! "how to handle X". Skills are loaded from a directory at daemon
//! startup and consulted by a [`SkillRouter`] when a worker is being
//! spawned — the router matches the worker's situation against the
//! registry and returns the skills that apply, sorted by relevance.
//!
//! ## Skills vs. playbooks
//!
//! `evy-memory::PlaybookStore` already loads markdown-plus-frontmatter
//! procedures. **Skills are distinct** even though the file format is
//! similar:
//!
//! | | Playbooks (`evy-memory`) | Skills (this crate) |
//! |---|---|---|
//! | Consumer | Evy herself (orchestrator) | Workers (children of Evy) |
//! | Lifecycle | Long-lived, operator-authored | Per-worker, dispatched on spawn |
//! | Ownership | Operator | Skill library curated by the team |
//!
//! Two consumers, two lifecycles. Do not collapse them.
//!
//! ## File layout
//!
//! Skills live under a directory in the Claude Code convention:
//!
//! ```text
//! <dir>/<skill-name>/SKILL.md
//! ```
//!
//! …with frontmatter like:
//!
//! ```yaml
//! ---
//! name: my-skill
//! description: One-line description
//! triggers: ["how do I X", "when should I Y"]
//! priority: 5
//! ---
//! ```
//!
//! See ADR 0020 §"Layer 4" for the architectural context that motivates
//! skills as a distinct surface from playbooks.

mod error;
mod registry;
mod router;
mod skill;

pub use registry::SkillRegistry;
pub use router::{RoutedSkill, SkillRouter};
pub use skill::Skill;
