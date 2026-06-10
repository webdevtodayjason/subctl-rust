//! v4-native chat attachments — port of v3's `components/evy/attachments.ts`
//! plus the `/attachments` routes in `components/evy/server.ts` (≈6300-6395),
//! surfaced by the dashboard as `/api/evy/attachments*` (oracle:
//! `http://127.0.0.1:8787`). The v4 BFF previously *deferred* this family
//! (`dashboard/V4_BRIDGE.md` line 40); this module closes that gap natively.
//!
//! ## Surface (v3-shape parity)
//! - `POST   /api/evy/attachments` (raw bytes + `X-Filename`/`X-Mime`/`X-Source`
//!   headers) → `201 { ok, attachment:{id,filename,size,mime,sha256,created_at} }`
//! - `GET    /api/evy/attachments`        → `{ ok, count, attachments:[…meta] }`
//! - `GET    /api/evy/attachments/{id}`   → raw bytes (`Content-Type` +
//!   `Content-Disposition: inline`); `404 { ok:false, error:"not found" }`
//! - `DELETE /api/evy/attachments/{id}`   → `{ ok:true }` / `404 { ok:false, error }`
//!
//! ## Storage layout (match v3)
//! `$SUBCTL_CONFIG_DIR/evy/attachments/` (default `~/.config/subctl`), with
//! one date-bucketed dir per day and a single `index.jsonl`:
//! ```text
//!   attachments/
//!   ├── 2026-05-10/<id>-<filename>
//!   └── index.jsonl   # {id,filename,sha256,size,mime,source,created_at,deleted_at,storage_path}
//! ```
//! Phase-1 limits are carried verbatim: a 5 MiB cap and a text-family mime
//! allowlist. Delete is a soft-delete in the index plus a hard `unlink` of the
//! on-disk file (reclaim space). Every read validates the id is bare hex and
//! the resolved file stays under the attachments root — traversal-proof even
//! against a hand-poisoned `index.jsonl`.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::Path as AxPath;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Digest;

/// Phase-1 size cap (5 MiB), matching v3's `MAX_BYTES`.
const MAX_BYTES: usize = 5 * 1024 * 1024;

/// Generous read ceiling for the request body — well above [`MAX_BYTES`] so the
/// handler can return v3's specific "too large" error rather than a transport
/// truncation for the common slightly-over case.
const BODY_READ_LIMIT: usize = 16 * 1024 * 1024;

/// Exact non-`text/*` mimes accepted in Phase 1 (matches v3 `TEXT_MIME_EXACT`).
const TEXT_MIME_EXACT: &[&str] = &[
    "application/json",
    "application/yaml",
    "application/x-yaml",
    "application/toml",
    "application/xml",
    "application/javascript",
    "application/typescript",
];

/// Where an attachment came from. Serializes lowercase to match v3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Operator file upload.
    Upload,
    /// Auto-converted from a large paste in the chat input.
    Paste,
    /// Produced by a master tool.
    Tool,
}

impl Source {
    fn parse(raw: &str) -> Self {
        match raw {
            "paste" => Source::Paste,
            "tool" => Source::Tool,
            _ => Source::Upload,
        }
    }
}

/// One `index.jsonl` record. `storage_path` + `deleted_at` are persisted but
/// never appear in the list/create HTTP responses (see [`AttachmentMeta`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// 16-hex-char id.
    pub id: String,
    /// Sanitized original filename.
    pub filename: String,
    /// SHA-256 of the stored bytes (hex).
    pub sha256: String,
    /// Byte length.
    pub size: u64,
    /// Resolved mime type.
    pub mime: String,
    /// Origin.
    pub source: Source,
    /// ISO-8601 create time.
    pub created_at: String,
    /// Soft-delete timestamp; `null` while live.
    pub deleted_at: Option<String>,
    /// Absolute on-disk path of the stored bytes.
    pub storage_path: String,
}

/// Public metadata row returned by `GET /api/evy/attachments` (v3 list shape —
/// no `sha256`, `storage_path`, or `deleted_at`).
#[derive(Debug, Serialize)]
struct AttachmentMeta {
    id: String,
    filename: String,
    size: u64,
    mime: String,
    source: Source,
    created_at: String,
}

/// A save failure carrying v3's `error` + optional `hint`.
#[derive(Debug)]
pub struct SaveError {
    error: String,
    hint: Option<String>,
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Resolve the attachments root: `$SUBCTL_CONFIG_DIR/evy/attachments`
/// (default `~/.config/subctl`).
fn attachments_root() -> PathBuf {
    let cfg =
        std::env::var("SUBCTL_CONFIG_DIR").unwrap_or_else(|_| format!("{}/.config/subctl", home()));
    PathBuf::from(cfg).join("evy").join("attachments")
}

fn index_path(root: &Path) -> PathBuf {
    root.join("index.jsonl")
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn today_dir() -> String {
    Utc::now().format("%Y-%m-%d").to_string()
}

/// 16 lowercase-hex chars (matches v3's `randomBytes(8).toString("hex")` shape
/// and the `[a-f0-9]+` route regex). Derived from a UUID v4 — no extra dep.
fn make_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// Strip path separators + control chars; cap at 200 chars (v3
/// `sanitizeFilename`).
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    cleaned.chars().take(200).collect()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode a header value the way the browser-side `encodeURIComponent` expects
/// (`decodeURIComponent`): percent-escapes → bytes → UTF-8. Returns `None` on a
/// malformed escape or non-UTF-8 result, so the caller can fall back to the raw
/// header exactly like v3's `try { decodeURIComponent } catch`.
fn decode_uri_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_val(bytes[i + 1])?;
            let lo = hex_val(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `true` if the id is a bare lowercase-hex string (route-regex parity +
/// traversal guard — no `/`, `.`, `..` can appear).
fn is_hex_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_allowed_mime(mime: &str) -> bool {
    TEXT_MIME_EXACT.contains(&mime) || mime.starts_with("text/")
}

/// Infer a mime from the filename extension (v3 `inferMime`); `fallback` wins
/// when provided (the `X-Mime` header).
fn infer_mime(filename: &str, fallback: Option<&str>) -> String {
    if let Some(f) = fallback {
        return f.to_string();
    }
    let lower = filename.to_lowercase();
    let ext_is = |suffixes: &[&str]| suffixes.iter().any(|s| lower.ends_with(s));
    if ext_is(&[".md", ".markdown"]) {
        "text/markdown"
    } else if ext_is(&[".txt", ".log"]) {
        "text/plain"
    } else if ext_is(&[".json"]) {
        "application/json"
    } else if ext_is(&[".yaml", ".yml"]) {
        "application/yaml"
    } else if ext_is(&[".toml"]) {
        "application/toml"
    } else if ext_is(&[".xml"]) {
        "application/xml"
    } else if ext_is(&[".js", ".mjs"]) {
        "application/javascript"
    } else if ext_is(&[".ts", ".tsx"]) {
        "application/typescript"
    } else if ext_is(&[".html", ".htm"]) {
        "text/html"
    } else if ext_is(&[".css"]) {
        "text/css"
    } else if ext_is(&[".sh", ".bash", ".zsh"]) {
        "text/x-shellscript"
    } else if ext_is(&[".py"]) {
        "text/x-python"
    } else {
        "text/plain"
    }
    .to_string()
}

fn read_index(root: &Path) -> Vec<Attachment> {
    let Ok(raw) = std::fs::read_to_string(index_path(root)) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Attachment>(l).ok())
        .collect()
}

fn append_index(root: &Path, entry: &Attachment) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(root)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_path(root))?;
    writeln!(f, "{}", serde_json::to_string(entry).unwrap_or_default())
}

fn rewrite_index(root: &Path, entries: &[Attachment]) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let body: String = entries
        .iter()
        .map(|e| serde_json::to_string(e).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let body = if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    };
    std::fs::write(index_path(root), body)
}

/// Live (non-deleted) attachments, unsorted.
fn do_list(root: &Path) -> Vec<Attachment> {
    read_index(root)
        .into_iter()
        .filter(|e| e.deleted_at.is_none())
        .collect()
}

/// Look up a live attachment by id.
fn do_get(root: &Path, id: &str) -> Option<Attachment> {
    read_index(root)
        .into_iter()
        .find(|e| e.id == id && e.deleted_at.is_none())
}

/// Persist `body` as a new attachment. Mirrors v3 `saveAttachment`: empty/oversize/
/// disallowed-mime are rejected before any write.
fn do_save(
    root: &Path,
    body: &[u8],
    raw_filename: &str,
    mime_hint: Option<&str>,
    source: Source,
) -> Result<Attachment, SaveError> {
    if body.is_empty() {
        return Err(SaveError {
            error: "empty body".into(),
            hint: None,
        });
    }
    if body.len() > MAX_BYTES {
        return Err(SaveError {
            error: format!("attachment too large: {} bytes (cap {MAX_BYTES})", body.len()),
            hint: Some(
                "Phase 1 cap is 5 MiB. For larger documents use the vault directly via vault_append, or split into parts."
                    .into(),
            ),
        });
    }
    let filename = sanitize_filename(if raw_filename.is_empty() {
        "untitled.txt"
    } else {
        raw_filename
    });
    let mime = infer_mime(&filename, mime_hint);
    if !is_allowed_mime(&mime) {
        return Err(SaveError {
            error: format!("mime type not allowed: {mime}"),
            hint: Some(
                "Phase 1 accepts text/* + JSON/YAML/TOML/XML. PDF + image support deferred to Phase 2 (vision-capable supervisor required)."
                    .into(),
            ),
        });
    }

    let date_dir = today_dir();
    let dir = root.join(&date_dir);
    std::fs::create_dir_all(&dir).map_err(|e| SaveError {
        error: format!("mkdir: {e}"),
        hint: None,
    })?;

    let id = make_id();
    let storage_path = dir.join(format!("{id}-{filename}"));
    std::fs::write(&storage_path, body).map_err(|e| SaveError {
        error: format!("write: {e}"),
        hint: None,
    })?;
    let sha256 = hex::encode(sha2::Sha256::digest(body));
    let entry = Attachment {
        id,
        filename,
        sha256,
        size: body.len() as u64,
        mime,
        source,
        created_at: now_iso(),
        deleted_at: None,
        storage_path: storage_path.to_string_lossy().into_owned(),
    };
    append_index(root, &entry).map_err(|e| SaveError {
        error: format!("index: {e}"),
        hint: None,
    })?;
    Ok(entry)
}

/// Soft-delete in the index + hard-`unlink` the file. `Ok(())` when removed,
/// `Err(msg)` (→ 404) when no live attachment with that id exists.
fn do_delete(root: &Path, id: &str) -> Result<(), String> {
    let mut all = read_index(root);
    let Some(idx) = all
        .iter()
        .position(|e| e.id == id && e.deleted_at.is_none())
    else {
        return Err(format!("no attachment with id={id}"));
    };
    all[idx].deleted_at = Some(now_iso());
    let _ = std::fs::remove_file(&all[idx].storage_path); // best-effort, may be gone
    rewrite_index(root, &all).map_err(|e| format!("rewrite index: {e}"))?;
    Ok(())
}

// ── HTTP handlers ────────────────────────────────────────────────────────────

/// `POST /api/evy/attachments` — body is raw bytes; metadata rides in headers
/// (`X-Filename` url-encoded, `X-Mime` optional, `X-Source`). 201 on success.
pub(crate) async fn upload_handler(headers: HeaderMap, body: Body) -> Response {
    let raw_filename = headers
        .get("X-Filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("untitled.txt");
    // Browser sends X-Filename url-encoded; fall back to raw on malformed.
    let filename = decode_uri_component(raw_filename).unwrap_or_else(|| raw_filename.to_string());
    let mime_hint = headers.get("X-Mime").and_then(|v| v.to_str().ok());
    let source = Source::parse(
        headers
            .get("X-Source")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("upload"),
    );

    let bytes = match axum::body::to_bytes(body, BODY_READ_LIMIT).await {
        Ok(b) => b,
        Err(_) => {
            // Body exceeded the read ceiling — surface v3's too-large error.
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": format!("attachment too large: >{BODY_READ_LIMIT} bytes (cap {MAX_BYTES})"),
                    "hint": "Phase 1 cap is 5 MiB. For larger documents use the vault directly via vault_append, or split into parts.",
                })),
            )
                .into_response();
        }
    };

    match do_save(&attachments_root(), &bytes, &filename, mime_hint, source) {
        Ok(a) => (
            StatusCode::CREATED,
            Json(json!({
                "ok": true,
                "attachment": {
                    "id": a.id,
                    "filename": a.filename,
                    "size": a.size,
                    "mime": a.mime,
                    "sha256": a.sha256,
                    "created_at": a.created_at,
                }
            })),
        )
            .into_response(),
        Err(e) => {
            let mut obj = serde_json::Map::new();
            obj.insert("ok".into(), json!(false));
            obj.insert("error".into(), json!(e.error));
            if let Some(hint) = e.hint {
                obj.insert("hint".into(), json!(hint));
            }
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::Value::Object(obj)),
            )
                .into_response()
        }
    }
}

/// `GET /api/evy/attachments` → `{ ok, count, attachments:[…meta] }`, newest first.
pub(crate) async fn list_handler() -> Response {
    let mut all = do_list(&attachments_root());
    // Sort newest-first by created_at (v3 `Date.parse(b) - Date.parse(a)`).
    all.sort_by_key(|a| std::cmp::Reverse(created_ms(&a.created_at)));
    let metas: Vec<AttachmentMeta> = all
        .into_iter()
        .map(|a| AttachmentMeta {
            id: a.id,
            filename: a.filename,
            size: a.size,
            mime: a.mime,
            source: a.source,
            created_at: a.created_at,
        })
        .collect();
    Json(json!({ "ok": true, "count": metas.len(), "attachments": metas })).into_response()
}

fn created_ms(iso: &str) -> i64 {
    DateTime::parse_from_rfc3339(iso)
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

/// `GET /api/evy/attachments/{id}` — stream the stored bytes back with the
/// recorded mime + an inline `Content-Disposition`. 404 when unknown; 500 on a
/// read failure (v3 parity).
pub(crate) async fn serve_handler(AxPath(id): AxPath<String>) -> Response {
    let root = attachments_root();
    if !is_hex_id(&id) {
        return not_found();
    }
    let Some(att) = do_get(&root, &id) else {
        return not_found();
    };
    // Defense-in-depth: even a hand-poisoned index can't escape the root.
    let path = PathBuf::from(&att.storage_path);
    if !path.starts_with(&root) {
        return not_found();
    }
    match std::fs::read(&path) {
        Ok(buf) => {
            let mut resp = Response::new(Body::from(buf));
            let headers = resp.headers_mut();
            if let Ok(ct) = att.mime.parse() {
                headers.insert(axum::http::header::CONTENT_TYPE, ct);
            }
            if let Ok(cd) = format!("inline; filename=\"{}\"", att.filename).parse() {
                headers.insert(axum::http::header::CONTENT_DISPOSITION, cd);
            }
            resp
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `DELETE /api/evy/attachments/{id}` → `{ ok:true }` / `404 { ok:false, error }`.
pub(crate) async fn delete_handler(AxPath(id): AxPath<String>) -> Response {
    if !is_hex_id(&id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no attachment with id={id}") })),
        )
            .into_response();
    }
    match do_delete(&attachments_root(), &id) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": error })),
        )
            .into_response(),
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "ok": false, "error": "not found" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "evy-attach-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn save_then_list_and_get_round_trip() {
        let root = tmpdir();
        let a = do_save(&root, b"# hello\n", "notes.md", None, Source::Upload).unwrap();
        assert_eq!(a.mime, "text/markdown");
        assert_eq!(a.size, 8);
        assert_eq!(a.id.len(), 16);
        assert!(a.id.bytes().all(|b| b.is_ascii_hexdigit()));
        // sha256 of "# hello\n"
        assert_eq!(a.sha256, hex::encode(sha2::Sha256::digest(b"# hello\n")));

        let list = do_list(&root);
        assert_eq!(list.len(), 1);
        let got = do_get(&root, &a.id).unwrap();
        assert_eq!(got.filename, "notes.md");
        // Stored bytes are readable at the recorded path.
        assert_eq!(std::fs::read(&got.storage_path).unwrap(), b"# hello\n");
    }

    #[test]
    fn rejects_empty_oversize_and_bad_mime() {
        let root = tmpdir();
        assert_eq!(
            do_save(&root, b"", "x.txt", None, Source::Upload)
                .unwrap_err()
                .error,
            "empty body"
        );

        let big = vec![b'a'; MAX_BYTES + 1];
        let err = do_save(&root, &big, "big.txt", None, Source::Upload).unwrap_err();
        assert!(err.error.starts_with("attachment too large"));
        assert!(err.hint.is_some());

        // .png infers nothing in the text map → text/plain, BUT an explicit
        // disallowed X-Mime is rejected.
        let err = do_save(
            &root,
            b"\x89PNG",
            "logo.png",
            Some("image/png"),
            Source::Upload,
        )
        .unwrap_err();
        assert_eq!(err.error, "mime type not allowed: image/png");
        assert!(err.hint.is_some());
    }

    #[test]
    fn delete_soft_deletes_and_404s_after() {
        let root = tmpdir();
        let a = do_save(&root, b"bye", "f.txt", None, Source::Upload).unwrap();
        assert!(std::path::Path::new(&a.storage_path).exists());
        assert!(do_delete(&root, &a.id).is_ok());
        // File unlinked, index hides it, re-delete 404s.
        assert!(!std::path::Path::new(&a.storage_path).exists());
        assert!(do_get(&root, &a.id).is_none());
        assert!(do_delete(&root, &a.id).is_err());
        assert!(do_list(&root).is_empty());
    }

    #[test]
    fn mime_inference_and_x_mime_override() {
        assert_eq!(infer_mime("a.json", None), "application/json");
        assert_eq!(infer_mime("a.yml", None), "application/yaml");
        assert_eq!(infer_mime("a.unknown", None), "text/plain");
        assert_eq!(infer_mime("a.bin", Some("text/csv")), "text/csv");
    }

    #[test]
    fn sanitize_strips_separators_and_controls() {
        assert_eq!(sanitize_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_filename("a\nb\tc"), "a_b_c");
        assert_eq!(sanitize_filename(&"x".repeat(300)).chars().count(), 200);
    }

    #[test]
    fn hex_id_guard_rejects_traversal() {
        assert!(is_hex_id("9fe0631f90aa40e0"));
        assert!(!is_hex_id("../secrets"));
        assert!(!is_hex_id("abc.def"));
        assert!(!is_hex_id(""));
    }

    #[test]
    fn decode_uri_component_handles_utf8_and_malformed() {
        assert_eq!(
            decode_uri_component("foo%20bar.txt").unwrap(),
            "foo bar.txt"
        );
        // %E2%80%A6 = "…"
        assert_eq!(decode_uri_component("a%E2%80%A6.md").unwrap(), "a….md");
        // Malformed escape → None (caller falls back to raw).
        assert!(decode_uri_component("bad%2").is_none());
        assert!(decode_uri_component("bad%zz").is_none());
    }
}
