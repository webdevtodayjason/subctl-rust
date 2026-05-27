//! `accounts.conf` reader + `auth.json` reader/writer for OAuth tokens.
//!
//! ## On-disk reality (verified before writing this module)
//!
//! `accounts.conf` lives at `~/.config/subctl/accounts.conf` and is a
//! pipe-delimited text file — **NOT** a token store. The format is:
//!
//! ```text
//! # subctl accounts (header comment)
//! # Format: alias|provider|email|config_dir|description
//! claude-jason    | claude        | jason@webdevtoday.com  | ~/.claude-jason         | Daily driver
//! openai-jason    | openai-codex  | jbrashear72@icloud.com | /Users/sem/.codex-jason | Codex personal
//! ```
//!
//! Tokens live in `<config_dir>/auth.json` (Codex) or in
//! `~/.config/subctl/evy/oauth/xai-oauth.json` (xAI single-tenant for now).
//!
//! The spec's `AccountRecord { name, provider, access_token,
//! refresh_token, expires_at }` is therefore the **combined view** —
//! [`AccountsStore::get`] reads the row from `accounts.conf` and joins the
//! `auth.json` to populate the token fields. [`AccountsStore::put`] writes
//! the auth.json **atomically** (tmp+rename, 0600 perms); it does NOT mutate
//! the accounts.conf row (those are operator-managed; subctl doesn't
//! register new accounts behind the operator's back).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use evy_core::Result;
use serde::{Deserialize, Serialize};

use super::OauthError;

/// One row out of `accounts.conf` (no tokens — those live elsewhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    /// Short alias the operator types (`openai-jason`).
    pub alias: String,
    /// Which provider this row belongs to (`claude` | `openai-codex` |
    /// `xai-oauth`). Free-form string for forward-compat with new providers.
    pub provider: String,
    /// Operator's email for provenance / display only.
    pub email: String,
    /// Per-account config directory (where `auth.json` lives, plus
    /// provider-specific state). Tilde-expanded by the parser.
    pub config_dir: PathBuf,
    /// Free-form description column (everything after the 4th `|`).
    pub description: String,
}

/// Tokens persisted to `<config_dir>/auth.json`. Mirrors v3's shape so a
/// rollback / sidecar v3 CLI keeps working with files we wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Current bearer token (OpenAI: JWT; xAI: opaque).
    pub access_token: String,
    /// Long-lived refresh token. Optional because some flows / mock data
    /// don't include it; in practice both Codex and xAI always rotate one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// ISO-8601 timestamp of the most recent refresh (or initial mint).
    /// Stored separately from `expires_at` because v3 records this for
    /// observability ("when did we last touch this account").
    pub last_refresh: DateTime<Utc>,
    /// Absolute expiry as derived from the provider's `expires_in`.
    pub expires_at: DateTime<Utc>,
}

/// The combined view: row metadata joined with the token blob. This is what
/// [`AccountsStore::get`] returns — matches the team-lead's spec shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    /// Alias (a.k.a. `name` in the spec).
    pub name: String,
    /// Provider string.
    pub provider: String,
    /// Bearer token currently on disk.
    pub access_token: String,
    /// Refresh token (None if the on-disk file lacks one).
    pub refresh_token: Option<String>,
    /// Token expiry timestamp.
    pub expires_at: DateTime<Utc>,
}

/// Accounts store. Holds the path to `accounts.conf` and lazy-parses on
/// access; the in-memory cache is owned by the caller (call sites are
/// expected to re-`open` if they need a fresh read).
#[derive(Debug, Clone)]
pub struct AccountsStore {
    /// Absolute path to `accounts.conf`.
    pub path: PathBuf,
}

impl AccountsStore {
    /// Construct a store pointing at the given `accounts.conf`. **Does not
    /// touch the file** — `get` / `put` do the I/O lazily so construction
    /// is infallible. We accept the path even if it doesn't exist yet so
    /// tests can point at a temp dir before any rows have been written.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Parse every non-comment row from `accounts.conf`. Returns an empty
    /// vec (NOT an error) if the file doesn't exist — matches v3 behavior
    /// where the daemon starts fine on a fresh machine.
    pub fn list_rows(&self) -> Result<Vec<AccountRow>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path).map_err(OauthError::Io)?;
        Ok(parse_accounts_conf(&text))
    }

    /// Look up one row by alias. None if there's no matching row.
    pub fn find_row(&self, alias: &str) -> Result<Option<AccountRow>> {
        Ok(self.list_rows()?.into_iter().find(|r| r.alias == alias))
    }

    /// Read the combined account+token view. Returns `Ok(None)` when the
    /// alias has no `accounts.conf` row OR has a row but no token blob on
    /// disk yet.
    pub async fn get(&self, account: &str) -> Result<Option<AccountRecord>> {
        let row = match self.find_row(account)? {
            Some(r) => r,
            None => return Ok(None),
        };
        let tokens = match read_token_file(&token_path_for(&row)) {
            Ok(Some(t)) => t,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(AccountRecord {
            name: row.alias,
            provider: row.provider,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        }))
    }

    /// Persist the token half of an [`AccountRecord`] back to disk. Looks
    /// up the row by `record.name`, writes the auth.json under that row's
    /// `config_dir` atomically with 0600 perms. Fails if the row doesn't
    /// exist — subctl never invents new accounts.conf entries on token
    /// refresh.
    pub async fn put(&self, record: AccountRecord) -> Result<()> {
        let row = self
            .find_row(&record.name)?
            .ok_or_else(|| OauthError::InvalidResponse {
                provider: "accounts",
                reason: format!("no accounts.conf row for alias {}", record.name),
            })?;
        let tokens = TokenRecord {
            access_token: record.access_token,
            refresh_token: record.refresh_token,
            last_refresh: Utc::now(),
            expires_at: record.expires_at,
        };
        write_token_file(&token_path_for(&row), &tokens)?;
        Ok(())
    }
}

// ─── parsing ────────────────────────────────────────────────────────────────

/// Parse the full `accounts.conf` text. Public so unit tests outside this
/// module can exercise it; not re-exported from the crate root.
pub fn parse_accounts_conf(text: &str) -> Vec<AccountRow> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if let Some(row) = parse_row(line) {
            rows.push(row);
        }
    }
    rows
}

fn parse_row(line: &str) -> Option<AccountRow> {
    let stripped = line.trim();
    if stripped.is_empty() || stripped.starts_with('#') {
        return None;
    }
    let fields: Vec<&str> = stripped.split('|').map(str::trim).collect();
    if fields.len() < 4 {
        return None;
    }
    let alias = fields[0];
    let provider = fields[1];
    let email = fields[2];
    let config_dir = fields[3];
    let description = if fields.len() > 4 {
        fields[4..].join(" | ")
    } else {
        String::new()
    };
    if alias.is_empty() || provider.is_empty() || config_dir.is_empty() {
        return None;
    }
    Some(AccountRow {
        alias: alias.to_string(),
        provider: provider.to_string(),
        email: email.to_string(),
        config_dir: expand_tilde(config_dir),
        description,
    })
}

/// Tilde-expand a path string the same way `lib/core.sh` and v3's
/// `openai-codex-auth.ts` do.
pub(crate) fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(h) = home_dir() {
            return h;
        }
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// ─── token file I/O ─────────────────────────────────────────────────────────

/// Where the token blob lives for a given account. For Codex (the only
/// per-account provider today) it's `<config_dir>/auth.json`. Future
/// providers may key off `row.provider` to pick a different name.
pub(crate) fn token_path_for(row: &AccountRow) -> PathBuf {
    row.config_dir.join("auth.json")
}

/// On-disk shape of `auth.json` — mirrors v3's
/// `completeCodexLogin` write exactly so a v3 reader (or rollback) sees an
/// unchanged file. Extra fields (`OPENAI_API_KEY`, `_subctl`) preserved on
/// round-trip via `serde_json::Value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<serde_json::Value>,
    tokens: AuthTokens,
    last_refresh: DateTime<Utc>,
    /// Subctl metadata (alias, email, minted_by, minted_at). Round-tripped
    /// opaquely — we don't introspect or rewrite it on refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    _subctl: Option<serde_json::Value>,
    /// Catch-all for any fields v3 added that we don't know about — keeps
    /// the on-disk file losslessly round-trippable.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthTokens {
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// v3 also stores `id_token` and `account_id` here; round-tripped via
    /// `extra`.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// Read + parse `<path>` as auth.json. Returns Ok(None) on missing file (a
/// freshly-configured alias that hasn't logged in yet). Errors only on
/// permission / parse failures.
pub fn read_token_file(path: &Path) -> std::result::Result<Option<TokenRecord>, OauthError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut text = String::new();
    File::open(path)?.read_to_string(&mut text)?;
    let auth: AuthJson = serde_json::from_str(&text)?;
    // v3's expires_at is not on disk explicitly — it's derived from the JWT
    // `exp` claim or absent entirely. We tolerate both: if there's an
    // `expires_at` field in `extra`, use it; otherwise fall back to
    // `last_refresh` (the on-disk timestamp the v3 writer stamps), and let
    // the caller decide via `is_expiring` whether to refresh.
    let expires_at = auth
        .extra
        .get("expires_at")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or(auth.last_refresh);
    Ok(Some(TokenRecord {
        access_token: auth.tokens.access_token,
        refresh_token: auth.tokens.refresh_token,
        last_refresh: auth.last_refresh,
        expires_at,
    }))
}

/// Atomically write the token blob to `<path>` with 0600 perms. Mirrors v3's
/// `atomicWriteAuthFile`: open a `.tmp.<pid>.<rand>` sibling with O_EXCL +
/// 0o600, write, fsync, rename into place. Never leaves the secret
/// world-readable for a TOCTOU window.
pub fn write_token_file(path: &Path, record: &TokenRecord) -> std::result::Result<(), OauthError> {
    let dir = path.parent().ok_or_else(|| OauthError::InvalidResponse {
        provider: "accounts",
        reason: format!("auth.json path {} has no parent dir", path.display()),
    })?;
    std::fs::create_dir_all(dir)?;
    // 0o700 the dir if it's brand-new — same defense as v3's mkdirSync.
    set_dir_perms_0700(dir).ok();

    let auth = AuthJson {
        openai_api_key: Some(serde_json::Value::Null),
        tokens: AuthTokens {
            access_token: record.access_token.clone(),
            refresh_token: record.refresh_token.clone(),
            extra: HashMap::new(),
        },
        last_refresh: record.last_refresh,
        _subctl: None,
        extra: {
            // Persist expires_at in the `extra` bag so re-reads recover it;
            // v3 writers never set this so reading a v3-written file still
            // works (we fall back to last_refresh).
            let mut m = HashMap::new();
            m.insert(
                "expires_at".to_string(),
                serde_json::Value::String(record.expires_at.to_rfc3339()),
            );
            m
        },
    };
    let json = serde_json::to_string_pretty(&auth)?;

    let mut suffix = [0u8; 8];
    use rand::Rng;
    rand::rng().fill_bytes(&mut suffix);
    let suffix_hex: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
    let tmp_name = format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("auth.json"),
        std::process::id(),
        suffix_hex
    );
    let tmp_path = dir.join(tmp_name);

    // O_EXCL prevents a racing writer from clobbering our temp; 0o600 means
    // the secret is never group/world-readable, even mid-write.
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    // Rename is atomic on the same filesystem. If it fails (cross-FS,
    // perms), best-effort unlink the temp so we don't leave secrets behind.
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(unix)]
fn set_dir_perms_0700(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(p)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(p, perms)
}

#[cfg(not(unix))]
fn set_dir_perms_0700(_p: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parses_v3_format_with_comments_and_whitespace() {
        let text = "\
# subctl accounts (header)
# Format: alias|provider|email|config_dir|description

claude-jason    | claude        | jason@webdevtoday.com  | ~/.claude-jason          | Daily driver
openai-jason    | openai-codex  | jb@example.com          | /Users/sem/.codex-jason  | Codex personal
malformed-row-only-two-fields | claude
";
        let rows = parse_accounts_conf(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].alias, "claude-jason");
        assert_eq!(rows[0].provider, "claude");
        assert_eq!(rows[0].email, "jason@webdevtoday.com");
        // Tilde expansion happens if HOME is set.
        if let Some(h) = home_dir() {
            assert_eq!(rows[0].config_dir, h.join(".claude-jason"));
        }
        assert_eq!(rows[1].alias, "openai-jason");
        assert_eq!(rows[1].provider, "openai-codex");
        assert_eq!(rows[1].config_dir, PathBuf::from("/Users/sem/.codex-jason"));
        assert_eq!(rows[1].description, "Codex personal");
    }

    #[test]
    fn description_preserves_internal_pipes() {
        let text = "alias-x | claude | e@x.com | /tmp/x | first | second | third";
        let rows = parse_accounts_conf(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].description, "first | second | third");
    }

    #[test]
    fn write_then_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        let now = Utc::now();
        let rec = TokenRecord {
            access_token: "AT-123".into(),
            refresh_token: Some("RT-456".into()),
            last_refresh: now,
            expires_at: now + chrono::Duration::seconds(600),
        };
        write_token_file(&path, &rec).unwrap();

        // Permissions assert (Unix): exactly 0o600 (regular file mode bits).
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "auth.json must be 0600, got {mode:o}");

        let got = read_token_file(&path).unwrap().unwrap();
        assert_eq!(got.access_token, "AT-123");
        assert_eq!(got.refresh_token.as_deref(), Some("RT-456"));
        // Round-trip preserves expires_at via the `extra` slot.
        assert_eq!(got.expires_at.timestamp(), rec.expires_at.timestamp());
    }

    #[test]
    fn read_returns_none_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        assert!(read_token_file(&path).unwrap().is_none());
    }

    #[test]
    fn read_tolerates_v3_shape_without_expires_at() {
        // A v3-written auth.json has no `expires_at` field — we fall back
        // to `last_refresh`. This test pins that interop guarantee.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        let v3_payload = r#"{
            "OPENAI_API_KEY": null,
            "tokens": {
                "access_token": "v3-AT",
                "refresh_token": "v3-RT"
            },
            "last_refresh": "2026-01-15T10:00:00Z",
            "_subctl": { "alias": "openai-jason" }
        }"#;
        std::fs::write(&path, v3_payload).unwrap();
        let got = read_token_file(&path).unwrap().unwrap();
        assert_eq!(got.access_token, "v3-AT");
        assert_eq!(got.refresh_token.as_deref(), Some("v3-RT"));
        // expires_at fell back to last_refresh.
        assert_eq!(got.expires_at.to_rfc3339(), "2026-01-15T10:00:00+00:00");
    }

    #[tokio::test]
    async fn accounts_store_get_returns_none_for_unknown_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let conf = tmp.path().join("accounts.conf");
        std::fs::write(
            &conf,
            "openai-foo | openai-codex | a@b.com | /nonexistent | x",
        )
        .unwrap();
        let store = AccountsStore::open(&conf).unwrap();
        assert!(store.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn accounts_store_put_then_get_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let conf = tmp.path().join("accounts.conf");
        let cfg_dir = tmp.path().join("openai-tester");
        std::fs::write(
            &conf,
            format!(
                "openai-tester | openai-codex | t@x.com | {} | tester",
                cfg_dir.display()
            ),
        )
        .unwrap();
        let store = AccountsStore::open(&conf).unwrap();
        let now = Utc::now();
        let rec = AccountRecord {
            name: "openai-tester".into(),
            provider: "openai-codex".into(),
            access_token: "AT".into(),
            refresh_token: Some("RT".into()),
            expires_at: now + chrono::Duration::seconds(600),
        };
        store.put(rec.clone()).await.unwrap();
        let got = store.get("openai-tester").await.unwrap().unwrap();
        assert_eq!(got.access_token, "AT");
        assert_eq!(got.refresh_token.as_deref(), Some("RT"));
    }

    #[tokio::test]
    async fn accounts_store_put_fails_for_unknown_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let conf = tmp.path().join("accounts.conf");
        std::fs::write(&conf, "").unwrap();
        let store = AccountsStore::open(&conf).unwrap();
        let rec = AccountRecord {
            name: "ghost".into(),
            provider: "openai-codex".into(),
            access_token: "AT".into(),
            refresh_token: None,
            expires_at: Utc::now(),
        };
        assert!(store.put(rec).await.is_err());
    }
}
