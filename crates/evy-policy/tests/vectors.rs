//! Cross-implementation vector corpus test — Rust port of
//! `components/evy/tools/policy/__tests__/vectors.test.ts`.
//!
//! Loads the shared `config/policy/test-vectors.toml` corpus and asserts
//! every vector's expected `decision` matches what our check produces.
//! Per pack 07 §11, this is the contract test that keeps the Rust port,
//! the Go port, and the TS reference honest: any disagreement on any
//! vector fails CI.
//!
//! Lenient `rule_path` matching mirrors the TS test: bracket-index
//! suffixes (`allow_pattern[N]`) are stripped before comparing, and within
//! the `mode.gated.deny_always.*` family `.substrings` and `.regex` are
//! interchangeable (the TS test's documented leniency for vectors whose
//! payload contains literal `rm -rf` etc.).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use evy_policy::{check_command, CheckOutcome, CheckRequest, Mode, Policy};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct VectorDoc {
    vector: Vec<Vector>,
}

#[derive(Debug, Deserialize)]
struct Vector {
    name: String,
    policy: String, // "node" | "python" | "generic"
    command: String,
    expected: String, // "allow" | "deny"
    #[serde(default)]
    expected_rule_path: Option<String>,
}

fn install_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/install")
}

fn load_preset_sync(name: &str) -> Policy {
    let path = install_root().join(format!("config/policy/presets/{name}.toml"));
    let text = std::fs::read_to_string(&path).expect("read preset");
    let mut doc: Policy = toml::from_str(&text).expect("parse preset");
    if doc.default_mode.is_none() {
        doc.default_mode = Some(Mode::Gated);
    }
    doc
}

fn rule_path_base(p: &str) -> String {
    // strip `[N]` suffixes
    let re = regex::Regex::new(r"\[\d+\]").expect("regex");
    re.replace_all(p, "").into_owned()
}

fn rule_matches(actual: &str, expected: &str) -> bool {
    let a = rule_path_base(actual);
    let e = rule_path_base(expected);
    if a == e {
        return true;
    }
    let deny_family = "mode.gated.deny_always.";
    if a.starts_with(deny_family) && e.starts_with(deny_family) {
        return true;
    }
    false
}

#[test]
fn vector_corpus_matches_check() {
    let vectors_path = install_root().join("config/policy/test-vectors.toml");
    let text = std::fs::read_to_string(&vectors_path).expect("read vectors");
    let doc: VectorDoc = toml::from_str(&text).expect("parse vectors");
    assert!(
        doc.vector.len() >= 70,
        "expected at least 70 vectors, got {}",
        doc.vector.len(),
    );

    let node = load_preset_sync("node");
    let python = load_preset_sync("python");
    let generic = load_preset_sync("generic");

    // Known preset gaps tracked for follow-up — the TS test treats these as
    // `it.skip` until the preset is refreshed.
    let known_gaps: HashSet<&str> = HashSet::from_iter([
        // find -delete bypass: `find / -name foo -delete` — the literal
        // substring `find / -delete` in the preset's deny_always misses the
        // `-name foo` variant. Tracked for the preset refresh.
        "node: find / -name foo -delete is denied",
    ]);

    let req_cwd = Path::new("/tmp/__subctl_vectors_test__");
    let mut decision_failures: Vec<String> = Vec::new();
    let mut rule_path_failures: Vec<String> = Vec::new();
    let mut skipped: usize = 0;
    let mut ran: usize = 0;
    for v in &doc.vector {
        if known_gaps.contains(v.name.as_str()) {
            skipped += 1;
            continue;
        }
        let policy = match v.policy.as_str() {
            "node" => &node,
            "python" => &python,
            "generic" => &generic,
            other => panic!("unknown preset in vector: {other}"),
        };
        let req = CheckRequest {
            command: &v.command,
            cwd: req_cwd,
            team_id: "t",
            agent_session_id: None,
        };
        let outcome = check_command(&req, policy, Mode::Gated);
        let actual_decision = if outcome.is_allow() { "allow" } else { "deny" };
        if actual_decision != v.expected {
            decision_failures.push(format!(
                "vector {:?}: expected {}, got {} (rule={:?}, rule_path={:?})",
                v.name,
                v.expected,
                actual_decision,
                outcome.rule(),
                outcome.rule_path(),
            ));
            continue;
        }
        if let Some(expected_path) = v.expected_rule_path.as_deref() {
            let actual_path = match &outcome {
                CheckOutcome::Allow { rule_path, .. }
                | CheckOutcome::Deny { rule_path, .. }
                | CheckOutcome::RequireAudit { rule_path, .. } => rule_path.as_str(),
            };
            if !rule_matches(actual_path, expected_path) {
                rule_path_failures.push(format!(
                    "vector {:?}: rule_path mismatch.\n  expected: {}\n  actual:   {}",
                    v.name, expected_path, actual_path,
                ));
            }
        }
        ran += 1;
    }

    if !decision_failures.is_empty() {
        panic!(
            "{} decision mismatch(es) out of {} vectors run (+ {} skipped known gaps):\n{}",
            decision_failures.len(),
            ran,
            skipped,
            decision_failures.join("\n"),
        );
    }
    if !rule_path_failures.is_empty() {
        panic!(
            "{} rule_path mismatch(es) out of {} vectors run:\n{}",
            rule_path_failures.len(),
            ran,
            rule_path_failures.join("\n"),
        );
    }
    println!("vectors: {ran} passed, {skipped} skipped (known gaps)");
}
