//! Byte-compatibility tests against v3 `components/evy/trust-marker.ts`.
//!
//! The fixtures in `tests/hmac-fixtures/*.json` were captured by running
//! v3's `buildSignedDirective` on known `(secret_hex, phase, ts, body)`
//! triplets — see the capture script in the slice-2A worker report.
//!
//! Each fixture pins:
//! - the SPEC-wrapped signed body (every `\n` indented by two spaces)
//! - the 16-hex truncated HMAC-SHA256 of `phase + "\n" + ts + "\n" + signedBody`
//! - the marker bracket line (with or without the optional `phase=` field)
//! - the full wire format = `marker + "\n" + signedBody`
//!
//! If any of these assertions fails, a v3 worker spawned with the team
//! contract would refuse a directive produced by this code — which is the
//! whole reason the HMAC layer exists. Treat a fixture-test failure as a
//! wire-protocol break and coordinate with the dashboard / worker side
//! before regenerating.

use std::fs;
use std::path::PathBuf;

use evy_providers::hmac::{HmacKey, TrustMarker};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Fixture {
    secret_hex: String,
    phase: Option<String>,
    ts: String,
    body: String,
    hmac: String,
    signed_body: String,
    marker: String,
    wire_format: String,
}

fn load(name: &str) -> Fixture {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/hmac-fixtures")
        .join(name);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse fixture {}: {e}", path.display()))
}

fn assert_fixture(fixture_file: &str) {
    let f = load(fixture_file);
    let key = HmacKey::from_hex(&f.secret_hex)
        .unwrap_or_else(|e| panic!("fixture {fixture_file} has invalid key: {e}"));
    let marker = TrustMarker::with_ts(f.body.clone(), f.phase.clone(), f.ts.clone(), &key);

    assert_eq!(
        marker.hmac, f.hmac,
        "fixture {fixture_file}: HMAC byte mismatch",
    );
    assert_eq!(
        marker.marker_line(),
        f.marker,
        "fixture {fixture_file}: marker bracket line byte mismatch",
    );
    assert_eq!(
        marker.to_directive_string(),
        f.wire_format,
        "fixture {fixture_file}: wire format byte mismatch",
    );

    // Self-verify path — the marker we built should verify against the
    // same key. Tests both the construction side and the verifier side.
    marker
        .verify(&key)
        .unwrap_or_else(|e| panic!("fixture {fixture_file}: self-verify failed: {e}"));

    // Spot-check the signed-body shape too, in case wrap_spec ever drifts.
    let actual_directive = marker.to_directive_string();
    let body_part = actual_directive
        .split_once('\n')
        .expect("directive has marker + body separator")
        .1;
    assert_eq!(
        body_part, f.signed_body,
        "fixture {fixture_file}: signed_body byte mismatch",
    );
}

#[test]
fn fixture_01_with_phase_matches_v3_bytes() {
    assert_fixture("fixture-01-with-phase.json");
}

#[test]
fn fixture_02_no_phase_multiline_matches_v3_bytes() {
    assert_fixture("fixture-02-no-phase-multiline.json");
}

#[test]
fn fixture_03_zero_key_matches_v3_bytes() {
    assert_fixture("fixture-03-zero-key.json");
}
