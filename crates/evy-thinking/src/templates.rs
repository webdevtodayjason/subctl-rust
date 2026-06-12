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
- You are reasoning out loud; your capabilities on this surface are stated \
below."
    )
}

/// Embedded canonical Evy persona spec, vendored verbatim from the v3 repo
/// (`docs/persona/evy.md`, operator-authored, 2026-05-12). Source of truth for
/// Evy's voice; see ADR 0004 (persona/librarian framing).
///
/// The FULL spec (voice + operational orchestration rules, ~7.3k tokens) is
/// kept vendored for planning / dispatch mode (future wiring). Conversational
/// chat uses the slim [`EVY_VOICE`] brief instead, so casual turns don't pay
/// the full persona's prefill cost — see [`conversational_system_prompt`].
#[allow(dead_code)]
const EVY_PERSONA: &str = include_str!("persona/evy.md");

/// Slim voice brief (~600 tokens) — Evy's identity + how she speaks, with the
/// orchestration spec (tool-calling, dispatch, templates) stripped. Used by
/// [`conversational_system_prompt`]; the chat surface can't dispatch anyway.
const EVY_VOICE: &str = include_str!("persona/evy-voice.md");

/// System prompt for **conversational** chat — Evy speaking as herself.
///
/// Unlike [`planning_system_prompt`], this does NOT force the 3-phase
/// topic→plan→conclude flow. It loads Evy's slim voice brief ([`EVY_VOICE`])
/// so ordinary exchanges ("hello") sound like Evy rather than a clinical
/// planning instrument — without shipping the full ~7.3k-token persona every
/// turn. When the operator explicitly asks to plan something, the chat surface
/// switches that session to [`planning_system_prompt`].
#[must_use]
pub fn conversational_system_prompt() -> String {
    format!(
        "You are Evy. The text between the markers below is your canonical persona \
specification, authored by the operator — embody it: adopt her voice, her perspective, \
and the way she refers to herself and her role as subctl's orchestrator.\n\
\n\
Hold a natural conversation. You do NOT have to turn every message into a plan or a \
structured artifact — a greeting just gets Evy. If the operator asks you to think \
through, design, or plan something concrete, you may run a structured planning session \
(Goal / Unknowns / Approach / Risks); otherwise simply talk with them as Evy.\n\
\n\
Your capabilities on this surface are stated below — believe that statement, not \
assumptions.\n\
\n\
--- BEGIN EVY PERSONA (canonical, operator-authored) ---\n\
{EVY_VOICE}\n\
--- END EVY PERSONA ---"
    )
}

/// Capability brief appended (by [`crate::ThinkingPartner`]) for
/// backends whose [`crate::LlmBackend::capability_brief`] advertises an
/// active tool registry. States which tools exist and the contract for
/// using them — most importantly that results must come from real
/// calls, never be invented.
#[must_use]
pub fn tool_capability_brief(tool_names: &[String]) -> String {
    format!(
        "CAPABILITIES — you have live tools on this surface: {names}. \
Call a tool whenever the operator asks about accounts, usage, sessions, \
workers, or watchdogs — the tool result is the live truth; your training \
data and the conversation are not. NEVER invent or guess a tool result: \
if a tool errors, say so plainly. Action tools (spawn / kill / notify) \
are policy-gated and audit-logged; use them only when the operator \
clearly asked for the action, and report exactly what the tool returned.",
        names = tool_names.join(", ")
    )
}

/// Capability brief appended (by [`crate::ThinkingPartner`]) for
/// backends WITHOUT an active tool registry. Replaces the old blanket
/// "no file/web/worker access" line — still true here, but now paired
/// with guidance to answer fleet questions from the injected live
/// status block when one is present.
#[must_use]
pub fn no_tools_brief() -> String {
    "CAPABILITIES — you do not have direct file, web, or worker-spawn \
access on this surface. If a LIVE FLEET STATUS block appears below, it \
is real telemetry gathered moments ago: answer questions about \
accounts, usage, sessions, workers, and watchdogs from it. If the \
block is absent and the operator asks about live state, say you cannot \
see it right now — do not guess."
        .to_string()
}

/// Header line wrapped around the injected live-status block so the
/// model (and anyone reading a transcript) can tell telemetry from
/// conversation. The partner appends `status_header() + block`.
#[must_use]
pub fn status_header() -> String {
    "── LIVE FLEET STATUS (auto-gathered; trust over memory) ──".to_string()
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

    #[test]
    fn conversational_prompt_embeds_persona_and_is_not_planning() {
        let p = conversational_system_prompt();
        assert!(p.contains("You are Evy"), "must establish Evy");
        assert!(
            p.contains("subCTL") || p.contains("orchestrator"),
            "must embed the canonical persona body (vendored evy.md)"
        );
        assert!(
            !p.contains("PHASE 1"),
            "conversational prompt must NOT force the structured planning phases"
        );
    }
}
