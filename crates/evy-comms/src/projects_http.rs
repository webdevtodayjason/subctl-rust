//! Cutover — native projects CRUD + policy-preset surface.
//!
//! Ports the v3 Bun dashboard's project + policy-preset endpoints to v4-native
//! Rust. The v3 originals live in `dashboard/server.ts` (the `/api/projects*`
//! family) and `dashboard/lib/policy-api.ts` (`handleListPresets` /
//! `handleApplyPreset`). Each endpoint returns the **exact v3 wire shape**.
//!
//! ## Endpoints (registered in `http.rs` before the `/api/{*rest}` catch-all)
//!
//! | Method | v4 canonical | v3 path (also served) | Shape source |
//! |--------|--------------|-----------------------|--------------|
//! | GET  | `/api/evy/projects`            | `/api/projects`            | `server.ts:3170` |
//! | POST | `/api/evy/projects/create`     | `/api/projects/create`     | `server.ts:4394` |
//! | GET  | `/api/evy/projects/{name}`     | `/api/projects/{name}`     | `server.ts:4610` |
//! | GET  | `/api/evy/policy/presets`      | `/api/policy/presets`      | `policy-api.ts:382` |
//! | POST | `/api/evy/policy/preset/{path}`| `/api/policy/preset/{path}`| `policy-api.ts:338` |
//!
//! Both prefixes resolve to the same handlers (mirrors the `/api/state` +
//! `/api/evy/accounts` precedent in `accounts_http.rs`): v4 serves the bare v3
//! path natively so a dashboard proxy hit lands on Rust, and the `/api/evy/*`
//! alias matches the daemon's canonical convention.
//!
//! ## Deliberate deviations from v3 (parity is shape, not host side effects)
//!
//! * `GET …/{name}` → `dev_teams` is always `[]`. v3 derives it from
//!   `buildOrchestrations()` (tmux session scanning); `V4_BRIDGE.md` keeps
//!   orchestration/teams on the v3 master (`/api/master/teams`), so the v4
//!   daemon does not own that scan.
//! * `POST …/create` omits v3's `launchctl unload/load` master restart. The v4
//!   daemon must not bounce the v3 master; everything else (clone/mkdir, vault
//!   seed, `policy.json` append) is faithful.
//!
//! Like `teams_http`, the logic lives in dir/path-parameterised core fns so it
//! is unit-testable against temp dirs without touching process-global env; the
//! thin handler wrappers resolve roots from env (`SUBCTL_CODE_ROOT`,
//! `SUBCTL_CONFIG_DIR`, `HOME`, `SUBCTL_INSTALL_ROOT`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

/// Filesystem roots the project handlers read/write. Resolved from env in
/// [`Roots::from_env`]; injected directly in unit tests so logic never depends
/// on process-global state.
struct Roots {
    /// Project scan root (`SUBCTL_CODE_ROOT`, default `$HOME/code`).
    code_root: PathBuf,
    /// Evy state dir (`<SUBCTL_CONFIG_DIR or $HOME/.config/subctl>/evy`) —
    /// holds `policy.json` and `decisions.jsonl`.
    config_evy: PathBuf,
    /// Obsidian vault root (`$HOME/Documents/Obsidian Vault`).
    vault_root: PathBuf,
    /// Home dir (`$HOME`) — used for `~` expansion + the project-path guard.
    home: PathBuf,
}

impl Roots {
    /// Resolve all roots from process env, mirroring the v3 dashboard.
    fn from_env() -> Self {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let code_root = std::env::var("SUBCTL_CODE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join("code"));
        let config_evy = std::env::var("SUBCTL_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config").join("subctl"))
            .join("evy");
        let vault_root = home.join("Documents").join("Obsidian Vault");
        Self {
            code_root,
            config_evy,
            vault_root,
            home,
        }
    }
}

// ---------------------------------------------------------------------------
// Subprocess helpers (git / gh) — bounded by a timeout per the conventions.
// ---------------------------------------------------------------------------

/// Captured result of a finished subprocess.
struct CmdOut {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run `program args` (optionally in `cwd`), bounded by `dur`. Returns `None`
/// on spawn failure or timeout — callers treat that as "command unavailable",
/// exactly like the v3 `spawnSync` try/catch.
async fn run(program: &str, args: &[&str], cwd: Option<&Path>, dur: Duration) -> Option<CmdOut> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).stdin(std::process::Stdio::null());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    match tokio::time::timeout(dur, cmd.output()).await {
        Ok(Ok(o)) => Some(CmdOut {
            success: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
        _ => None,
    }
}

/// `git -C dir <args>` returning trimmed stdout on exit 0, else `None`.
/// Mirrors v3's `gitOut` (which returns `""` on failure — callers map `None`
/// to the empty/`null` case the same way).
async fn git_capture(dir: &Path, args: &[&str], dur: Duration) -> Option<String> {
    let dir_s = dir.to_str()?;
    let mut full: Vec<&str> = vec!["-C", dir_s];
    full.extend_from_slice(args);
    let out = run("git", &full, None, dur).await?;
    out.success.then(|| out.stdout.trim().to_string())
}

// ---------------------------------------------------------------------------
// policy.json (lenient JSON5-ish reader, matching the v3 strip pipeline)
// ---------------------------------------------------------------------------

/// Drop `"_comment…":` lines then trailing commas, then parse. Mirrors the v3
/// `raw.split("\n").filter(…).join("\n").replace(/,(\s*[}\]])/g,"$1")` pipeline
/// so `policy.json` (which carries `_comment_*` keys) parses cleanly.
fn parse_policy_json_lenient(text: &str) -> Option<Value> {
    let stripped: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("\"_comment"))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&strip_trailing_commas(&stripped)).ok()
}

/// Remove a comma that is immediately followed (modulo whitespace) by `}` or
/// `]`. String-aware so commas inside string literals are preserved.
fn strip_trailing_commas(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut escaped = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            ',' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                    i += 1;
                    continue; // drop the comma
                }
                out.push(c);
            }
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

/// Read `policy.json` and return its parsed root object, or `None`.
fn read_policy_doc(roots: &Roots) -> Option<Value> {
    let path = roots.config_evy.join("policy.json");
    let text = std::fs::read_to_string(path).ok()?;
    parse_policy_json_lenient(&text)
}

/// Build a `{ expanded_path -> autonomy_level }` map from `policy.json`,
/// expanding a leading `~` against `home` (matching v3).
fn policy_autonomy_map(roots: &Roots) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(doc) = read_policy_doc(roots) else {
        return map;
    };
    let Some(projects) = doc.get("projects").and_then(Value::as_array) else {
        return map;
    };
    let home = roots.home.to_string_lossy();
    for p in projects {
        let Some(raw) = p.get("path").and_then(Value::as_str) else {
            continue;
        };
        let expanded = if let Some(rest) = raw.strip_prefix('~') {
            format!("{home}{rest}")
        } else {
            raw.to_string()
        };
        let autonomy = p
            .get("autonomy_level")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        map.insert(expanded, autonomy);
    }
    map
}

// ---------------------------------------------------------------------------
// GET /api/evy/projects — scan code_root, mark policy membership.
// ---------------------------------------------------------------------------

/// Build one project's summary row for the list endpoint.
async fn project_summary(path: &Path, autonomy: &HashMap<String, String>) -> Value {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let last_commit = git_capture(
        path,
        &["log", "-1", "--format=%h %s (%cr)"],
        Duration::from_millis(1500),
    )
    .await;
    let branch = git_capture(
        path,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        Duration::from_millis(1000),
    )
    .await;
    let has = |rel: &str| path.join(rel).exists();
    let path_s = path.to_string_lossy().to_string();
    json!({
        "name": name,
        "path": path_s,
        "branch": branch,
        "last_commit": last_commit,
        "has_claude_md": has("CLAUDE.md"),
        "has_package_json": has("package.json"),
        "has_pyproject": has("pyproject.toml") || has("requirements.txt"),
        "has_readme": has("README.md") || has("README"),
        "in_policy": autonomy.contains_key(&path_s),
        "autonomy_level": autonomy.get(&path_s).cloned(),
    })
}

/// Core of `GET /api/evy/projects`.
async fn do_list(roots: &Roots) -> Value {
    let autonomy = policy_autonomy_map(roots);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&roots.code_root) {
        for entry in rd.flatten() {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let hidden = entry
                .file_name()
                .to_str()
                .map(|s| s.starts_with('.'))
                .unwrap_or(true);
            if is_dir && !hidden {
                dirs.push(entry.path());
            }
        }
    }
    let autonomy_ref = &autonomy;
    let futs = dirs.iter().map(|d| project_summary(d, autonomy_ref));
    let projects: Vec<Value> = futures::future::join_all(futs).await;
    json!({
        "ok": true,
        "code_root": roots.code_root.to_string_lossy(),
        "projects": projects,
    })
}

// ---------------------------------------------------------------------------
// GET /api/evy/projects/{name} — drill-down detail for one project.
// ---------------------------------------------------------------------------

/// Parse `decisions.jsonl` (last 200 lines), keep entries whose `project`
/// equals `name` or `path`, newest-first, capped at 20 — matching v3.
fn project_decisions(roots: &Roots, name: &str, path_s: &str) -> Vec<Value> {
    let dec_path = roots.config_evy.join("decisions.jsonl");
    let Ok(text) = std::fs::read_to_string(dec_path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    let tail = if lines.len() > 200 {
        &lines[lines.len() - 200..]
    } else {
        &lines[..]
    };
    let mut out: Vec<Value> = Vec::new();
    for line in tail {
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let proj = d.get("project").and_then(Value::as_str);
        if proj == Some(name) || proj == Some(path_s) {
            out.push(d);
        }
    }
    out.reverse();
    out.truncate(20);
    out
}

/// Extract `owner/repo` from a GitHub remote URL, or `None`. Mirrors the v3
/// regex `github\.com[:/]([^/]+)/([^/.]+)(\.git)?`.
fn extract_gh_repo(url: &str) -> Option<String> {
    let idx = url.find("github.com")?;
    let rest = &url[idx + "github.com".len()..];
    let rest = rest.strip_prefix(':').or_else(|| rest.strip_prefix('/'))?;
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next().filter(|o| !o.is_empty())?;
    let repo_raw = parts.next()?;
    // repo stops at the first `/`, `.` (so `.git` and any suffix drop off).
    let repo = repo_raw
        .split(['/', '.'])
        .next()
        .filter(|r| !r.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// Best-effort `gh <args> --json …` returning a parsed JSON array, else `[]`.
async fn gh_json_list(args: &[&str]) -> Value {
    match run("gh", args, None, Duration::from_millis(4000)).await {
        Some(out) if out.success => serde_json::from_str(&out.stdout).unwrap_or_else(|_| json!([])),
        _ => json!([]),
    }
}

/// Core of `GET /api/evy/projects/{name}`. `Err` carries `(status, body)`.
async fn do_detail(roots: &Roots, name: &str) -> Result<Value, (StatusCode, Value)> {
    let path = roots.code_root.join(name);
    if !path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": "project not found" }),
        ));
    }
    let d150 = Duration::from_millis(1500);
    let branch = git_capture(&path, &["rev-parse", "--abbrev-ref", "HEAD"], d150).await;
    let last_commit = git_capture(
        &path,
        &["log", "-1", "--format=%h%x09%s%x09%cr%x09%an"],
        d150,
    )
    .await;
    let remote_url = git_capture(&path, &["config", "--get", "remote.origin.url"], d150).await;
    let dirty = git_capture(&path, &["status", "--porcelain"], d150)
        .await
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let ab = git_capture(
        &path,
        &["rev-list", "--left-right", "--count", "HEAD...@{u}"],
        d150,
    )
    .await
    .unwrap_or_default();
    let mut ab_parts = ab.split('\t');
    let ahead: i64 = ab_parts
        .next()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let behind: i64 = ab_parts
        .next()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    let recent_commits: Vec<Value> = git_capture(
        &path,
        &["log", "-10", "--format=%h%x09%s%x09%cr%x09%an"],
        d150,
    )
    .await
    .unwrap_or_default()
    .lines()
    .filter(|l| !l.is_empty())
    .map(|line| {
        let mut f = line.split('\t');
        json!({
            "sha": f.next(),
            "subject": f.next(),
            "when": f.next(),
            "author": f.next(),
        })
    })
    .collect();

    let has = |rel: &str| path.join(rel).exists();
    let path_s = path.to_string_lossy().to_string();

    // Policy entry (full object, like v3) keyed by expanded path.
    let policy_entry: Option<Value> = read_policy_doc(roots)
        .and_then(|doc| doc.get("projects").and_then(Value::as_array).cloned())
        .and_then(|projects| {
            let home = roots.home.to_string_lossy();
            projects.into_iter().find(|p| {
                p.get("path").and_then(Value::as_str).is_some_and(|raw| {
                    let expanded = raw
                        .strip_prefix('~')
                        .map(|rest| format!("{home}{rest}"))
                        .unwrap_or_else(|| raw.to_string());
                    expanded == path_s
                })
            })
        });

    let decisions = project_decisions(roots, name, &path_s);

    let vault_project_dir = roots.vault_root.join(name);
    let vault_exists = vault_project_dir.exists();

    let gh_repo = remote_url.as_deref().and_then(extract_gh_repo);
    let (prs, issues) = if let Some(repo) = gh_repo.as_deref() {
        let prs = gh_json_list(&[
            "pr",
            "list",
            "--repo",
            repo,
            "--state",
            "open",
            "--limit",
            "10",
            "--json",
            "number,title,state,isDraft,headRefName,statusCheckRollup,url,updatedAt",
        ])
        .await;
        let issues = gh_json_list(&[
            "issue",
            "list",
            "--repo",
            repo,
            "--state",
            "open",
            "--limit",
            "10",
            "--json",
            "number,title,state,labels,url,updatedAt",
        ])
        .await;
        (prs, issues)
    } else {
        (json!([]), json!([]))
    };

    Ok(json!({
        "ok": true,
        "name": name,
        "path": path_s,
        "remote_url": remote_url,
        "github_repo": gh_repo,
        "branch": branch,
        "last_commit": last_commit,
        "dirty": dirty,
        "ahead": ahead,
        "behind": behind,
        "recent_commits": recent_commits,
        "flags": {
            "has_claude_md": has("CLAUDE.md"),
            "has_package_json": has("package.json"),
            "has_pyproject": has("pyproject.toml") || has("requirements.txt"),
            "has_readme": has("README.md") || has("README"),
        },
        "in_policy": policy_entry.is_some(),
        "policy": policy_entry,
        "decisions": decisions,
        // Deviation: orchestration/tmux scanning stays on the v3 master
        // (V4_BRIDGE.md → /api/master/teams). v4 reports no dev teams.
        "dev_teams": [],
        "vault": {
            "project_dir": vault_project_dir.to_string_lossy(),
            "exists": vault_exists,
            "root": roots.vault_root.to_string_lossy(),
        },
        "prs": prs,
        "issues": issues,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/evy/projects/create — wizard endpoint.
// ---------------------------------------------------------------------------

/// Normalise a raw project name: trim, collapse whitespace runs to `-`, strip
/// chars outside `[A-Za-z0-9._-]`, trim leading/trailing dashes. (v3 parity.)
fn normalize_name(raw: &str) -> String {
    let dashed: String = {
        // collapse any run of whitespace into a single '-'
        let mut s = String::with_capacity(raw.trim().len());
        let mut prev_ws = false;
        for c in raw.trim().chars() {
            if c.is_whitespace() {
                if !prev_ws {
                    s.push('-');
                }
                prev_ws = true;
            } else {
                s.push(c);
                prev_ws = false;
            }
        }
        s
    };
    let kept: String = dashed
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
        .collect();
    kept.trim_matches('-').to_string()
}

/// Normalise a pasted git URL into something `git clone` accepts (v3 parity).
fn normalize_git_url(raw: &str) -> String {
    let mut g = raw.trim().to_string();
    if let Some(rest) = g.strip_prefix("gh repo clone ") {
        g = rest.trim().to_string();
    }
    // `owner/repo` shorthand → https URL.
    let is_shorthand = {
        let mut segs = g.splitn(2, '/');
        match (segs.next(), segs.next()) {
            (Some(a), Some(b)) => {
                !a.is_empty()
                    && !b.is_empty()
                    && !b.contains('/')
                    && a.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
                    && b.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            }
            _ => false,
        }
    };
    if is_shorthand {
        g = format!("https://github.com/{g}.git");
    }
    g
}

/// Core of `POST /api/evy/projects/create`. `Err` carries `(status, body)`.
async fn do_create(roots: &Roots, body: &Value) -> Result<Value, (StatusCode, Value)> {
    let name = normalize_name(body.get("name").and_then(Value::as_str).unwrap_or(""));
    let git_url = normalize_git_url(body.get("git_url").and_then(Value::as_str).unwrap_or(""));
    let autonomy = body
        .get("autonomy_level")
        .and_then(Value::as_str)
        .unwrap_or("ask")
        .to_string();
    let create_vault = body.get("create_vault").and_then(Value::as_bool) != Some(false);
    let add_to_policy = body.get("add_to_policy").and_then(Value::as_bool) != Some(false);
    let create_github = body.get("create_github_repo").and_then(Value::as_bool) == Some(true);
    let gh_visibility = body
        .get("github_visibility")
        .and_then(Value::as_str)
        .unwrap_or("private");

    // Validation (after normalisation).
    if name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "name required (after normalizing spaces/special chars, nothing was left)" }),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": format!("normalized name \"{name}\" still contains invalid chars — alphanumerics + dots/dashes/underscores only") }),
        ));
    }
    if !matches!(autonomy.as_str(), "drive" | "ask" | "shadow") {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "autonomy_level must be drive/ask/shadow" }),
        ));
    }

    let project_path = roots.code_root.join(&name);
    if project_path.exists() {
        return Err((
            StatusCode::CONFLICT,
            json!({ "ok": false, "error": format!("~/code/{name} already exists") }),
        ));
    }

    let mut steps: Vec<Value> = Vec::new();

    // 1. Clone or mkdir+init.
    if !git_url.is_empty() {
        let path_s = project_path.to_string_lossy().to_string();
        match run(
            "git",
            &["clone", &git_url, &path_s],
            None,
            Duration::from_secs(120),
        )
        .await
        {
            Some(o) if o.success => {
                steps.push(json!({ "step": "clone", "ok": true, "detail": git_url }));
            }
            other => {
                let detail = other
                    .map(|o| {
                        let s = if o.stderr.is_empty() {
                            o.stdout
                        } else {
                            o.stderr
                        };
                        s.chars().take(500).collect::<String>()
                    })
                    .unwrap_or_else(|| "git clone timed out".to_string());
                steps.push(json!({ "step": "clone", "ok": false, "detail": detail }));
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "ok": false, "error": "git clone failed", "steps": steps }),
                ));
            }
        }
    } else if let Err(e) = std::fs::create_dir_all(&project_path) {
        steps.push(json!({ "step": "mkdir+init", "ok": false, "detail": e.to_string() }));
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "ok": false, "error": "init failed", "steps": steps }),
        ));
    } else {
        let readme = format!(
            "# {name}\n\nCreated via subctl new-project wizard on {}.\n",
            chrono::Utc::now().to_rfc3339()
        );
        let _ = std::fs::write(project_path.join("README.md"), readme);
        let _ = run(
            "git",
            &[
                "-C",
                &project_path.to_string_lossy(),
                "init",
                "--initial-branch=main",
            ],
            None,
            Duration::from_secs(10),
        )
        .await;
        let _ = run(
            "git",
            &["-C", &project_path.to_string_lossy(), "add", "."],
            None,
            Duration::from_secs(5),
        )
        .await;
        let _ = run(
            "git",
            &[
                "-C",
                &project_path.to_string_lossy(),
                "commit",
                "-m",
                "Initial commit",
            ],
            None,
            Duration::from_secs(10),
        )
        .await;
        steps.push(
            json!({ "step": "mkdir+init", "ok": true, "detail": project_path.to_string_lossy() }),
        );

        // 1b. Optional GitHub create+push (non-fatal).
        if create_github {
            let vis_flag = match gh_visibility {
                "public" => "--public",
                "internal" => "--internal",
                _ => "--private",
            };
            let path_s = project_path.to_string_lossy().to_string();
            let desc = format!(
                "Created via subctl on {}",
                &chrono::Utc::now().to_rfc3339()[..10]
            );
            let args = [
                "repo",
                "create",
                &name,
                vis_flag,
                "--source",
                &path_s,
                "--remote",
                "origin",
                "--push",
                "--description",
                &desc,
            ];
            match run("gh", &args, None, Duration::from_secs(60)).await {
                Some(o) if o.success => {
                    let out = format!("{}{}", o.stdout, o.stderr);
                    let tail: String = out
                        .trim()
                        .chars()
                        .rev()
                        .take(200)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect();
                    let detail = if tail.is_empty() {
                        format!("created {gh_visibility} repo")
                    } else {
                        tail
                    };
                    steps.push(json!({ "step": "github", "ok": true, "detail": detail }));
                }
                other => {
                    let err = other
                        .map(|o| {
                            if o.stderr.is_empty() {
                                o.stdout
                            } else {
                                o.stderr
                            }
                        })
                        .unwrap_or_default();
                    let detail = format!(
                        "{}  (project created locally; you can push later with: gh repo create {name} {vis_flag} --source={path_s} --remote=origin --push)",
                        err.chars().take(500).collect::<String>()
                    );
                    steps.push(json!({ "step": "github", "ok": false, "detail": detail }));
                }
            }
        }
    }

    // 2. Vault (non-fatal).
    let mut vault_path: Option<String> = None;
    if create_vault {
        let vp = roots.vault_root.join(&name);
        let mk = std::fs::create_dir_all(vp.join("design"))
            .and_then(|()| std::fs::create_dir_all(vp.join("reviews")))
            .and_then(|()| std::fs::create_dir_all(vp.join("postmortems")));
        match mk {
            Ok(()) => {
                let repo_line = if git_url.is_empty() {
                    "**Repo:** (local-only)".to_string()
                } else {
                    format!("**Repo:** {git_url}")
                };
                let resume = format!(
                    "# {name} — RESUME\n\n**Path:** `{}`\n{repo_line}\n**Created:** {}\n\n## Current state\n\n_New project. Master will populate this as work progresses._\n\n## What's next\n\n- [ ] Define initial scope\n- [ ] Spawn first dev team\n",
                    project_path.to_string_lossy(),
                    chrono::Utc::now().to_rfc3339(),
                );
                let _ = std::fs::write(vp.join("RESUME.md"), resume);
                vault_path = Some(vp.to_string_lossy().to_string());
                steps.push(json!({ "step": "vault", "ok": true, "detail": vp.to_string_lossy() }));
            }
            Err(e) => {
                steps.push(json!({ "step": "vault", "ok": false, "detail": e.to_string() }));
            }
        }
    }

    // 3. policy.json append (non-fatal). Deviation: v3 then restarts the master
    // via launchctl; v4 does not bounce the v3 master.
    if add_to_policy {
        let policy_path = roots.config_evy.join("policy.json");
        match std::fs::read_to_string(&policy_path) {
            Ok(text) => match parse_policy_json_lenient(&text) {
                Some(mut doc) => {
                    let entry = json!({
                        "path": project_path.to_string_lossy(),
                        "autonomy_level": autonomy,
                        "_comment_autonomy": format!("Added via dashboard wizard on {}", &chrono::Utc::now().to_rfc3339()[..10]),
                    });
                    let arr = doc
                        .as_object_mut()
                        .map(|o| o.entry("projects").or_insert_with(|| json!([])));
                    if let Some(Value::Array(list)) = arr {
                        list.push(entry);
                    }
                    let wrote = serde_json::to_string_pretty(&doc)
                        .ok()
                        .filter(|pretty| std::fs::write(&policy_path, pretty).is_ok())
                        .is_some();
                    steps.push(if wrote {
                        json!({ "step": "policy", "ok": true, "detail": format!("appended {name} (autonomy={autonomy})") })
                    } else {
                        json!({ "step": "policy", "ok": false, "detail": "policy.json write failed" })
                    });
                }
                None => steps.push(
                    json!({ "step": "policy", "ok": false, "detail": "policy.json parse failed" }),
                ),
            },
            Err(_) => steps
                .push(json!({ "step": "policy", "ok": false, "detail": "policy.json missing" })),
        }
    }

    Ok(json!({
        "ok": true,
        "name": name,
        "path": project_path.to_string_lossy(),
        "vault_path": vault_path,
        "steps": steps,
    }))
}

// ---------------------------------------------------------------------------
// Policy presets (GET …/presets, POST …/preset/{path}).
// ---------------------------------------------------------------------------

/// Enumerate `*.toml` preset basenames under `<install>/config/policy/presets`,
/// sorted. Mirrors v3's `listAvailablePresets`.
fn list_presets_in(install: Option<&Path>) -> Vec<String> {
    let Some(root) = install else {
        return Vec::new();
    };
    let dir = root.join("config").join("policy").join("presets");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("toml"))
                .then(|| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
                .flatten()
        })
        .collect();
    names.sort();
    names
}

/// Env-backed preset list using `evy_policy::resolve_subctl_install`
/// (`SUBCTL_INSTALL_ROOT`). Any resolution error → `[]`, like v3.
fn list_available_presets() -> Vec<String> {
    let install = evy_policy::resolve_subctl_install().ok();
    list_presets_in(install.as_deref())
}

/// Resolve a project name/path segment to an absolute root under `code_root`
/// or `home`. Rejects traversal. Mirrors v3's `resolveProjectFromName`.
fn resolve_project_from_name(
    name: &str,
    code_root: &Path,
    home: &Path,
) -> Result<PathBuf, (StatusCode, Value)> {
    let invalid = || {
        (
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "invalid project name" }),
        )
    };
    if name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "project name required" }),
        ));
    }
    if name.contains("..") || name.contains('\0') {
        return Err(invalid());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._/~-".contains(c))
    {
        return Err(invalid());
    }

    let path = if let Some(rest) = name.strip_prefix('~') {
        home.join(rest.trim_start_matches('/'))
    } else if name.starts_with('/') {
        PathBuf::from(name)
    } else {
        code_root.join(name)
    };

    if !(path.starts_with(code_root) || path.starts_with(home)) {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": format!(
                "project path must be under {} or {}",
                code_root.to_string_lossy(),
                home.to_string_lossy()
            ) }),
        ));
    }
    if !path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": format!("project not found: {}", path.to_string_lossy()) }),
        ));
    }
    Ok(path)
}

/// Write a generated policy.toml and return the v3 `{ok,path,bytes,doc}` shape.
fn write_policy_file(path: &Path, toml: &str, doc: Value) -> Result<Value, (StatusCode, Value)> {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "ok": false, "path": path.to_string_lossy(), "error": e.to_string() }),
            ));
        }
    }
    match std::fs::write(path, toml) {
        Ok(()) => Ok(json!({
            "ok": true,
            "path": path.to_string_lossy(),
            "bytes": toml.len(),
            "doc": doc,
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "ok": false, "path": path.to_string_lossy(), "error": e.to_string() }),
        )),
    }
}

/// Core of `POST …/policy/preset/{path}` — write a preset-only policy.toml for
/// the named project. `install` scopes the known-preset validation set.
fn do_apply_preset(
    name: &str,
    body: &Value,
    code_root: &Path,
    home: &Path,
    install: Option<&Path>,
) -> Result<Value, (StatusCode, Value)> {
    let project = resolve_project_from_name(name, code_root, home)?;
    let preset = body.get("preset").and_then(Value::as_str).unwrap_or("");
    if preset.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "preset name required in body" }),
        ));
    }
    let known = list_presets_in(install);
    if !known.iter().any(|k| k == preset) {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": format!("unknown preset: {preset}"), "known": known }),
        ));
    }
    let toml = format!(
        "# subctl project policy — generated by dashboard \"Apply preset\" action\n# preset: {preset}\n# Generated: {}\n\npreset = \"{preset}\"\n\n[mode]\n",
        chrono::Utc::now().to_rfc3339(),
    );
    let policy_path = project.join(".subctl").join("policy.toml");
    write_policy_file(&policy_path, &toml, json!({ "mode": {}, "preset": preset }))
}

// ---------------------------------------------------------------------------
// Axum handlers — thin wrappers resolving roots from env.
// ---------------------------------------------------------------------------

/// Map a `Result<Value, (StatusCode, Value)>` core result into a response.
fn resp(r: Result<Value, (StatusCode, Value)>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err((status, body)) => (status, Json(body)).into_response(),
    }
}

/// Parse a request body as JSON, tolerating empty/invalid bodies the way v3's
/// `try { await req.json() }` does (returns `Value::Null` on failure).
fn parse_body(bytes: &Bytes) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

/// `GET /api/evy/projects` → `{ ok, code_root, projects:[…] }`.
pub(crate) async fn projects_list_handler() -> Response {
    Json(do_list(&Roots::from_env()).await).into_response()
}

/// `GET /api/evy/projects/{name}` → full project detail (v3 shape).
pub(crate) async fn project_detail_handler(AxPath(name): AxPath<String>) -> Response {
    resp(do_detail(&Roots::from_env(), &name).await)
}

/// `POST /api/evy/projects/create` → `{ ok, name, path, vault_path, steps }`.
pub(crate) async fn project_create_handler(bytes: Bytes) -> Response {
    let body = parse_body(&bytes);
    if body.is_null() && !bytes.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid JSON body" })),
        )
            .into_response();
    }
    resp(do_create(&Roots::from_env(), &body).await)
}

/// `GET /api/evy/policy/presets` → `{ ok, presets:[…] }`.
pub(crate) async fn presets_list_handler() -> Response {
    Json(json!({ "ok": true, "presets": list_available_presets() })).into_response()
}

/// `POST /api/evy/policy/preset/{path}` → writes a preset-only policy.toml.
pub(crate) async fn preset_apply_handler(AxPath(name): AxPath<String>, bytes: Bytes) -> Response {
    let roots = Roots::from_env();
    let install = evy_policy::resolve_subctl_install().ok();
    let body = parse_body(&bytes);
    resp(do_apply_preset(
        &name,
        &body,
        &roots.code_root,
        &roots.home,
        install.as_deref(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "evy-projects-test-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn roots_under(base: &Path) -> Roots {
        Roots {
            code_root: base.join("code"),
            config_evy: base.join("config").join("evy"),
            vault_root: base.join("vault"),
            home: base.to_path_buf(),
        }
    }

    #[test]
    fn strip_trailing_commas_keeps_string_commas() {
        let input = r#"{"a":"x,y",}"#;
        assert_eq!(strip_trailing_commas(input), r#"{"a":"x,y"}"#);
    }

    #[test]
    fn parse_policy_json_drops_comment_lines_and_trailing_commas() {
        let raw = "{\n  \"projects\": [\n    {\n      \"path\": \"/p\",\n      \"autonomy_level\": \"ask\",\n      \"_comment_autonomy\": \"note\"\n    }\n  ]\n}\n";
        let doc = parse_policy_json_lenient(raw).expect("parse");
        let projects = doc["projects"].as_array().unwrap();
        assert_eq!(projects[0]["path"], json!("/p"));
        assert_eq!(projects[0]["autonomy_level"], json!("ask"));
        assert!(projects[0].get("_comment_autonomy").is_none());
    }

    #[test]
    fn extract_gh_repo_variants() {
        assert_eq!(
            extract_gh_repo("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            extract_gh_repo("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(extract_gh_repo("https://gitlab.com/owner/repo"), None);
    }

    #[test]
    fn normalize_name_collapses_and_strips() {
        assert_eq!(normalize_name("  My Cool  Project!! "), "My-Cool-Project");
        assert_eq!(normalize_name("--weird__name--"), "weird__name");
        assert_eq!(normalize_name("!!!"), "");
    }

    #[test]
    fn normalize_git_url_shorthand_and_clone_prefix() {
        assert_eq!(
            normalize_git_url("owner/repo"),
            "https://github.com/owner/repo.git"
        );
        assert_eq!(
            normalize_git_url("gh repo clone owner/repo"),
            "https://github.com/owner/repo.git"
        );
        assert_eq!(
            normalize_git_url("https://github.com/owner/repo"),
            "https://github.com/owner/repo"
        );
    }

    #[test]
    fn list_presets_in_sorts_and_filters() {
        let base = tmpdir("presets");
        let dir = base.join("config").join("policy").join("presets");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("node.toml"), "").unwrap();
        std::fs::write(dir.join("generic.toml"), "").unwrap();
        std::fs::write(dir.join("ignore.txt"), "").unwrap();
        assert_eq!(list_presets_in(Some(&base)), vec!["generic", "node"]);
        assert!(list_presets_in(None).is_empty());
    }

    #[tokio::test]
    async fn list_marks_policy_membership() {
        let base = tmpdir("list");
        let roots = roots_under(&base);
        let proj = roots.code_root.join("subctl");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("CLAUDE.md"), "x").unwrap();
        std::fs::create_dir_all(roots.code_root.join(".hidden")).unwrap();
        std::fs::create_dir_all(&roots.config_evy).unwrap();
        let policy = json!({ "projects": [ { "path": proj.to_string_lossy(), "autonomy_level": "drive" } ] });
        std::fs::write(roots.config_evy.join("policy.json"), policy.to_string()).unwrap();

        let out = do_list(&roots).await;
        assert_eq!(out["ok"], json!(true));
        let projects = out["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 1, "hidden dirs excluded");
        assert_eq!(projects[0]["name"], json!("subctl"));
        assert_eq!(projects[0]["has_claude_md"], json!(true));
        assert_eq!(projects[0]["in_policy"], json!(true));
        assert_eq!(projects[0]["autonomy_level"], json!("drive"));
    }

    #[tokio::test]
    async fn detail_404_for_missing_project() {
        let base = tmpdir("detail404");
        let roots = roots_under(&base);
        std::fs::create_dir_all(&roots.code_root).unwrap();
        let err = do_detail(&roots, "nope").await.unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert_eq!(err.1["error"], json!("project not found"));
    }

    #[tokio::test]
    async fn detail_shape_for_non_repo_dir() {
        let base = tmpdir("detail");
        let roots = roots_under(&base);
        let proj = roots.code_root.join("demo");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("README.md"), "hi").unwrap();
        let v = do_detail(&roots, "demo").await.expect("ok");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["name"], json!("demo"));
        assert_eq!(v["dev_teams"], json!([]));
        assert_eq!(v["prs"], json!([]));
        assert_eq!(v["issues"], json!([]));
        assert_eq!(v["flags"]["has_readme"], json!(true));
        assert_eq!(v["ahead"], json!(0));
        assert_eq!(v["in_policy"], json!(false));
        assert!(v["vault"]["exists"] == json!(false));
    }

    #[tokio::test]
    async fn create_mkdir_vault_and_policy_append() {
        let base = tmpdir("create");
        let roots = roots_under(&base);
        std::fs::create_dir_all(&roots.code_root).unwrap();
        std::fs::create_dir_all(&roots.config_evy).unwrap();
        std::fs::write(
            roots.config_evy.join("policy.json"),
            json!({ "projects": [] }).to_string(),
        )
        .unwrap();

        let body =
            json!({ "name": "Fresh Idea", "autonomy_level": "drive", "create_github_repo": false });
        let v = do_create(&roots, &body).await.expect("created");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["name"], json!("Fresh-Idea"));
        assert!(roots
            .code_root
            .join("Fresh-Idea")
            .join("README.md")
            .exists());
        assert!(roots
            .vault_root
            .join("Fresh-Idea")
            .join("RESUME.md")
            .exists());
        assert_eq!(
            v["vault_path"],
            json!(roots.vault_root.join("Fresh-Idea").to_string_lossy())
        );

        // policy.json now carries the appended entry.
        let written = parse_policy_json_lenient(
            &std::fs::read_to_string(roots.config_evy.join("policy.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["projects"][0]["autonomy_level"], json!("drive"));
        // step ledger records each stage.
        let steps = v["steps"].as_array().unwrap();
        assert!(steps
            .iter()
            .any(|s| s["step"] == json!("policy") && s["ok"] == json!(true)));
    }

    #[tokio::test]
    async fn create_rejects_bad_autonomy_and_conflict() {
        let base = tmpdir("create2");
        let roots = roots_under(&base);
        std::fs::create_dir_all(&roots.code_root).unwrap();

        let bad = do_create(&roots, &json!({ "name": "x", "autonomy_level": "bogus" }))
            .await
            .unwrap_err();
        assert_eq!(bad.0, StatusCode::BAD_REQUEST);

        std::fs::create_dir_all(roots.code_root.join("dupe")).unwrap();
        let conflict = do_create(
            &roots,
            &json!({ "name": "dupe", "add_to_policy": false, "create_vault": false }),
        )
        .await
        .unwrap_err();
        assert_eq!(conflict.0, StatusCode::CONFLICT);
    }

    #[test]
    fn apply_preset_writes_policy_toml() {
        let base = tmpdir("apply");
        let code_root = base.join("code");
        let proj = code_root.join("subctl");
        std::fs::create_dir_all(&proj).unwrap();
        // install fixture with a `node` preset.
        let install = base.join("install");
        let pdir = install.join("config").join("policy").join("presets");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("node.toml"), "").unwrap();

        let ok = do_apply_preset(
            "subctl",
            &json!({ "preset": "node" }),
            &code_root,
            &base,
            Some(&install),
        )
        .expect("applied");
        assert_eq!(ok["ok"], json!(true));
        assert_eq!(ok["doc"]["preset"], json!("node"));
        let written = std::fs::read_to_string(proj.join(".subctl").join("policy.toml")).unwrap();
        assert!(written.contains("preset = \"node\""));
    }

    #[test]
    fn apply_preset_rejects_unknown_and_missing() {
        let base = tmpdir("apply2");
        let code_root = base.join("code");
        std::fs::create_dir_all(code_root.join("subctl")).unwrap();
        let install = base.join("install");
        std::fs::create_dir_all(install.join("config").join("policy").join("presets")).unwrap();

        let missing =
            do_apply_preset("subctl", &json!({}), &code_root, &base, Some(&install)).unwrap_err();
        assert_eq!(missing.0, StatusCode::BAD_REQUEST);
        assert_eq!(missing.1["error"], json!("preset name required in body"));

        let unknown = do_apply_preset(
            "subctl",
            &json!({ "preset": "ghost" }),
            &code_root,
            &base,
            Some(&install),
        )
        .unwrap_err();
        assert_eq!(unknown.0, StatusCode::BAD_REQUEST);
        assert!(unknown.1["error"]
            .as_str()
            .unwrap()
            .contains("unknown preset"));
    }

    #[test]
    fn resolve_project_rejects_traversal() {
        let base = tmpdir("resolve");
        let code_root = base.join("code");
        std::fs::create_dir_all(&code_root).unwrap();
        let err = resolve_project_from_name("../../etc/passwd", &code_root, &base).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }
}
