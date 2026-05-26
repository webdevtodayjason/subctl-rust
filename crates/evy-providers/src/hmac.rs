//! HMAC trust marker (ADR 0011 Layer 1) — byte-compatible Rust port of
//! v3 `components/evy/trust-marker.ts`.
//!
//! # Why this exists
//!
//! ADR 0002 introduced a plaintext directive marker that the dashboard
//! prepended to every supervisor message. It was identified as gameable
//! within hours of shipping: anything that can write to the worker's
//! tmux pane can forge the marker. ADR 0011 fixed it with an HMAC-
//! authenticated marker that workers verify by recomputing against a
//! per-team shared secret embedded in their spawn-time system prompt.
//!
//! # Wire format
//!
//! ```text
//! [subctl-master directive · phase=<phase> · ts:<iso> · hmac:<hmac16>]
//! SPEC:
//!   <every line of the operator body indented two spaces>
//! ```
//!
//! The phase field is optional; when absent, the marker collapses to
//! `[subctl-master directive · ts:<iso> · hmac:<hmac16>]`.
//!
//! `hmac` is the first 16 hex chars (= 8 bytes) of
//! `HMAC-SHA256(secret, phase + "\n" + ts + "\n" + signedBody)`, where
//! `signedBody = "SPEC:\n  " + body.split("\n").join("\n  ")`. The body
//! itself is NOT embedded in the marker — the caller writes
//! `marker + "\n" + signedBody` to the worker pane.
//!
//! # Why the wire identifier stays `subctl-master`
//!
//! Workers in-flight when the daemon restarts would refuse new directives
//! if the identifier string changed. v3's trust-marker.ts comment makes
//! this explicit: a future version-negotiated rollout can introduce
//! `subctl-evy`, but Phase 2 keeps the legacy wire identity unchanged.
//!
//! # Why byte-compatibility matters
//!
//! v3 workers spawned with the team contract MUST be able to verify
//! envelopes produced by this code. The fixtures in
//! `crates/evy-providers/tests/hmac-fixtures/*.json` were captured from
//! v3's `buildSignedDirective` for known inputs; `tests/hmac_fixtures.rs`
//! re-derives the MAC + wire format and asserts byte equality.
//!
//! # Secret hygiene
//!
//! The key value MUST NOT appear in logs, telemetry, audit lines, or
//! stderr. [`HmacKey`] deliberately does NOT implement `Debug`; instead
//! it implements a redacted variant that prints `HmacKey(<redacted>)`.

use std::sync::OnceLock;

use chrono::SecondsFormat;
use hmac::{Hmac, KeyInit, Mac};
use rand::Rng;
use sha2::Sha256;

use evy_core::{Error, ProviderKind, Result};

type HmacSha256 = Hmac<Sha256>;

/// Number of hex chars retained from the HMAC-SHA256 output.
///
/// 16 hex chars = 8 bytes. Matches v3 trust-marker.ts (`HMAC_TRUNCATE_HEX`).
/// ADR 0011 §"Reasoning": 8 bytes is ample integrity for the threat model
/// (forging requires either reading the secret off disk or guessing a 64-bit
/// MAC per attempt; the worker rate-limits as a side effect of being a slow
/// language-model loop).
const HMAC_TRUNCATE_HEX: usize = 16;

/// Wire-protocol identifier embedded in every marker.
///
/// **Do NOT rename without version negotiation.** Workers in-flight when
/// the daemon restarts would reject new directives if this string changes.
/// See module-level rustdoc.
const WIRE_IDENT: &str = "subctl-master directive";

/// SPEC prefix prepended to the body before signing.
///
/// Centralizing the SPEC wrap means every emitter that goes through this
/// module inherits the requirement for free — no caller has to remember
/// to prepend "SPEC:". The HMAC mechanism proves WHO; the SPEC block
/// proves WHAT. A signed marker with an empty/missing SPEC is a contract
/// violation, not a hint to look elsewhere for the task body.
const SPEC_PREFIX: &str = "SPEC:\n  ";

/// Two-space indent applied to every line break in the body before signing.
///
/// Bytes are part of the HMAC input — the worker MUST receive the same
/// bytes master signed, including indentation.
const SPEC_INDENT: &str = "\n  ";

// ─── HmacKey ─────────────────────────────────────────────────────────────

/// Per-session 32-byte HMAC key.
///
/// Phase 2 holds this in memory only — the daemon generates a fresh key
/// at boot via [`HmacKey::generate`] and threads it through provider
/// configs. Filesystem persistence (matching v3's
/// `~/.local/state/subctl/teams/<team_id>/hmac.secret`) is Phase 3
/// hardening; the in-memory model is intentionally weaker so a
/// daemon restart invalidates all in-flight worker contracts.
///
/// `Clone` is intentional — providers each hold their own copy; the key
/// material is fungible byte-by-byte. `Debug` is intentionally NOT
/// implemented (use the manual impl that prints `HmacKey(<redacted>)`).
#[derive(Clone)]
pub struct HmacKey([u8; 32]);

impl HmacKey {
    /// Mint a cryptographically random 32-byte key.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        // `rand::rng()` returns a ThreadRng, which is a CSPRNG (ChaCha12
        // seeded from the OS, re-seeded periodically). Documented as
        // suitable for cryptographic key material. See rand 0.10 docs:
        // `ThreadRng` implements `CryptoRng` + `Rng`.
        rand::rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Construct from a 64-char lowercase-hex string. Used by the fixture
    /// tests and any future operator-supplied key path.
    ///
    /// # Errors
    /// Returns `Error::InvalidMandate` if the input isn't exactly 64 hex
    /// chars (32 bytes). We tag with `InvalidMandate` rather than a new
    /// variant because the only failure path is operator-supplied malformed
    /// input — there is no provider involved.
    pub fn from_hex(hex_str: &str) -> Result<Self> {
        let bytes = hex::decode(hex_str.trim())
            .map_err(|e| Error::InvalidMandate(format!("HMAC key is not valid hex: {e}")))?;
        let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            Error::InvalidMandate(format!(
                "HMAC key must be exactly 32 bytes (64 hex chars); got {} bytes",
                v.len()
            ))
        })?;
        Ok(Self(arr))
    }

    /// Raw key material. Callers MUST NOT log or echo the return value.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Lowercase-hex encoding. Callers MUST NOT log or echo the return
    /// value. Exists for legitimate transport paths (worker spawn-time
    /// prompt injection, fixture round-trips) only.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Debug for HmacKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted — protects against accidental `tracing::debug!(?key)`
        // calls that would otherwise leak the secret into logs.
        f.write_str("HmacKey(<redacted>)")
    }
}

// ─── TrustMarker ─────────────────────────────────────────────────────────

/// A built HMAC-authenticated directive envelope, ready to paste into the
/// worker pane.
///
/// Construct via [`TrustMarker::new`] (auto-timestamps to now) or
/// [`TrustMarker::with_ts`] (caller supplies an ISO timestamp, used by
/// tests for determinism). [`Self::to_directive_string`] returns the
/// wire-format bytes the worker receives.
///
/// Field shape mirrors v3 `trust-marker.ts` — see module-level rustdoc.
#[derive(Debug, Clone)]
pub struct TrustMarker {
    /// Optional phase string. `None` collapses the marker to the
    /// no-phase form.
    pub phase: Option<String>,
    /// ISO 8601 timestamp baked into the marker. v3 emits
    /// `<date>T<time>.<millis>Z` via `new Date().toISOString()`; this
    /// crate matches that shape via chrono's
    /// `to_rfc3339_opts(SecondsFormat::Millis, true)`.
    pub ts: String,
    /// The operator's raw body. NOT embedded in the marker line — when
    /// the wire format is rendered, the body is SPEC-wrapped and appended
    /// after the marker.
    pub body: String,
    /// First 16 hex chars of HMAC-SHA256 over `phase + "\n" + ts + "\n" + signedBody`.
    pub hmac: String,
}

impl TrustMarker {
    /// Build a trust marker with `ts` set to "now" (ISO 8601, ms precision).
    ///
    /// `phase` empty / whitespace-only collapses to `None` to match v3's
    /// `buildDirectiveMarker` normalization.
    #[must_use]
    pub fn new(body: String, phase: Option<String>, key: &HmacKey) -> Self {
        let ts = now_iso();
        Self::with_ts(body, phase, ts, key)
    }

    /// Build a trust marker with an explicit timestamp. Used by tests
    /// for deterministic golden-fixture comparisons.
    #[must_use]
    pub fn with_ts(body: String, phase: Option<String>, ts: String, key: &HmacKey) -> Self {
        let phase_norm = normalize_phase(phase);
        let signed_body = wrap_spec(&body);
        let hmac = compute_hmac(key, phase_norm.as_deref().unwrap_or(""), &ts, &signed_body);
        Self {
            phase: phase_norm,
            ts,
            body,
            hmac,
        }
    }

    /// Render the wire-format string (`marker + "\n" + signed_body`).
    ///
    /// This is the value that gets pasted into the worker's tmux pane.
    /// Deterministic, allocation-bounded, no I/O.
    #[must_use]
    pub fn to_directive_string(&self) -> String {
        let marker = self.marker_line();
        let signed_body = wrap_spec(&self.body);
        format!("{marker}\n{signed_body}")
    }

    /// Render just the marker bracket line, without the SPEC body. Public
    /// because some future caller (audit log, verifier diagnostic) may
    /// want to inspect the bracket without the body in the same allocation.
    #[must_use]
    pub fn marker_line(&self) -> String {
        match &self.phase {
            Some(p) => format!(
                "[{wire} · phase={p} · ts:{ts} · hmac:{hmac}]",
                wire = WIRE_IDENT,
                ts = self.ts,
                hmac = self.hmac,
            ),
            None => format!(
                "[{wire} · ts:{ts} · hmac:{hmac}]",
                wire = WIRE_IDENT,
                ts = self.ts,
                hmac = self.hmac,
            ),
        }
    }

    /// Recompute the MAC from `(key, self.phase, self.ts, self.body)` and
    /// compare to `self.hmac`. Returns `Ok(())` on match.
    ///
    /// # Errors
    /// Returns `Error::Provider { kind: ClaudeCode, … }` on mismatch.
    /// Used by tests today; reserved as the canonical worker-side check
    /// for future native-language workers (Claude Code workers perform the
    /// check via in-prompt model reasoning per ADR 0011 Layer 1).
    pub fn verify(&self, key: &HmacKey) -> Result<()> {
        let signed_body = wrap_spec(&self.body);
        let expected = compute_hmac(
            key,
            self.phase.as_deref().unwrap_or(""),
            &self.ts,
            &signed_body,
        );
        if constant_time_eq(expected.as_bytes(), self.hmac.as_bytes()) {
            Ok(())
        } else {
            Err(Error::Provider {
                // ClaudeCode is the canonical caller; the kind is only
                // for error tagging, not behavior. Future per-provider
                // wrappers can override.
                kind: ProviderKind::ClaudeCode,
                reason: "HMAC verification failed".to_string(),
            })
        }
    }
}

// ─── pure helpers ────────────────────────────────────────────────────────

/// Normalize the phase string to match v3 `buildDirectiveMarker`:
/// trim whitespace, empty → `None`.
fn normalize_phase(phase: Option<String>) -> Option<String> {
    phase.and_then(|p| {
        let trimmed = p.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// Apply the SPEC wrap + two-space indent. Matches v3:
/// `signedBody = "SPEC:\n  " + body.split("\n").join("\n  ")`.
fn wrap_spec(body: &str) -> String {
    let mut out = String::with_capacity(SPEC_PREFIX.len() + body.len() + 16);
    out.push_str(SPEC_PREFIX);
    // body.split("\n").join("\n  ") in JS = same as Rust's replace,
    // because we want EVERY "\n" replaced, not just line boundaries.
    out.push_str(&body.replace('\n', SPEC_INDENT));
    out
}

/// Compute the truncated HMAC tag over `phase + "\n" + ts + "\n" + signed_body`.
/// Returns the first 16 hex chars (lowercase) of HMAC-SHA256.
///
/// # Byte-compatibility note
///
/// v3's `trust-marker.ts` calls Node's `createHmac("sha256", secret)`
/// where `secret` is the 64-char hex *string* read from disk. Node
/// interprets a string secret as its UTF-8 byte representation, so the
/// HMAC key on the wire is the 64 ASCII bytes of the hex digits, NOT the
/// 32 decoded bytes of entropy. We key the Rust HMAC with `key.to_hex()`
/// for the same reason — without this, every fixture mismatches by 16
/// hex chars even though the protocol structure is identical. The
/// `tests/hmac_fixtures.rs` fixtures hard-pin this behaviour.
fn compute_hmac(key: &HmacKey, phase: &str, ts: &str, signed_body: &str) -> String {
    let key_bytes = key.to_hex();
    // `new_from_slice` accepts any key length for HMAC (no panicking
    // upper bound), so this is infallible for our 64-byte input.
    let mut mac =
        HmacSha256::new_from_slice(key_bytes.as_bytes()).expect("HMAC accepts any key length");
    mac.update(phase.as_bytes());
    mac.update(b"\n");
    mac.update(ts.as_bytes());
    mac.update(b"\n");
    mac.update(signed_body.as_bytes());
    let full = hex::encode(mac.finalize().into_bytes());
    full[..HMAC_TRUNCATE_HEX].to_string()
}

/// ISO 8601 timestamp at millisecond precision, UTC, trailing `Z`. Matches
/// v3's `new Date().toISOString()` byte-for-byte for any wall-clock instant.
fn now_iso() -> String {
    // chrono produces e.g. "2026-05-26T17:00:00.123Z" for SecondsFormat::Millis
    // + use_z=true, which mirrors JS's toISOString shape.
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Constant-time byte slice comparison. Operates on equal-length inputs
/// (asymmetric lengths short-circuit to `false`, which is fine — the
/// HMAC field is fixed-length by construction).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─── lazy default key (test/dev convenience) ─────────────────────────────

static DEFAULT_KEY: OnceLock<HmacKey> = OnceLock::new();

/// Process-global default HMAC key, lazily initialized.
///
/// Phase 2 simplification: provider adapters that don't yet thread a key
/// through their config fall back to this. Real daemon code passes a
/// session-scoped key explicitly via [`crate::config::ClaudeCodeConfig::hmac_key`]
/// / [`crate::config::CodexConfig::hmac_key`]. The default exists so unit
/// tests don't need to construct one — there is no production code path
/// that uses it without an explicit override.
pub fn default_key() -> &'static HmacKey {
    DEFAULT_KEY.get_or_init(HmacKey::generate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key() -> HmacKey {
        HmacKey::from_hex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap()
    }

    #[test]
    fn key_roundtrips_through_hex() {
        let k = fixed_key();
        let again = HmacKey::from_hex(&k.to_hex()).unwrap();
        assert_eq!(k.as_bytes(), again.as_bytes());
    }

    #[test]
    fn key_debug_is_redacted() {
        let k = HmacKey::generate();
        let s = format!("{k:?}");
        assert_eq!(s, "HmacKey(<redacted>)", "Debug must not leak key material");
    }

    #[test]
    fn key_from_hex_rejects_short_input() {
        let err = HmacKey::from_hex("dead").unwrap_err();
        assert!(err.to_string().contains("32 bytes"), "got: {err}");
    }

    #[test]
    fn key_from_hex_rejects_non_hex() {
        let err = HmacKey::from_hex("zz".repeat(32).as_str()).unwrap_err();
        assert!(err.to_string().contains("hex"), "got: {err}");
    }

    #[test]
    fn key_generate_is_random() {
        let a = HmacKey::generate();
        let b = HmacKey::generate();
        assert_ne!(a.as_bytes(), b.as_bytes(), "two fresh keys must differ");
    }

    #[test]
    fn empty_phase_normalizes_to_none() {
        assert_eq!(normalize_phase(Some("".into())), None);
        assert_eq!(normalize_phase(Some("   ".into())), None);
        assert_eq!(normalize_phase(None), None);
        assert_eq!(normalize_phase(Some("  ok  ".into())), Some("ok".into()));
    }

    #[test]
    fn wrap_spec_indents_every_line() {
        let s = wrap_spec("line one\nline two\nline three");
        assert_eq!(s, "SPEC:\n  line one\n  line two\n  line three");
    }

    #[test]
    fn wrap_spec_single_line() {
        assert_eq!(wrap_spec("hello"), "SPEC:\n  hello");
    }

    #[test]
    fn wrap_spec_empty() {
        // Empty body still gets the SPEC: header — v3 emits the same
        // shape. The two-space indent on an empty string is "" so the
        // result is "SPEC:\n  ".
        assert_eq!(wrap_spec(""), "SPEC:\n  ");
    }

    #[test]
    fn marker_with_phase_has_phase_field() {
        let key = fixed_key();
        let m = TrustMarker::with_ts(
            "body".into(),
            Some("bootstrap".into()),
            "2026-05-26T17:00:00.000Z".into(),
            &key,
        );
        let line = m.marker_line();
        assert!(line.starts_with("[subctl-master directive · phase=bootstrap · ts:"));
        assert!(line.ends_with(']'));
    }

    #[test]
    fn marker_without_phase_skips_phase_field() {
        let key = fixed_key();
        let m = TrustMarker::with_ts("body".into(), None, "2026-05-26T17:00:00.000Z".into(), &key);
        let line = m.marker_line();
        assert!(!line.contains("phase="));
        assert!(line.starts_with("[subctl-master directive · ts:"));
    }

    #[test]
    fn directive_string_is_marker_then_signed_body() {
        let key = fixed_key();
        let m = TrustMarker::with_ts(
            "hello".into(),
            Some("p".into()),
            "2026-05-26T17:00:00.000Z".into(),
            &key,
        );
        let s = m.to_directive_string();
        // Two lines: marker, then "SPEC:\n  hello"
        let mut lines = s.lines();
        let first = lines.next().unwrap();
        assert!(first.starts_with("[subctl-master directive ·"));
        assert_eq!(lines.next().unwrap(), "SPEC:");
        assert_eq!(lines.next().unwrap(), "  hello");
    }

    #[test]
    fn verify_accepts_self_built_marker() {
        let key = fixed_key();
        let m = TrustMarker::new("body".into(), Some("p".into()), &key);
        m.verify(&key).expect("self-verify must succeed");
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let k1 = fixed_key();
        let k2 =
            HmacKey::from_hex("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
                .unwrap();
        let m = TrustMarker::new("body".into(), Some("p".into()), &k1);
        assert!(m.verify(&k2).is_err());
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let key = fixed_key();
        let mut m = TrustMarker::new("body".into(), Some("p".into()), &key);
        m.body.push_str(" tampered");
        assert!(m.verify(&key).is_err());
    }

    #[test]
    fn verify_rejects_tampered_phase() {
        let key = fixed_key();
        let mut m = TrustMarker::new("body".into(), Some("p".into()), &key);
        m.phase = Some("different".into());
        assert!(m.verify(&key).is_err());
    }

    #[test]
    fn verify_rejects_tampered_ts() {
        let key = fixed_key();
        let mut m = TrustMarker::new("body".into(), Some("p".into()), &key);
        m.ts.push('X');
        assert!(m.verify(&key).is_err());
    }

    #[test]
    fn compute_hmac_is_truncated_to_16_chars() {
        let key = HmacKey::from_hex(&"0".repeat(64)).unwrap();
        let h = compute_hmac(&key, "", "x", "y");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn now_iso_has_millis_and_z_suffix() {
        let s = now_iso();
        // Sample: "2026-05-26T17:00:00.123Z"
        assert!(s.ends_with('Z'), "must end with Z: got {s}");
        assert!(s.contains('.'), "must include ms separator: got {s}");
        let dot = s.find('.').unwrap();
        // .NNNZ → 4 chars between '.' and end
        assert_eq!(s.len() - dot, 5, "ms must be 3 digits: got {s}");
    }

    #[test]
    fn constant_time_eq_handles_unequal_lengths() {
        assert!(!constant_time_eq(b"a", b"bb"));
    }

    #[test]
    fn constant_time_eq_detects_one_bit_difference() {
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(constant_time_eq(b"abc", b"abc"));
    }

    #[test]
    fn default_key_is_stable_within_process() {
        let a = default_key().as_bytes().to_vec();
        let b = default_key().as_bytes().to_vec();
        assert_eq!(a, b, "default_key must memoize");
    }
}
