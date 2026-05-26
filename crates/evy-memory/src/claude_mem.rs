//! Read-only consumer of the operator's `claude-mem` SQLite database.
//!
//! `claude-mem` is a separate npm/CLI package that maintains a sqlite
//! database of cross-session observations (typically at
//! `~/.claude-mem/claude-mem.db`). ADR 0010 established claude-mem as a
//! parallel tier-4 corpus; v4 keeps that boundary by *consuming* the
//! database read-only rather than re-implementing the capture pipeline.
//!
//! The team-lead spec for slice 2C directs us to read claude-mem
//! **directly via sqlx** (rather than shelling out to `claude-mem
//! search`, as ADR 0020 §"Layer 2" suggested). This is faster and avoids
//! a subprocess dependency, at the cost of being coupled to claude-mem's
//! schema. The reader is built against schema version observed
//! 2026-05-26 against `claude-mem` 13.x: tables `observations`,
//! `session_summaries`, `sdk_sessions`, with FTS5 virtuals over each.
//!
//! Only the `observations` table is queried at this slice. Pulling from
//! `session_summaries` is a Phase 3 follow-up — see TODO at end of
//! module.

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use evy_core::Result;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::error;

/// A single episode read out of claude-mem's `observations` table.
///
/// Field shapes intentionally mirror claude-mem's storage (no
/// presentation-layer translation here). The `kind` string is the
/// claude-mem `type` column verbatim (`"bugfix"`, `"change"`,
/// `"decision"`, `"discovery"`, `"feature"`, `"refactor"`,
/// `"security_alert"`, `"security_note"`). Emoji rendering belongs to
/// any UI that displays these.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Episode {
    /// Stable id in claude-mem's id space. We prefix the integer PK with
    /// `"obs-"` to leave room for future tables (e.g. `"sum-123"` for
    /// session summaries) without colliding.
    pub id: String,
    /// Created-at timestamp (claude-mem stores ISO-8601 + epoch).
    pub ts: DateTime<Utc>,
    /// Project slug as claude-mem recorded it (e.g. `"subctl"`).
    pub project: String,
    /// Best human-readable summary: claude-mem's `title`, falling back
    /// to a truncation of `text` when `title` is null.
    pub summary: String,
    /// claude-mem's `type` column verbatim.
    pub kind: String,
}

/// Read-only handle to claude-mem's sqlite database.
#[derive(Debug, Clone)]
pub struct ClaudeMemReader {
    pool: SqlitePool,
}

impl ClaudeMemReader {
    /// Open `db_path` read-only. The file MUST exist (we do NOT pass
    /// `create_if_missing` — claude-mem owns the database; if the file
    /// is absent we want to surface an error, not lay a fresh empty
    /// schema on top of the operator's actual data).
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] if the database cannot be opened
    /// or the schema is incompatible (a probe query is run before
    /// returning).
    pub async fn open(db_path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(false)
            .read_only(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await
            .map_err(error::from_sqlx)?;
        // Smoke probe: confirm the observations table exists. Cheap,
        // and surfaces "wrong file" / "out-of-date schema" loudly.
        sqlx::query("SELECT 1 FROM observations LIMIT 1")
            .fetch_optional(&pool)
            .await
            .map_err(error::from_sqlx)?;
        Ok(Self { pool })
    }

    /// Most-recent `limit` episodes (newest first).
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure or
    /// [`evy_core::Error::InvalidMandate`] if a row's timestamp is
    /// malformed.
    pub async fn recent_episodes(&self, limit: usize) -> Result<Vec<Episode>> {
        let rows = sqlx::query(
            "SELECT id, project, type AS kind, title, text, \
                    created_at, created_at_epoch \
             FROM observations \
             ORDER BY created_at_epoch DESC LIMIT ?1",
        )
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        rows.into_iter().map(decode_episode).collect()
    }

    /// Full-text search via claude-mem's FTS5 virtual table.
    ///
    /// `query` is wrapped in double-quotes so callers can pass arbitrary
    /// operator text without triggering FTS5 syntax errors on special
    /// characters (`:`, `*`, `^`, etc.). The escape is intentional:
    /// quotes inside `query` are doubled. Result rows are ordered by
    /// recency, not BM25 — we want "match + recent" rather than "best
    /// lexical match" for decision-time retrieval.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure or
    /// [`evy_core::Error::InvalidMandate`] if a row's timestamp is
    /// malformed.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Episode>> {
        if query.trim().is_empty() {
            return self.recent_episodes(limit).await;
        }
        let escaped = query.replace('"', "\"\"");
        let phrase = format!("\"{escaped}\"");
        let rows = sqlx::query(
            "SELECT o.id, o.project, o.type AS kind, o.title, o.text, \
                    o.created_at, o.created_at_epoch \
             FROM observations o \
             JOIN observations_fts fts ON o.id = fts.rowid \
             WHERE observations_fts MATCH ?1 \
             ORDER BY o.created_at_epoch DESC LIMIT ?2",
        )
        .bind(phrase)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        rows.into_iter().map(decode_episode).collect()
    }
}

fn decode_episode(row: sqlx::sqlite::SqliteRow) -> Result<Episode> {
    let id_int: i64 = row.try_get("id").map_err(error::from_sqlx)?;
    let project: String = row.try_get("project").map_err(error::from_sqlx)?;
    let kind: String = row.try_get("kind").map_err(error::from_sqlx)?;
    let title: Option<String> = row.try_get("title").map_err(error::from_sqlx)?;
    let text: Option<String> = row.try_get("text").map_err(error::from_sqlx)?;
    // Prefer the indexed epoch column; fall back to parsing the ISO
    // string if it's somehow missing.
    let epoch: Option<i64> = row.try_get("created_at_epoch").ok();
    let ts = match epoch {
        Some(ms) => Utc
            .timestamp_millis_opt(ms)
            .single()
            .ok_or_else(|| error::bad_row(format!("claude-mem bad epoch {ms}")))?,
        None => {
            let s: String = row.try_get("created_at").map_err(error::from_sqlx)?;
            DateTime::parse_from_rfc3339(&s)
                .map_err(|e| error::bad_row(format!("claude-mem ts `{s}`: {e}")))?
                .with_timezone(&Utc)
        }
    };
    let summary = title
        .filter(|s| !s.is_empty())
        .or_else(|| text.as_ref().map(|t| truncate(t, 160)))
        .unwrap_or_default();
    Ok(Episode {
        id: format!("obs-{id_int}"),
        ts,
        project,
        summary,
        kind,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// TODO: Phase 3 — also consume `session_summaries` (richer prose: request /
//        investigated / learned / completed / next_steps). The current
//        `observations` corpus is sufficient for retrieval substrate; the
//        summary corpus is the natural source for distilled-playbook
//        candidates.
// TODO: Phase 3 — surface the BM25 score from observations_fts so
//        decision-time retrieval can weight lexical match against
//        recency rather than picking recency unconditionally.
// TODO: Phase 3 — locate-or-fail-open: ADR 0020 §"Layer 2" says the
//        consumer must fail open if claude-mem isn't installed. Today
//        callers handle that via Option<Arc<ClaudeMemReader>>; promote
//        to an explicit `try_open` that returns `Ok(None)` on missing
//        db file.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_handles_unicode_safely() {
        // 5-char unicode string, ask for 3 chars; must not split a
        // multi-byte codepoint or panic.
        let s = "héllo";
        let t = truncate(s, 3);
        assert_eq!(t.chars().count(), 4); // 3 chars + ellipsis
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_passthrough_when_short() {
        assert_eq!(truncate("hi", 10), "hi");
    }
}
