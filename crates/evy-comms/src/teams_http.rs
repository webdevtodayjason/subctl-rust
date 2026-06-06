//! Cutover Phase 2 slice 2m — team-template CRUD (native).
//!
//! Ports the v3 `/api/teams` JSON-template CRUD (`dashboard/server.ts:3500-3600`):
//! templates are `*.json` files in `~/.config/subctl/evy/team-templates/`
//! (`SUBCTL_TEAM_TEMPLATES_DIR` override). Templates are opaque JSON (the
//! dashboard owns the schema); v4 just stores/serves them. `/api/evy/teams/tools`
//! is routed to the reverse-proxy separately so it doesn't collide with `{name}`.
//!
//! Deviation from v3: `PUT /{name}` is **upsert** (create-or-replace) rather than
//! update-only-404 — simpler and matches the natural REST expectation.
//!
//! The handlers are thin wrappers over dir-parameterized core fns (`do_*`) so the
//! logic is unit-testable against a temp dir without touching process-global env.

use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Directory holding the `*.json` team templates.
fn teams_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SUBCTL_TEAM_TEMPLATES_DIR") {
        return PathBuf::from(d);
    }
    let cfg = std::env::var("SUBCTL_CONFIG_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.config/subctl")
    });
    PathBuf::from(cfg).join("evy").join("team-templates")
}

/// A safe template name: a single path segment of `[A-Za-z0-9._-]`, not `.`/`..`.
/// Rejects traversal (`/`, `..`) before the name ever touches a file path.
fn safe_name(name: &str) -> bool {
    !(name.is_empty() || name == "." || name == "..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

type CrudResult = std::result::Result<Value, (StatusCode, String)>;

fn do_list(dir: &Path) -> Value {
    let mut teams: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mut val) = serde_json::from_str::<Value>(&text) else {
                continue; // skip bad JSON, like v3
            };
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if let Some(obj) = val.as_object_mut() {
                obj.entry("name").or_insert(json!(stem));
            }
            teams.push(val);
        }
    }
    teams.sort_by(|a, b| {
        a.get("name").and_then(Value::as_str).unwrap_or("")
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
    json!({ "ok": true, "teams_dir": dir.to_string_lossy(), "teams": teams })
}

fn do_get(dir: &Path, name: &str) -> CrudResult {
    if !safe_name(name) {
        return Err((StatusCode::BAD_REQUEST, "invalid template name".into()));
    }
    let file = dir.join(format!("{name}.json"));
    match std::fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map(|template| json!({ "ok": true, "name": name, "file": file.to_string_lossy(), "template": template }))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("corrupt template: {e}"))),
        Err(_) => Err((StatusCode::NOT_FOUND, "template not found".into())),
    }
}

fn do_put(dir: &Path, name: &str, body: &Value) -> CrudResult {
    if !safe_name(name) {
        return Err((StatusCode::BAD_REQUEST, "invalid template name".into()));
    }
    std::fs::create_dir_all(dir).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir: {e}")))?;
    let file = dir.join(format!("{name}.json"));
    let pretty = serde_json::to_string_pretty(body).unwrap_or_else(|_| body.to_string());
    std::fs::write(&file, pretty)
        .map(|()| json!({ "ok": true, "name": name, "file": file.to_string_lossy() }))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")))
}

fn do_delete(dir: &Path, name: &str) -> CrudResult {
    if !safe_name(name) {
        return Err((StatusCode::BAD_REQUEST, "invalid template name".into()));
    }
    let file = dir.join(format!("{name}.json"));
    if !file.exists() {
        return Err((StatusCode::NOT_FOUND, "template not found".into()));
    }
    std::fs::remove_file(&file)
        .map(|()| json!({ "ok": true, "deleted": name }))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("delete: {e}")))
}

fn resp(r: CrudResult) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err((status, msg)) => (status, Json(json!({ "ok": false, "error": msg }))).into_response(),
    }
}

/// `GET /api/evy/teams` → `{ ok, teams_dir, teams:[{name, …template}] }`.
pub(crate) async fn teams_list_handler() -> Response {
    Json(do_list(&teams_dir())).into_response()
}

/// `GET /api/evy/teams/{name}` → `{ ok, name, file, template }`.
pub(crate) async fn team_get_handler(AxPath(name): AxPath<String>) -> Response {
    resp(do_get(&teams_dir(), &name))
}

/// `PUT /api/evy/teams/{name}` — upsert the template (pretty-printed JSON).
pub(crate) async fn team_put_handler(AxPath(name): AxPath<String>, Json(body): Json<Value>) -> Response {
    resp(do_put(&teams_dir(), &name, &body))
}

/// `DELETE /api/evy/teams/{name}` → `{ ok, deleted }`.
pub(crate) async fn team_delete_handler(AxPath(name): AxPath<String>) -> Response {
    resp(do_delete(&teams_dir(), &name))
}

/// `POST /api/evy/teams` — create from a body carrying a `name` field.
pub(crate) async fn team_post_handler(Json(body): Json<Value>) -> Response {
    match body.get("name").and_then(Value::as_str) {
        Some(name) => resp(do_put(&teams_dir(), name, &body)),
        None => resp(Err((StatusCode::BAD_REQUEST, "body must include a string `name`".into()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "evy-teams-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = tmpdir();
        let tmpl = json!({ "description": "rust crew", "agents": 3 });
        assert!(do_put(&dir, "foo", &tmpl).is_ok());
        let got = do_get(&dir, "foo").unwrap();
        assert_eq!(got["template"]["agents"], json!(3));
        assert_eq!(got["name"], json!("foo"));
    }

    #[test]
    fn list_includes_put_template_with_name() {
        let dir = tmpdir();
        do_put(&dir, "alpha", &json!({ "x": 1 })).unwrap();
        let list = do_list(&dir);
        let names: Vec<&str> = list["teams"].as_array().unwrap().iter()
            .filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, vec!["alpha"]);
    }

    #[test]
    fn delete_removes_and_404s_after() {
        let dir = tmpdir();
        do_put(&dir, "gone", &json!({})).unwrap();
        assert!(do_delete(&dir, "gone").is_ok());
        let (status, _) = do_get(&dir, "gone").unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = tmpdir();
        assert_eq!(do_get(&dir, "../../etc/passwd").unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(do_put(&dir, "a/b", &json!({})).unwrap_err().0, StatusCode::BAD_REQUEST);
    }
}
