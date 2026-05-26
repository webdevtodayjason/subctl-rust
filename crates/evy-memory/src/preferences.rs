//! Operator-preference model — layer 5 of the learning loop.
//!
//! ADR 0020 §"Layer 5 — Operator-preference model". Concretely:
//! Evy's own model of what the operator wants. Distinct from the
//! operator-authored auto-memory at
//! `~/.claude/projects/-Users-sem-code-subctl/memory/` which the
//! operator curates; this store is updated implicitly from corrections
//! and from explicit `OperatorPreference` feedback events.
//!
//! Storage is a single sqlite table (`operator_preferences`) with one
//! row per key. `value` is a JSON-serialised [`PreferenceValue`]; `kind`
//! holds the discriminator (`boolean` / `text` / `number` / `list`) so
//! filtered queries don't pay JSON-parse cost.

use std::path::Path;

use chrono::Utc;
use evy_core::Result;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::error;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Opaque newtype for preference keys. A `String` wrapped so callers
/// can't accidentally mix it with arbitrary strings (and so we can
/// extend the type later — e.g., scoped keys — without a breaking
/// rename).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PreferenceKey(pub String);

impl PreferenceKey {
    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PreferenceKey {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for PreferenceKey {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Tagged enum of preference value shapes. The `tag = "kind"` form keeps
/// the JSON encoding self-describing — the storage layer reads the tag
/// out into the indexed `kind` column for cheap filtering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PreferenceValue {
    /// On/off flags ("loud idle banner", "use Codex for Rust").
    Boolean(bool),
    /// Free-form text ("prefer 'feat:' commit prefixes").
    Text(String),
    /// Numeric thresholds ("min batch size before bumping minor").
    Number(f64),
    /// Ordered list values ("preferred-providers": [Codex, ClaudeCode]).
    List(Vec<String>),
}

impl PreferenceValue {
    /// Discriminator string matching the `serde` rename.
    #[must_use]
    pub fn discriminator(&self) -> &'static str {
        match self {
            Self::Boolean(_) => "boolean",
            Self::Text(_) => "text",
            Self::Number(_) => "number",
            Self::List(_) => "list",
        }
    }
}

/// Sqlx-backed preference store. Cheap to clone — internally an
/// `Arc`-shared pool.
#[derive(Debug, Clone)]
pub struct OperatorPreferenceModel {
    pool: SqlitePool,
}

impl OperatorPreferenceModel {
    /// Open (or create) the preference store at `db_path` and run all
    /// evy-memory migrations. Safe to call against a db that another
    /// evy-memory store has already opened.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] if the database cannot be opened
    /// or if migrations fail.
    pub async fn open(db_path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .map_err(error::from_sqlx)?;
        MIGRATOR.run(&pool).await.map_err(error::from_migrate)?;
        Ok(Self { pool })
    }

    /// Upsert a preference. Subsequent reads of `key` see `value`.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Serde`] if `value` cannot be
    /// JSON-encoded, [`evy_core::Error::Io`] on insert/update failure.
    pub async fn set(&self, key: PreferenceKey, value: PreferenceValue) -> Result<()> {
        let kind = value.discriminator();
        let value_json = serde_json::to_string(&value)?;
        let ts = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO operator_preferences (key, value, kind, updated_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(key) DO UPDATE SET \
                value = excluded.value, \
                kind = excluded.kind, \
                updated_at = excluded.updated_at",
        )
        .bind(&key.0)
        .bind(value_json)
        .bind(kind)
        .bind(ts)
        .execute(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        Ok(())
    }

    /// Lookup the value for `key`, or `None` if no row exists.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure,
    /// [`evy_core::Error::Serde`] if the stored JSON cannot be parsed.
    pub async fn get(&self, key: &PreferenceKey) -> Result<Option<PreferenceValue>> {
        let row = sqlx::query("SELECT value FROM operator_preferences WHERE key = ?1")
            .bind(&key.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(error::from_sqlx)?;
        match row {
            Some(r) => {
                let s: String = r.try_get("value").map_err(error::from_sqlx)?;
                let value: PreferenceValue = serde_json::from_str(&s)?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    /// Every preference currently set, ordered by `updated_at` desc
    /// (most-recently-touched first).
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure,
    /// [`evy_core::Error::Serde`] if any stored JSON cannot be parsed.
    pub async fn list(&self) -> Result<Vec<(PreferenceKey, PreferenceValue)>> {
        let rows = sqlx::query(
            "SELECT key, value FROM operator_preferences \
             ORDER BY updated_at DESC, key ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let key: String = row.try_get("key").map_err(error::from_sqlx)?;
            let value_s: String = row.try_get("value").map_err(error::from_sqlx)?;
            let value: PreferenceValue = serde_json::from_str(&value_s)?;
            out.push((PreferenceKey(key), value));
        }
        Ok(out)
    }

    /// Drop the row at `key`. Idempotent — deleting a missing key
    /// succeeds.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on delete failure.
    pub async fn delete(&self, key: &PreferenceKey) -> Result<()> {
        sqlx::query("DELETE FROM operator_preferences WHERE key = ?1")
            .bind(&key.0)
            .execute(&self.pool)
            .await
            .map_err(error::from_sqlx)?;
        Ok(())
    }

    /// Preferences whose key contains `substring`. Case-sensitive — the
    /// caller normalises if needed. Used by retrieval to surface
    /// preferences matching a free-form query.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure,
    /// [`evy_core::Error::Serde`] if any stored JSON cannot be parsed.
    pub async fn search_keys(
        &self,
        substring: &str,
    ) -> Result<Vec<(PreferenceKey, PreferenceValue)>> {
        if substring.is_empty() {
            return self.list().await;
        }
        let pattern = format!("%{substring}%");
        let rows = sqlx::query(
            "SELECT key, value FROM operator_preferences \
             WHERE key LIKE ?1 \
             ORDER BY updated_at DESC, key ASC",
        )
        .bind(pattern)
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let key: String = row.try_get("key").map_err(error::from_sqlx)?;
            let value_s: String = row.try_get("value").map_err(error::from_sqlx)?;
            let value: PreferenceValue = serde_json::from_str(&value_s)?;
            out.push((PreferenceKey(key), value));
        }
        Ok(out)
    }
}

// TODO: Phase 4 — implicit preference inference. Today preferences are
//   only updated when a `FeedbackKind::OperatorPreference` event lands;
//   layer 5 of ADR 0020 also envisions inference from corrections
//   (`Rejected` / `Corrected` patterns).
// TODO: Phase 4 — scoped keys. Concrete keys like
//   `dispatch.prefer.rust.codex` work today; a structured key scheme
//   (workspace × subject × name) would let retrieval query by scope.
// TODO: Phase 4 — bilateral sync with the operator's auto-memory at
//   `~/.claude/projects/.../memory/` per ADR 0018. The v3 design
//   imagined two-way maintenance; this Rust store currently lives
//   independently.

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn fresh_model() -> (tempfile::TempDir, OperatorPreferenceModel) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let model = OperatorPreferenceModel::open(&path).await.expect("open");
        (dir, model)
    }

    #[tokio::test]
    async fn empty_get_returns_none() {
        let (_dir, model) = fresh_model().await;
        let v = model.get(&PreferenceKey::from("missing")).await.unwrap();
        assert!(v.is_none());
    }

    #[tokio::test]
    async fn set_then_get_roundtrips() {
        let (_dir, model) = fresh_model().await;
        let cases: Vec<(PreferenceKey, PreferenceValue)> = vec![
            (
                PreferenceKey::from("idle_banner_loud"),
                PreferenceValue::Boolean(true),
            ),
            (
                PreferenceKey::from("commit_prefix"),
                PreferenceValue::Text("feat: ".into()),
            ),
            (
                PreferenceKey::from("max_batch"),
                PreferenceValue::Number(3.0),
            ),
            (
                PreferenceKey::from("preferred_providers"),
                PreferenceValue::List(vec!["Codex".into(), "ClaudeCode".into()]),
            ),
        ];
        for (k, v) in &cases {
            model.set(k.clone(), v.clone()).await.unwrap();
        }
        for (k, v) in &cases {
            let got = model.get(k).await.unwrap();
            assert_eq!(got.as_ref(), Some(v));
        }
    }

    #[tokio::test]
    async fn set_upserts_existing_key() {
        let (_dir, model) = fresh_model().await;
        let key = PreferenceKey::from("idle_banner_loud");
        model
            .set(key.clone(), PreferenceValue::Boolean(true))
            .await
            .unwrap();
        model
            .set(key.clone(), PreferenceValue::Boolean(false))
            .await
            .unwrap();
        assert_eq!(
            model.get(&key).await.unwrap(),
            Some(PreferenceValue::Boolean(false))
        );
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let (_dir, model) = fresh_model().await;
        let key = PreferenceKey::from("tmp");
        model
            .set(key.clone(), PreferenceValue::Text("v".into()))
            .await
            .unwrap();
        model.delete(&key).await.unwrap();
        assert!(model.get(&key).await.unwrap().is_none());
        // Idempotent — second delete also Ok.
        model.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn list_returns_all_keys() {
        let (_dir, model) = fresh_model().await;
        model
            .set(PreferenceKey::from("a"), PreferenceValue::Boolean(true))
            .await
            .unwrap();
        model
            .set(PreferenceKey::from("b"), PreferenceValue::Number(1.0))
            .await
            .unwrap();
        let listed = model.list().await.unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn search_keys_substring_filters() {
        let (_dir, model) = fresh_model().await;
        model
            .set(
                PreferenceKey::from("dispatch.prefer.rust.codex"),
                PreferenceValue::Boolean(true),
            )
            .await
            .unwrap();
        model
            .set(
                PreferenceKey::from("dispatch.prefer.ts.claude"),
                PreferenceValue::Boolean(true),
            )
            .await
            .unwrap();
        model
            .set(
                PreferenceKey::from("idle_banner_loud"),
                PreferenceValue::Boolean(true),
            )
            .await
            .unwrap();
        let rust_hits = model.search_keys("rust").await.unwrap();
        assert_eq!(rust_hits.len(), 1);
        let dispatch_hits = model.search_keys("dispatch").await.unwrap();
        assert_eq!(dispatch_hits.len(), 2);
        let nothing = model.search_keys("zzz").await.unwrap();
        assert!(nothing.is_empty());
    }

    #[test]
    fn discriminator_matches_serde_tag() {
        let cases: Vec<(PreferenceValue, &str)> = vec![
            (PreferenceValue::Boolean(true), "boolean"),
            (PreferenceValue::Text("x".into()), "text"),
            (PreferenceValue::Number(1.0), "number"),
            (PreferenceValue::List(vec![]), "list"),
        ];
        for (v, expected) in cases {
            assert_eq!(v.discriminator(), expected);
            let json = serde_json::to_value(&v).unwrap();
            assert_eq!(json["kind"].as_str(), Some(expected));
        }
    }
}
