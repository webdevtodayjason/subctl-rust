//! Prompt templates for the structured planning UX.
//!
//! These are the *spec* of the planning conversation — the operator-
//! facing UX shape lives here, not in the partner code that wires them.
//! They will evolve as Evy learns what makes planning sessions actually
//! useful (see ADR 0020 §"residual risks" — the UX shape is the most
//! open variable left at blueprint-freeze).
//!
//! # Conversation shape this prompt drives
//!
//! 1. **First turn** (no operator input yet): the LLM produces 3-5
//!    targeted clarifying questions about `topic`. NOT free chat —
//!    structured Q&A. This is what surfaces show the operator after
//!    [`crate::ThinkingPartner::start_session`] returns.
//!
//! 2. **Operator answers** (one or more turns of `Role::Operator`): the
//!    LLM produces a draft plan with four sections:
//!
//!    * **Goal** (one sentence)
//!    * **Unknowns** (what to investigate first)
//!    * **Approach** (proposed steps)
//!    * **Risks** (what could go wrong)
//!
//!    Each iteration ends with `"Anything else to refine, or shall we
//!    conclude?"` so the operator knows the floor is open.
//!
//! 3. **On conclude** ([`crate::ThinkingPartner::conclude`] appends
//!    [`conclusion_user_turn`]): the LLM produces a final structured
//!    summary the operator can save as a playbook. Same four sections
//!    plus a `**Next steps**` block naming concrete handoff actions.
//!
//! # Why a single system prompt + templated user turns
//!
//! The Anthropic Messages API gives us a strong system slot. Putting the
//! shape rules there once (rather than re-asserting them on every user
//! turn) keeps the context window cheap and prevents the LLM from
//! drifting into chat mode mid-iteration.

/// The system prompt that drives the structured planning UX.
///
/// `topic` is interpolated verbatim — callers are responsible for
/// validating it (the partner guards against empty topics at
/// [`crate::ThinkingPartner::start_session`]).
#[must_use]
pub fn planning_system_prompt(topic: &str) -> String {
    format!(
        "You are Evy, a thinking-partner for a software-engineering operator. \
You are NOT writing code; you are drafting a plan. The operator wants to \
think through this topic with you:\n\
\n\
  {topic}\n\
\n\
Your job has three phases. Stay disciplined about which phase you are in.\n\
\n\
PHASE 1 — Clarify (your very first turn, before the operator has spoken):\n\
Ask 3-5 targeted clarifying questions about the topic. Number them. \
Pick questions whose answers actually change the plan — not generic ones. \
Do NOT draft a plan in this turn. Do NOT propose solutions. End with: \
\"Answer what you can; we'll iterate.\"\n\
\n\
PHASE 2 — Draft and iterate (every turn after the operator answers):\n\
Produce a structured plan with EXACTLY these four sections, in this order, \
as markdown headers:\n\
\n\
  **Goal** — one sentence.\n\
  **Unknowns** — what to investigate first, bulleted.\n\
  **Approach** — proposed steps, numbered.\n\
  **Risks** — what could go wrong, bulleted.\n\
\n\
After the four sections, end with exactly:\n\
\"Anything else to refine, or shall we conclude?\"\n\
\n\
On subsequent turns, refine the existing plan — don't restart it. \
Preserve what's settled, change what the operator pushed back on.\n\
\n\
PHASE 3 — Conclude (when the operator says they're done):\n\
Produce a final structured summary with the same four sections, plus:\n\
\n\
  **Next steps** — concrete handoff actions, numbered. \
Each should be specific enough that the operator (or a dispatched worker) \
could act on it without further clarification.\n\
\n\
Do not ask further questions in PHASE 3. This is the artifact the operator \
will save as a playbook.\n\
\n\
General rules:\n\
- Be concise. Operator time is the scarce resource.\n\
- Surface unknowns explicitly; don't paper over them with optimism.\n\
- If the operator says something that contradicts an earlier assumption, \
say so and revise — don't quietly continue.\n\
- You do not have web access, file access, or the ability to spawn workers. \
You are reasoning out loud."
    )
}

/// The synthetic user turn the partner pushes at session open. The
/// Anthropic Messages API requires `messages` to contain at least one
/// entry whose role is `user`, and the model only "responds" to the
/// last user turn. Without this kickoff, [`crate::ThinkingPartner::start_session`]
/// would emit an empty `messages` array and the backend would reject
/// the call with HTTP 400.
///
/// This message IS recorded in the session log as a `Role::Operator`
/// turn (so it round-trips through serde and the `on_message` hook),
/// but surfaces (TUI / Discord) typically suppress it from the
/// operator-visible thread — it's a wire-protocol artifact, not
/// something the operator typed.
#[must_use]
pub fn kickoff_user_turn() -> String {
    "Begin. Produce your PHASE 1 clarifying questions about the topic now.".to_string()
}

/// The synthetic user turn the partner appends when the operator
/// invokes [`crate::ThinkingPartner::conclude`]. Asks the LLM to switch
/// from PHASE 2 (iterate) to PHASE 3 (final summary).
///
/// Kept as a function (not a `const`) so it can grow conditional shape
/// later — e.g. "operator has provided N corrections; produce the
/// summary with a `Corrections` callout".
#[must_use]
pub fn conclusion_user_turn() -> String {
    "Let's conclude. Produce the final structured summary now \
(PHASE 3, including the Next steps section). Do not ask any further \
questions; this is the artifact I'll save as a playbook."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_includes_topic_verbatim() {
        let p = planning_system_prompt("migrate api to postgres 16");
        assert!(
            p.contains("migrate api to postgres 16"),
            "topic must be interpolated"
        );
    }

    #[test]
    fn system_prompt_names_all_four_sections() {
        let p = planning_system_prompt("x");
        for section in ["**Goal**", "**Unknowns**", "**Approach**", "**Risks**"] {
            assert!(p.contains(section), "missing section: {section}");
        }
    }

    #[test]
    fn system_prompt_declares_three_phases() {
        let p = planning_system_prompt("x");
        assert!(p.contains("PHASE 1"));
        assert!(p.contains("PHASE 2"));
        assert!(p.contains("PHASE 3"));
    }

    #[test]
    fn system_prompt_pins_the_iteration_closer() {
        let p = planning_system_prompt("x");
        assert!(
            p.contains("Anything else to refine, or shall we conclude?"),
            "iteration closer must be exact"
        );
    }

    #[test]
    fn system_prompt_pins_phase1_closer() {
        let p = planning_system_prompt("x");
        assert!(p.contains("Answer what you can; we'll iterate."));
    }

    #[test]
    fn system_prompt_forbids_code_generation() {
        let p = planning_system_prompt("x");
        assert!(p.contains("NOT writing code"));
    }

    #[test]
    fn conclusion_turn_invokes_phase3() {
        let t = conclusion_user_turn();
        assert!(t.contains("PHASE 3"));
        assert!(t.contains("Next steps"));
    }

    #[test]
    fn kickoff_turn_is_non_empty_and_invokes_phase1() {
        let t = kickoff_user_turn();
        assert!(
            !t.trim().is_empty(),
            "Anthropic requires non-empty user turn"
        );
        assert!(t.contains("PHASE 1"));
    }

    #[test]
    fn empty_topic_still_renders() {
        // The partner is responsible for rejecting empty topics; the
        // template itself must not panic on one.
        let p = planning_system_prompt("");
        assert!(p.contains("PHASE 1"));
    }
}
