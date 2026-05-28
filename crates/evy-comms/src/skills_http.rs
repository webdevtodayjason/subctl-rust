//! Phase 6 follow-up — `GET /api/evy/skills`.
//!
//! Thin proxy onto [`evy_skills::SkillRegistry::list`]. Returns the
//! catalog every chat turn could load via the `skill_view` tool, in the
//! deterministic alphabetised order the registry persists.
//!
//! # Wire shape
//!
//! ```json
//! {
//!   "skills": [
//!     {"name": "plan", "description": "...", "triggers": ["a", "b"], "priority": 0},
//!     ...
//!   ]
//! }
//! ```
//!
//! `priority` is included because the operator's skill catalog ranks
//! some skills above others — the TUI surface may want to render high-
//! priority skills in bold. `path` is intentionally omitted (an absolute
//! on-disk path is operator-machine specific and unhelpful to a remote
//! client).
//!
//! Returns 503 with `{"kind":"unavailable"}` when the daemon was not
//! built with a skill registry. Mirrors `chat.rs`'s "no partner" branch.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};

use crate::http::HttpState;

/// One row in the skills listing. Mirrors a subset of
/// [`evy_skills::Skill`] — operator-facing fields only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillSummary {
    /// Canonical skill identifier (frontmatter `name` or directory name).
    pub name: String,
    /// One-line description from frontmatter. Empty string when omitted.
    pub description: String,
    /// Substrings the skill router scans for. Empty vec when the skill
    /// has no triggers list.
    pub triggers: Vec<String>,
    /// Operator-supplied ranking nudge; `0` when not set. Higher =
    /// preferred.
    pub priority: i32,
}

/// JSON body returned by [`skills_list_handler`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsListResponse {
    /// All skills, in the registry's deterministic alphabetised order.
    pub skills: Vec<SkillSummary>,
}

/// Failure modes the handler emits. The only failure today is "no
/// registry"; serialised as `{"kind":"unavailable","message":"..."}` to
/// match the shape `ChatError::Unavailable` uses.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillsError {
    /// No skill registry attached to the daemon's `AppState`.
    Unavailable {
        /// Diagnostic message.
        message: String,
    },
}

impl IntoResponse for SkillsError {
    fn into_response(self) -> Response {
        let status = match self {
            SkillsError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(self)).into_response()
    }
}

/// `GET /api/evy/skills` handler.
///
/// `pub(crate)` because the handler's signature carries the crate-private
/// `HttpState`; downstream consumers re-derive [`SkillsListResponse`] /
/// [`SkillSummary`] structurally.
///
/// # Errors
/// Returns [`SkillsError::Unavailable`] (HTTP 503) when no skill registry
/// is attached.
pub(crate) async fn skills_list_handler(
    State(state): State<HttpState>,
) -> std::result::Result<Json<SkillsListResponse>, SkillsError> {
    let reg = state.app.skills().ok_or_else(|| SkillsError::Unavailable {
        message: "no skill registry attached to this daemon".to_string(),
    })?;

    let skills: Vec<SkillSummary> = reg
        .list()
        .iter()
        .map(|s| SkillSummary {
            name: s.name.clone(),
            description: s.description.clone(),
            triggers: s.triggers.clone(),
            priority: s.priority,
        })
        .collect();

    Ok(Json(SkillsListResponse { skills }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_error_unavailable_maps_to_503() {
        let e = SkillsError::Unavailable {
            message: "x".into(),
        };
        let resp = e.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn skill_summary_round_trips_through_serde() {
        let s = SkillSummary {
            name: "plan".into(),
            description: "draft a plan".into(),
            triggers: vec!["plan".into(), "outline".into()],
            priority: 3,
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: SkillSummary = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }
}
