//! Hot-path policy check — the function the `PreToolUse` hook calls before
//! every Bash invocation in a Gated worker. Pack 06 §4 is the reference
//! algorithm; this is the faithful Rust port of
//! `components/evy/tools/policy/check.ts`.
//!
//! Failure modes (pack 11 §8): fail-closed. Empty command → deny. Missing
//! gated config in a Gated-mode policy → deny. Regex compile failure → skip
//! that single pattern (others still apply).
//!
//! Caches (regex + package.json + Makefile presence) are kept in module-
//! level `Mutex`-protected `HashMap`s. The lock is held only across the
//! cache lookup/insert; the hot path stays well inside the 20ms p99 budget
//! the v3 reference promises.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use regex::Regex;

use crate::tokenize::tokenize;
use crate::types::{
    AllowPattern, CheckOutcome, CheckRequest, GatedMode, Mode, Policy, ScriptTable,
};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Decide whether `req.command` is allowed under `policy`.
///
/// `mode` overrides `policy.default_mode` — call sites pass the spawn-time
/// mode so the check honours the operator's override (pack 02 §1: the
/// command-tier mode wins).
///
/// `RequireAudit` is never returned by this function; it exists in
/// [`CheckOutcome`] for forward use by the binary's audit gate path.
#[must_use]
pub fn check_command(req: &CheckRequest<'_>, policy: &Policy, mode: Mode) -> CheckOutcome {
    match mode {
        Mode::Trusted => CheckOutcome::Allow {
            rule: "trusted_mode".into(),
            rule_path: "mode.trusted".into(),
        },
        Mode::Sealed => CheckOutcome::Deny {
            rule: "sealed_mode_bash_disabled".into(),
            rule_path: "mode.sealed".into(),
        },
        Mode::Gated => match policy.mode.gated.as_ref() {
            Some(g) => check_gated(g, req),
            None => CheckOutcome::Deny {
                rule: "gated_mode_missing_config".into(),
                rule_path: "mode.gated.default_deny".into(),
            },
        },
    }
}

/// Convenience wrapper that mirrors the spec's stub signature
/// (`check_command(cmd, policy, mode)`). Internally fabricates a
/// `CheckRequest` with no cwd / team_id; ecosystem helpers that need
/// `cwd` will skip the package.json layer. Prefer
/// [`check_command`] in production.
#[must_use]
pub fn check_command_simple(cmd: &str, policy: &Policy, mode: Mode) -> CheckOutcome {
    let req = CheckRequest {
        command: cmd,
        cwd: Path::new(""),
        team_id: "",
        agent_session_id: None,
    };
    check_command(&req, policy, mode)
}

fn check_gated(g: &GatedMode, req: &CheckRequest<'_>) -> CheckOutcome {
    let cmd = req.command.trim();

    // 1. deny_always wins over everything. Operates on the RAW (trimmed)
    //    command line so multi-line heredocs / pipelines are visible.
    if let Some(o) = check_deny_always(g, cmd) {
        return o;
    }

    // 2. Tokenize.
    let tokens = tokenize(cmd);
    if tokens.is_empty() {
        return CheckOutcome::Deny {
            rule: "empty_command".into(),
            rule_path: "mode.gated.default_deny".into(),
        };
    }
    let head = tokens[0].as_str();
    let first_non_flag = tokens[1..].iter().find(|t| !t.starts_with('-'));

    // 3. Ecosystem-specific checks. These fire BEFORE the allow_pattern walk
    //    and apply kill semantics: if a config table applies and the command
    //    is a script invocation, the helper returns the final allow/deny.
    if let Some(o) = check_ecosystem(g, head, &tokens, req.cwd) {
        return o;
    }

    // 4. allow_pattern walk — first match wins.
    if let Some(patterns) = g.allow_pattern.as_ref() {
        for (i, ap) in patterns.iter().enumerate() {
            if ap.command != head {
                continue;
            }
            if !args_match(ap, first_non_flag.map(String::as_str)) {
                continue;
            }
            if let Some(deny) = check_deny_if_arg_contains(ap, &tokens, i) {
                return deny;
            }
            return CheckOutcome::Allow {
                rule: format!(
                    "allow_pattern: {} {}",
                    ap.command,
                    ap.args.as_deref().unwrap_or(&[]).join("|"),
                ),
                rule_path: format!("mode.gated.allow_pattern[{i}]"),
            };
        }
    }

    // 5. allow.commands exact head match.
    if let Some(allow) = g.allow.as_ref() {
        if let Some(cmds) = allow.commands.as_ref() {
            if cmds.iter().any(|c| c == head) {
                return CheckOutcome::Allow {
                    rule: format!("allow.commands: {head}"),
                    rule_path: "mode.gated.allow.commands".into(),
                };
            }
        }
    }

    // 6. Default deny.
    CheckOutcome::Deny {
        rule: "no_match_default_deny".into(),
        rule_path: "mode.gated.default_deny".into(),
    }
}

fn args_match(ap: &AllowPattern, first_non_flag: Option<&str>) -> bool {
    match ap.args.as_ref() {
        None => true,
        Some(v) if v.is_empty() => true,
        Some(v) => match first_non_flag {
            Some(a) => v.iter().any(|x| x == a),
            None => false,
        },
    }
}

fn check_deny_if_arg_contains(
    ap: &AllowPattern,
    tokens: &[String],
    index: usize,
) -> Option<CheckOutcome> {
    let needles = ap.deny_if_arg_contains.as_ref()?;
    for needle in needles {
        if tokens.iter().any(|t| t.contains(needle)) {
            return Some(CheckOutcome::Deny {
                rule: format!("deny_if_arg_contains: \"{needle}\""),
                rule_path: format!("mode.gated.allow_pattern[{index}].deny_if_arg_contains"),
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// deny_always
// ---------------------------------------------------------------------------

fn check_deny_always(g: &GatedMode, cmd: &str) -> Option<CheckOutcome> {
    let da = g.deny_always.as_ref()?;
    if let Some(subs) = da.substrings.as_ref() {
        for sub in subs {
            if cmd.contains(sub) {
                return Some(CheckOutcome::Deny {
                    rule: format!("deny_always.substrings: \"{sub}\""),
                    rule_path: "mode.gated.deny_always.substrings".into(),
                });
            }
        }
    }
    if let Some(pats) = da.regex.as_ref() {
        for pat in pats {
            if let Some(re) = try_compile_regex(pat) {
                if re.is_match(cmd) {
                    return Some(CheckOutcome::Deny {
                        rule: format!("deny_always.regex: {pat}"),
                        rule_path: "mode.gated.deny_always.regex".into(),
                    });
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Ecosystem-specific helpers
// ---------------------------------------------------------------------------

fn check_ecosystem(
    g: &GatedMode,
    head: &str,
    tokens: &[String],
    cwd: &Path,
) -> Option<CheckOutcome> {
    let rest = &tokens[1..];
    if head == "npm" {
        if let Some(t) = g.npm.as_ref() {
            return check_script_runner(t, "npm", rest, cwd);
        }
    }
    if head == "pnpm" {
        if let Some(t) = g.pnpm.as_ref() {
            return check_script_runner(t, "pnpm", rest, cwd);
        }
    }
    if head == "yarn" {
        if let Some(t) = g.yarn.as_ref() {
            return check_script_runner(t, "yarn", rest, cwd);
        }
    }
    if head == "bun" {
        if let Some(t) = g.bun.as_ref() {
            return check_script_runner(t, "bun", rest, cwd);
        }
    }
    if (head == "python" || head == "python3") && g.python_modules.is_some() {
        return check_python_module(g, rest);
    }
    if head == "uv" {
        if let Some(t) = g.uv.as_ref() {
            return check_uv_run(t, rest);
        }
    }
    if head == "poetry" {
        if let Some(t) = g.poetry.as_ref() {
            return check_poetry_run(t, rest);
        }
    }
    if head == "make" {
        if let Some(t) = g.make.as_ref() {
            return check_make_target(t, rest, cwd);
        }
    }
    if head == "just" {
        if let Some(t) = g.just.as_ref() {
            return check_just_recipe(t, rest);
        }
    }
    None
}

fn check_script_runner(
    table: &ScriptTable,
    runner: &str,
    rest: &[String],
    cwd: &Path,
) -> Option<CheckOutcome> {
    let first = rest.first().map(String::as_str)?;
    if first != "run" && first != "run-script" {
        return None;
    }
    let script = rest
        .iter()
        .enumerate()
        .find(|(i, t)| *i >= 1 && !t.starts_with('-'))
        .map(|(_, t)| t.as_str())?;

    // If a package.json exists at cwd with a `scripts` section, the script
    // must also be declared there. This prevents bypass-by-adding-a-script.
    if !cwd.as_os_str().is_empty() {
        if let Some(pkg) = read_package_json_cached(cwd) {
            if let Some(scripts) = pkg.scripts {
                if !scripts.contains_key(script) {
                    return Some(CheckOutcome::Deny {
                        rule: format!(
                            "{runner}.allowed_scripts: \"{script}\" not declared in package.json",
                        ),
                        rule_path: format!("mode.gated.{runner}.allowed_scripts"),
                    });
                }
            }
        }
    }

    if !table.allowed_scripts.iter().any(|s| s == script) {
        return Some(CheckOutcome::Deny {
            rule: format!("{runner}.allowed_scripts: \"{script}\" not allowlisted"),
            rule_path: format!("mode.gated.{runner}.allowed_scripts"),
        });
    }
    Some(CheckOutcome::Allow {
        rule: format!("{runner}.allowed_scripts: \"{script}\""),
        rule_path: format!("mode.gated.{runner}.allowed_scripts"),
    })
}

fn check_python_module(g: &GatedMode, rest: &[String]) -> Option<CheckOutcome> {
    let pos = rest.iter().position(|t| t == "-m")?;
    let module = rest.get(pos + 1)?;
    let allowed = g
        .python_modules
        .as_ref()
        .map(|p| p.allowed.as_slice())
        .unwrap_or(&[]);
    if allowed.iter().any(|m| m == module) {
        Some(CheckOutcome::Allow {
            rule: format!("python_modules.allowed: \"{module}\""),
            rule_path: "mode.gated.python_modules.allowed".into(),
        })
    } else {
        Some(CheckOutcome::Deny {
            rule: format!("python_modules.allowed: \"{module}\" not allowlisted"),
            rule_path: "mode.gated.python_modules.allowed".into(),
        })
    }
}

fn check_uv_run(table: &crate::types::RunTargetsTable, rest: &[String]) -> Option<CheckOutcome> {
    if rest.first().map(String::as_str) != Some("run") {
        return None;
    }
    let target = rest
        .iter()
        .enumerate()
        .find(|(i, t)| *i >= 1 && !t.starts_with('-'))
        .map(|(_, t)| t.as_str())?;
    if table.allowed_run_targets.iter().any(|s| s == target) {
        Some(CheckOutcome::Allow {
            rule: format!("uv.allowed_run_targets: \"{target}\""),
            rule_path: "mode.gated.uv.allowed_run_targets".into(),
        })
    } else {
        Some(CheckOutcome::Deny {
            rule: format!("uv.allowed_run_targets: \"{target}\" not allowlisted"),
            rule_path: "mode.gated.uv.allowed_run_targets".into(),
        })
    }
}

fn check_poetry_run(
    table: &crate::types::RunTargetsTable,
    rest: &[String],
) -> Option<CheckOutcome> {
    if rest.first().map(String::as_str) != Some("run") {
        return None;
    }
    let target = rest
        .iter()
        .enumerate()
        .find(|(i, t)| *i >= 1 && !t.starts_with('-'))
        .map(|(_, t)| t.as_str())?;
    if table.allowed_run_targets.iter().any(|s| s == target) {
        Some(CheckOutcome::Allow {
            rule: format!("poetry.allowed_run_targets: \"{target}\""),
            rule_path: "mode.gated.poetry.allowed_run_targets".into(),
        })
    } else {
        Some(CheckOutcome::Deny {
            rule: format!("poetry.allowed_run_targets: \"{target}\" not allowlisted"),
            rule_path: "mode.gated.poetry.allowed_run_targets".into(),
        })
    }
}

fn check_make_target(
    table: &crate::types::MakeTable,
    rest: &[String],
    cwd: &Path,
) -> Option<CheckOutcome> {
    let target = rest.iter().find(|t| !t.starts_with('-'))?.as_str();
    // Best-effort Makefile presence check (cached). We don't gate on the
    // result yet — parsing Makefile targets cleanly is a v2.8 follow-up per
    // the v3 reference.
    let _present = read_makefile_presence_cached(cwd);
    if table.allowed_targets.iter().any(|s| s == target) {
        Some(CheckOutcome::Allow {
            rule: format!("make.allowed_targets: \"{target}\""),
            rule_path: "mode.gated.make.allowed_targets".into(),
        })
    } else {
        Some(CheckOutcome::Deny {
            rule: format!("make.allowed_targets: \"{target}\" not allowlisted"),
            rule_path: "mode.gated.make.allowed_targets".into(),
        })
    }
}

fn check_just_recipe(table: &crate::types::JustTable, rest: &[String]) -> Option<CheckOutcome> {
    let recipe = rest.iter().find(|t| !t.starts_with('-'))?.as_str();
    if table.allowed_recipes.iter().any(|s| s == recipe) {
        Some(CheckOutcome::Allow {
            rule: format!("just.allowed_recipes: \"{recipe}\""),
            rule_path: "mode.gated.just.allowed_recipes".into(),
        })
    } else {
        Some(CheckOutcome::Deny {
            rule: format!("just.allowed_recipes: \"{recipe}\" not allowlisted"),
            rule_path: "mode.gated.just.allowed_recipes".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Caches
// ---------------------------------------------------------------------------

static REGEX_CACHE: LazyLock<Mutex<HashMap<String, Option<Regex>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn try_compile_regex(pat: &str) -> Option<Regex> {
    let mut cache = REGEX_CACHE.lock().expect("regex cache poisoned");
    if let Some(entry) = cache.get(pat) {
        return entry.clone();
    }
    let compiled = Regex::new(pat).ok();
    cache.insert(pat.to_owned(), compiled.clone());
    compiled
}

#[derive(serde::Deserialize, Clone)]
struct PackageJsonShape {
    #[serde(default)]
    scripts: Option<HashMap<String, String>>,
}

type PkgJsonCacheEntry = (i64, Option<PackageJsonShape>);
type PkgJsonCache = Mutex<HashMap<std::path::PathBuf, PkgJsonCacheEntry>>;

static PKG_JSON_CACHE: LazyLock<PkgJsonCache> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn read_package_json_cached(cwd: &Path) -> Option<PackageJsonShape> {
    let path = cwd.join("package.json");
    let mtime = fs::metadata(&path).ok().and_then(|m| {
        m.modified().ok().and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as i64)
        })
    });
    let mut cache = PKG_JSON_CACHE.lock().expect("pkg json cache poisoned");
    if let Some((cached_mtime, doc)) = cache.get(&path) {
        if Some(*cached_mtime) == mtime {
            return doc.clone();
        }
    }
    let doc = if mtime.is_some() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<PackageJsonShape>(&text).ok())
    } else {
        None
    };
    cache.insert(path, (mtime.unwrap_or(-1), doc.clone()));
    doc
}

static MAKEFILE_PRESENCE_CACHE: LazyLock<Mutex<HashMap<std::path::PathBuf, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn read_makefile_presence_cached(cwd: &Path) -> bool {
    if cwd.as_os_str().is_empty() {
        return false;
    }
    let mut cache = MAKEFILE_PRESENCE_CACHE
        .lock()
        .expect("makefile cache poisoned");
    if let Some(present) = cache.get(cwd) {
        return *present;
    }
    let present = cwd.join("Makefile").exists() || cwd.join("GNUmakefile").exists();
    cache.insert(cwd.to_path_buf(), present);
    present
}

/// Test hook: clear every internal cache. Not part of the production API.
#[doc(hidden)]
pub fn _reset_caches_for_testing() {
    REGEX_CACHE.lock().expect("regex cache poisoned").clear();
    PKG_JSON_CACHE
        .lock()
        .expect("pkg json cache poisoned")
        .clear();
    MAKEFILE_PRESENCE_CACHE
        .lock()
        .expect("makefile cache poisoned")
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AllowPattern, AllowSection, DenyAlways, GatedMode, JustTable, MakeTable, ModeTables,
        Policy, PythonModulesTable, RunTargetsTable, ScriptTable,
    };
    use std::path::PathBuf;

    fn fixture_node() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("node-project")
    }

    fn req<'a>(cmd: &'a str) -> CheckRequest<'a> {
        CheckRequest {
            command: cmd,
            cwd: Path::new("/tmp/__check_test__"),
            team_id: "t",
            agent_session_id: None,
        }
    }

    fn req_with_cwd<'a>(cmd: &'a str, cwd: &'a Path) -> CheckRequest<'a> {
        CheckRequest {
            command: cmd,
            cwd,
            team_id: "t",
            agent_session_id: None,
        }
    }

    fn policy_with(gated: GatedMode) -> Policy {
        Policy {
            preset: None,
            default_mode: Some(Mode::Gated),
            mode: ModeTables {
                trusted: None,
                gated: Some(gated),
                sealed: None,
            },
            meta: None,
        }
    }

    // -- mode handling -------------------------------------------------------

    #[test]
    fn trusted_mode_blanket_allow() {
        let p = Policy::default();
        let o = check_command(&req("rm -rf /"), &p, Mode::Trusted);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.trusted");
    }

    #[test]
    fn sealed_mode_blanket_deny() {
        let p = Policy::default();
        let o = check_command(&req("ls"), &p, Mode::Sealed);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.sealed");
    }

    #[test]
    fn gated_mode_missing_gated_table_denies() {
        let p = Policy {
            default_mode: Some(Mode::Gated),
            ..Policy::default()
        };
        let o = check_command(&req("ls"), &p, Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.default_deny");
    }

    // -- allow.commands ------------------------------------------------------

    #[test]
    fn allow_commands_match_head() {
        let p = policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec!["ls".into(), "pwd".into(), "echo".into()]),
                ..Default::default()
            }),
            ..Default::default()
        });
        let o = check_command(&req("ls -la"), &p, Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow.commands");
    }

    #[test]
    fn allow_commands_unlisted_denies() {
        let p = policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec!["ls".into()]),
                ..Default::default()
            }),
            ..Default::default()
        });
        let o = check_command(&req("cat /etc/passwd"), &p, Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.default_deny");
    }

    // -- allow_pattern -------------------------------------------------------

    fn pattern_policy() -> Policy {
        policy_with(GatedMode {
            allow_pattern: Some(vec![
                AllowPattern {
                    command: "git".into(),
                    args: Some(
                        ["status", "diff", "log", "commit"]
                            .iter()
                            .map(|s| (*s).into())
                            .collect(),
                    ),
                    deny_if_arg_contains: None,
                },
                AllowPattern {
                    command: "curl".into(),
                    args: Some(vec![]),
                    deny_if_arg_contains: None,
                },
                AllowPattern {
                    command: "echo".into(),
                    args: None,
                    deny_if_arg_contains: None,
                },
            ]),
            ..Default::default()
        })
    }

    #[test]
    fn allow_pattern_matches_arg_in_list() {
        let o = check_command(&req("git status"), &pattern_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[0]");
    }

    #[test]
    fn allow_pattern_arg_not_in_list_falls_through_to_default_deny() {
        let o = check_command(&req("git push"), &pattern_policy(), Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.default_deny");
    }

    #[test]
    fn allow_pattern_empty_args_allows_any_arg() {
        let o = check_command(
            &req("curl https://api.example.com"),
            &pattern_policy(),
            Mode::Gated,
        );
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[1]");
    }

    #[test]
    fn allow_pattern_undefined_args_allows_any() {
        let o = check_command(&req("echo hi"), &pattern_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[2]");
    }

    #[test]
    fn allow_pattern_flags_before_first_positional_skipped() {
        let o = check_command(&req("git -v status"), &pattern_policy(), Mode::Gated);
        assert!(o.is_allow());
    }

    #[test]
    fn allow_pattern_first_match_wins() {
        let p = policy_with(GatedMode {
            allow_pattern: Some(vec![
                AllowPattern {
                    command: "git".into(),
                    args: Some(vec!["status".into()]),
                    deny_if_arg_contains: None,
                },
                AllowPattern {
                    command: "git".into(),
                    args: Some(vec!["status".into(), "push".into()]),
                    deny_if_arg_contains: None,
                },
            ]),
            ..Default::default()
        });
        let o = check_command(&req("git status"), &p, Mode::Gated);
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[0]");
    }

    // -- deny_if_arg_contains ------------------------------------------------

    fn diac_policy() -> Policy {
        policy_with(GatedMode {
            allow_pattern: Some(vec![AllowPattern {
                command: "git".into(),
                args: Some(vec!["reset".into()]),
                deny_if_arg_contains: Some(vec!["--hard".into(), "--force".into()]),
            }]),
            ..Default::default()
        })
    }

    #[test]
    fn deny_if_arg_contains_fires() {
        let o = check_command(&req("git reset --hard HEAD~1"), &diac_policy(), Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(
            o.rule_path(),
            "mode.gated.allow_pattern[0].deny_if_arg_contains"
        );
        assert!(o.rule().contains("--hard"));
    }

    #[test]
    fn deny_if_arg_contains_no_needle_allows() {
        let o = check_command(&req("git reset HEAD~1"), &diac_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[0]");
    }

    #[test]
    fn deny_if_arg_contains_matches_substring_within_token() {
        let o = check_command(
            &req("git reset --force-with-lease HEAD"),
            &diac_policy(),
            Mode::Gated,
        );
        assert!(o.is_deny());
    }

    // -- deny_always.substrings ---------------------------------------------

    fn substr_policy() -> Policy {
        policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec!["ls".into(), "echo".into(), "git".into()]),
                ..Default::default()
            }),
            allow_pattern: Some(vec![AllowPattern {
                command: "git".into(),
                args: Some(vec!["commit".into()]),
                deny_if_arg_contains: None,
            }]),
            deny_always: Some(DenyAlways {
                substrings: Some(vec!["rm -rf".into(), "dd if=".into(), "mkfs.".into()]),
                regex: None,
            }),
            ..Default::default()
        })
    }

    #[test]
    fn substr_beats_allow_commands() {
        let o = check_command(&req("rm -rf /tmp/x"), &substr_policy(), Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.deny_always.substrings");
    }

    #[test]
    fn substr_beats_allow_pattern() {
        let o = check_command(
            &req("git commit -m 'rm -rf old files'"),
            &substr_policy(),
            Mode::Gated,
        );
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.deny_always.substrings");
    }

    #[test]
    fn substr_case_sensitive() {
        let o = check_command(&req("RM -RF /tmp/x"), &substr_policy(), Mode::Gated);
        assert!(o.is_deny());
        assert_ne!(o.rule_path(), "mode.gated.deny_always.substrings");
    }

    #[test]
    fn substr_no_match_allows() {
        let o = check_command(
            &req("git commit -m 'add policy gate'"),
            &substr_policy(),
            Mode::Gated,
        );
        assert!(o.is_allow());
    }

    // -- deny_always.regex ---------------------------------------------------

    fn regex_policy() -> Policy {
        policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec![
                    "echo".into(),
                    "node".into(),
                    "python".into(),
                    "python3".into(),
                    "curl".into(),
                    "wget".into(),
                ]),
                ..Default::default()
            }),
            deny_always: Some(DenyAlways {
                substrings: None,
                regex: Some(vec![
                    r"\bnode\s+-e\b".into(),
                    r"\bpython3?\s+-c\b".into(),
                    r"\bcurl\b[^|]*\|\s*(sh|bash)\b".into(),
                    "this[[[invalid".into(), // bogus, must be silently skipped
                ]),
            }),
            ..Default::default()
        })
    }

    #[test]
    fn regex_match_denies() {
        let o = check_command(
            &req(r#"node -e "process.exit(0)""#),
            &regex_policy(),
            Mode::Gated,
        );
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.deny_always.regex");
    }

    #[test]
    fn regex_python_optional_3() {
        for cmd in ["python -c 'x'", "python3 -c 'x'"] {
            let o = check_command(&req(cmd), &regex_policy(), Mode::Gated);
            assert!(o.is_deny(), "{cmd}: {o:?}");
        }
    }

    #[test]
    fn regex_bad_pattern_silently_skipped() {
        let o = check_command(
            &req("curl https://x.com | sh"),
            &regex_policy(),
            Mode::Gated,
        );
        assert!(o.is_deny());
    }

    #[test]
    fn regex_non_match_allows() {
        let o = check_command(
            &req("curl https://docs.example.com"),
            &regex_policy(),
            Mode::Gated,
        );
        assert!(o.is_allow());
    }

    // -- ecosystem: script runners ------------------------------------------

    fn runners_policy() -> Policy {
        policy_with(GatedMode {
            allow_pattern: Some(vec![
                AllowPattern {
                    command: "npm".into(),
                    args: Some(vec!["install".into(), "test".into(), "run".into()]),
                    deny_if_arg_contains: None,
                },
                AllowPattern {
                    command: "pnpm".into(),
                    args: Some(vec!["install".into(), "run".into()]),
                    deny_if_arg_contains: None,
                },
                AllowPattern {
                    command: "yarn".into(),
                    args: Some(vec!["install".into(), "run".into(), "test".into()]),
                    deny_if_arg_contains: None,
                },
                AllowPattern {
                    command: "bun".into(),
                    args: Some(vec!["install".into(), "run".into(), "test".into()]),
                    deny_if_arg_contains: None,
                },
            ]),
            npm: Some(ScriptTable {
                allowed_scripts: vec!["test".into(), "lint".into(), "build".into()],
            }),
            pnpm: Some(ScriptTable {
                allowed_scripts: vec!["test".into(), "lint".into()],
            }),
            yarn: Some(ScriptTable {
                allowed_scripts: vec!["test".into(), "lint".into()],
            }),
            bun: Some(ScriptTable {
                allowed_scripts: vec!["test".into(), "lint".into()],
            }),
            ..Default::default()
        })
    }

    #[test]
    fn npm_run_allowed_allows() {
        _reset_caches_for_testing();
        let o = check_command(&req("npm run lint"), &runners_policy(), Mode::Gated);
        assert!(o.is_allow(), "{o:?}");
        assert_eq!(o.rule_path(), "mode.gated.npm.allowed_scripts");
    }

    #[test]
    fn npm_run_disallowed_denies() {
        _reset_caches_for_testing();
        let o = check_command(&req("npm run evil-script"), &runners_policy(), Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.npm.allowed_scripts");
    }

    #[test]
    fn npm_run_script_allowed_allows() {
        _reset_caches_for_testing();
        let o = check_command(&req("npm run-script build"), &runners_policy(), Mode::Gated);
        assert!(o.is_allow());
    }

    #[test]
    fn npm_install_falls_through_to_allow_pattern() {
        _reset_caches_for_testing();
        let o = check_command(&req("npm install"), &runners_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[0]");
    }

    #[test]
    fn npm_test_falls_through_to_allow_pattern() {
        _reset_caches_for_testing();
        let o = check_command(&req("npm test"), &runners_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[0]");
    }

    #[test]
    fn pnpm_run_allowed_allows() {
        _reset_caches_for_testing();
        let o = check_command(&req("pnpm run test"), &runners_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.pnpm.allowed_scripts");
    }

    #[test]
    fn bun_run_disallowed_denies() {
        _reset_caches_for_testing();
        let o = check_command(&req("bun run dangerous"), &runners_policy(), Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.bun.allowed_scripts");
    }

    #[test]
    fn yarn_run_allowed_allows() {
        _reset_caches_for_testing();
        let o = check_command(&req("yarn run lint"), &runners_policy(), Mode::Gated);
        assert!(o.is_allow());
    }

    #[test]
    fn npm_run_no_script_name_falls_through() {
        _reset_caches_for_testing();
        let o = check_command(&req("npm run"), &runners_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[0]");
    }

    #[test]
    fn package_json_declared_but_not_allowlisted_denies() {
        _reset_caches_for_testing();
        let cwd = fixture_node();
        let o = check_command(
            &req_with_cwd("npm run evil", &cwd),
            &runners_policy(),
            Mode::Gated,
        );
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.npm.allowed_scripts");
    }

    #[test]
    fn package_json_not_declared_denies_with_undeclared_reason() {
        _reset_caches_for_testing();
        let cwd = fixture_node();
        let o = check_command(
            &req_with_cwd("npm run evil-script", &cwd),
            &runners_policy(),
            Mode::Gated,
        );
        assert!(o.is_deny());
        assert!(o.rule().contains("not declared in package.json"), "{o:?}");
    }

    #[test]
    fn package_json_declared_and_allowlisted_allows() {
        _reset_caches_for_testing();
        let cwd = fixture_node();
        let o = check_command(
            &req_with_cwd("npm run lint", &cwd),
            &runners_policy(),
            Mode::Gated,
        );
        assert!(o.is_allow(), "{o:?}");
    }

    // -- ecosystem: python_modules ------------------------------------------

    fn python_policy() -> Policy {
        policy_with(GatedMode {
            allow_pattern: Some(vec![
                AllowPattern {
                    command: "python".into(),
                    args: Some(vec![]),
                    deny_if_arg_contains: None,
                },
                AllowPattern {
                    command: "python3".into(),
                    args: Some(vec![]),
                    deny_if_arg_contains: None,
                },
            ]),
            python_modules: Some(PythonModulesTable {
                allowed: vec!["pytest".into(), "ruff".into(), "pip".into()],
            }),
            ..Default::default()
        })
    }

    #[test]
    fn python_m_allowed_allows() {
        let o = check_command(
            &req("python -m pytest tests/"),
            &python_policy(),
            Mode::Gated,
        );
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.python_modules.allowed");
    }

    #[test]
    fn python3_m_allowed_allows() {
        let o = check_command(
            &req("python3 -m ruff check ."),
            &python_policy(),
            Mode::Gated,
        );
        assert!(o.is_allow());
    }

    #[test]
    fn python_m_disallowed_denies() {
        let o = check_command(
            &req("python -m alembic upgrade head"),
            &python_policy(),
            Mode::Gated,
        );
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.python_modules.allowed");
    }

    #[test]
    fn python_m_no_module_falls_through() {
        let o = check_command(&req("python -m"), &python_policy(), Mode::Gated);
        assert!(o.is_allow());
    }

    #[test]
    fn python_no_m_falls_through() {
        let o = check_command(&req("python script.py"), &python_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[0]");
    }

    // -- ecosystem: uv / poetry ---------------------------------------------

    fn uv_poetry_policy() -> Policy {
        policy_with(GatedMode {
            allow_pattern: Some(vec![
                AllowPattern {
                    command: "uv".into(),
                    args: Some(vec!["sync".into(), "run".into()]),
                    deny_if_arg_contains: None,
                },
                AllowPattern {
                    command: "poetry".into(),
                    args: Some(vec!["install".into(), "run".into()]),
                    deny_if_arg_contains: None,
                },
            ]),
            uv: Some(RunTargetsTable {
                allowed_run_targets: vec!["pytest".into(), "ruff".into()],
            }),
            poetry: Some(RunTargetsTable {
                allowed_run_targets: vec!["pytest".into()],
            }),
            ..Default::default()
        })
    }

    #[test]
    fn uv_run_allowed_allows() {
        let o = check_command(&req("uv run pytest"), &uv_poetry_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.uv.allowed_run_targets");
    }

    #[test]
    fn uv_run_disallowed_denies() {
        let o = check_command(
            &req("uv run mystery-script"),
            &uv_poetry_policy(),
            Mode::Gated,
        );
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.uv.allowed_run_targets");
    }

    #[test]
    fn uv_sync_falls_through() {
        let o = check_command(&req("uv sync"), &uv_poetry_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[0]");
    }

    #[test]
    fn poetry_run_allowed_allows() {
        let o = check_command(&req("poetry run pytest"), &uv_poetry_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.poetry.allowed_run_targets");
    }

    #[test]
    fn poetry_run_disallowed_denies() {
        let o = check_command(
            &req("poetry run shell-out"),
            &uv_poetry_policy(),
            Mode::Gated,
        );
        assert!(o.is_deny());
    }

    #[test]
    fn uv_run_no_target_falls_through() {
        let o = check_command(&req("uv run"), &uv_poetry_policy(), Mode::Gated);
        assert!(o.is_allow());
    }

    // -- ecosystem: make / just ---------------------------------------------

    fn make_just_policy() -> Policy {
        policy_with(GatedMode {
            make: Some(MakeTable {
                allowed_targets: vec!["test".into(), "build".into()],
            }),
            just: Some(JustTable {
                allowed_recipes: vec!["test".into(), "build".into()],
            }),
            ..Default::default()
        })
    }

    #[test]
    fn make_allowed_allows() {
        let o = check_command(&req("make test"), &make_just_policy(), Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.make.allowed_targets");
    }

    #[test]
    fn make_disallowed_denies() {
        let o = check_command(&req("make deploy"), &make_just_policy(), Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.make.allowed_targets");
    }

    #[test]
    fn just_allowed_allows() {
        let o = check_command(&req("just test"), &make_just_policy(), Mode::Gated);
        assert!(o.is_allow());
    }

    #[test]
    fn just_disallowed_denies() {
        let o = check_command(&req("just deploy"), &make_just_policy(), Mode::Gated);
        assert!(o.is_deny());
    }

    #[test]
    fn bare_make_no_target_default_deny() {
        let o = check_command(&req("make"), &make_just_policy(), Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.default_deny");
    }

    // -- precedence ---------------------------------------------------------

    #[test]
    fn deny_always_beats_allow_pattern() {
        let p = policy_with(GatedMode {
            allow_pattern: Some(vec![AllowPattern {
                command: "echo".into(),
                args: Some(vec![]),
                deny_if_arg_contains: None,
            }]),
            deny_always: Some(DenyAlways {
                substrings: Some(vec!["rm -rf".into()]),
                regex: None,
            }),
            ..Default::default()
        });
        let o = check_command(&req("echo 'rm -rf is dangerous'"), &p, Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.deny_always.substrings");
    }

    #[test]
    fn deny_always_beats_allow_commands() {
        let p = policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec!["rm".into()]),
                ..Default::default()
            }),
            deny_always: Some(DenyAlways {
                substrings: Some(vec!["rm -rf".into()]),
                regex: None,
            }),
            ..Default::default()
        });
        let o = check_command(&req("rm -rf /tmp/x"), &p, Mode::Gated);
        assert!(o.is_deny());
    }

    #[test]
    fn deny_if_arg_contains_beats_allow_pattern_allow() {
        let p = policy_with(GatedMode {
            allow_pattern: Some(vec![AllowPattern {
                command: "git".into(),
                args: Some(vec!["reset".into()]),
                deny_if_arg_contains: Some(vec!["--hard".into()]),
            }]),
            ..Default::default()
        });
        let o = check_command(&req("git reset --hard"), &p, Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(
            o.rule_path(),
            "mode.gated.allow_pattern[0].deny_if_arg_contains"
        );
    }

    #[test]
    fn allow_pattern_beats_allow_commands_when_both_match() {
        let p = policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec!["git".into()]),
                ..Default::default()
            }),
            allow_pattern: Some(vec![AllowPattern {
                command: "git".into(),
                args: Some(vec!["status".into()]),
                deny_if_arg_contains: None,
            }]),
            ..Default::default()
        });
        let o = check_command(&req("git status"), &p, Mode::Gated);
        assert!(o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.allow_pattern[0]");
    }

    #[test]
    fn ecosystem_deny_beats_allow_pattern_allow() {
        let p = policy_with(GatedMode {
            allow_pattern: Some(vec![AllowPattern {
                command: "npm".into(),
                args: Some(vec!["run".into()]),
                deny_if_arg_contains: None,
            }]),
            npm: Some(ScriptTable {
                allowed_scripts: vec!["test".into()],
            }),
            ..Default::default()
        });
        _reset_caches_for_testing();
        let o = check_command(&req("npm run evil"), &p, Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.npm.allowed_scripts");
    }

    // -- default deny + edges -----------------------------------------------

    #[test]
    fn unrelated_command_default_denies() {
        let p = policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec!["ls".into()]),
                ..Default::default()
            }),
            allow_pattern: Some(vec![AllowPattern {
                command: "git".into(),
                args: Some(vec!["status".into()]),
                deny_if_arg_contains: None,
            }]),
            ..Default::default()
        });
        let o = check_command(&req("ssh user@example.com"), &p, Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule_path(), "mode.gated.default_deny");
    }

    #[test]
    fn empty_command_denies() {
        let p = policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec!["ls".into()]),
                ..Default::default()
            }),
            ..Default::default()
        });
        let o = check_command(&req(""), &p, Mode::Gated);
        assert!(o.is_deny());
        assert_eq!(o.rule(), "empty_command");
    }

    #[test]
    fn whitespace_only_command_denies() {
        let p = policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec!["ls".into()]),
                ..Default::default()
            }),
            ..Default::default()
        });
        let o = check_command(&req("    \t"), &p, Mode::Gated);
        assert!(o.is_deny());
    }

    #[test]
    fn command_line_is_trimmed_before_deny_always() {
        let p = policy_with(GatedMode {
            allow_pattern: Some(vec![AllowPattern {
                command: "git".into(),
                args: Some(vec!["status".into()]),
                deny_if_arg_contains: None,
            }]),
            ..Default::default()
        });
        let o = check_command(&req("  git status  "), &p, Mode::Gated);
        assert!(o.is_allow());
    }

    // -- regex cache --------------------------------------------------------

    #[test]
    fn regex_cache_hot_loop_does_not_explode() {
        let p = policy_with(GatedMode {
            allow: Some(AllowSection {
                commands: Some(vec!["echo".into()]),
                ..Default::default()
            }),
            deny_always: Some(DenyAlways {
                substrings: None,
                regex: Some(vec![r"\bnode\s+-e\b".into()]),
            }),
            ..Default::default()
        });
        for _ in 0..100 {
            let _ = check_command(&req("echo hi"), &p, Mode::Gated);
            let _ = check_command(&req("node -e 'x'"), &p, Mode::Gated);
        }
    }
}
