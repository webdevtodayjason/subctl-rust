//! Egress redaction floor for the voice renderer.
//!
//! Policy (per the v4 voice spec): if any of the listed secret patterns
//! match the input text, the renderer **refuses** to synthesize. This
//! differs from v3's `redactForEgress` in `components/evy/memory.ts`,
//! which masks-and-passes-through. The v4 voice layer is more
//! conservative because:
//!
//! - TTS output bypasses the chat transcript (audio file, not text), so
//!   a masked rendering would still pronounce "[REDACTED]" or worse
//!   leak the underlying bytes via a partial pattern miss.
//! - Audio cache files persist for 24h on disk; a false-negative would
//!   ship the secret to anyone with cache read access.
//!
//! Returning an [`Error::Redacted`](crate::error::VoiceError::Redacted)
//! makes the policy auditable from caller code.

use std::sync::OnceLock;

use regex::Regex;

use crate::error::{Result, VoiceError};

// We use `std::sync::OnceLock` rather than pulling in `once_cell` to
// keep the dep tree minimal. LazyLock would also work on toolchain
// >= 1.80 but OnceLock is the conservative choice.

/// A single labelled redaction pattern. The label is the only thing the
/// renderer surfaces in `VoiceError::Redacted` — the matched bytes are
/// deliberately not echoed back into logs.
struct Pattern {
    /// Human-readable label included in the error.
    label: &'static str,
    /// Compiled regex.
    regex: Regex,
}

fn patterns() -> &'static [Pattern] {
    static CELL: OnceLock<Vec<Pattern>> = OnceLock::new();
    CELL.get_or_init(|| {
        // Patterns are the four listed in the v4 voice-porter spec.
        // Pattern 1 (`sk-…{20,}`) is a superset of pattern 4
        // (`sk-ant-…`); we list both explicitly so the report-back can
        // cite the exact set the operator approved.
        //
        // NOTE: `sk-` floor is **20+ chars** (not v3's 12) — the spec
        // raised it deliberately so short test fixtures don't trip the
        // gate. Keep this in mind when adding fixtures.
        vec![
            Pattern {
                label: "anthropic-key",
                regex: Regex::new(r"sk-ant-[A-Za-z0-9_-]+").expect("static regex"),
            },
            Pattern {
                label: "sk-key",
                regex: Regex::new(r"sk-[A-Za-z0-9_-]{20,}").expect("static regex"),
            },
            Pattern {
                label: "bearer",
                regex: Regex::new(r"Bearer [A-Za-z0-9_-]+").expect("static regex"),
            },
            Pattern {
                label: "evy-trust",
                // Literal substring (no regex metachars), but expressed
                // as a Regex so the dispatch loop stays uniform.
                regex: Regex::new(r"EVY-TRUST:").expect("static regex"),
            },
        ]
    })
}

/// Check the input text against the egress redaction patterns. Returns
/// `Ok(())` if no pattern fires; otherwise returns `VoiceError::Redacted`
/// with the label of the first match.
///
/// The function is `&str`-only and allocation-free in the happy path.
pub fn check_egress(text: &str) -> Result<()> {
    for p in patterns() {
        if p.regex.is_match(text) {
            return Err(VoiceError::Redacted { pattern: p.label });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_passes() {
        check_egress("hello world, this is evy speaking").expect("clean text must pass");
    }

    #[test]
    fn sk_key_blocks() {
        let e =
            check_egress("my key is sk-abc1234567890abcdef12345 ok").expect_err("sk-* must block");
        match e {
            VoiceError::Redacted { pattern } => assert_eq!(pattern, "sk-key"),
            other => panic!("expected Redacted, got {other:?}"),
        }
    }

    #[test]
    fn short_sk_does_not_block() {
        // Spec floor is 20+ chars; this fixture is 12 — must pass.
        check_egress("short ref sk-abcdef012345 should pass")
            .expect("short sk-* under 20 chars must not block");
    }

    #[test]
    fn anthropic_key_blocks() {
        let e = check_egress("anthro=sk-ant-abc123def456").expect_err("sk-ant- must block");
        match e {
            // Both patterns could fire; `anthropic-key` is checked first.
            VoiceError::Redacted { pattern } => assert_eq!(pattern, "anthropic-key"),
            other => panic!("expected Redacted, got {other:?}"),
        }
    }

    #[test]
    fn bearer_blocks() {
        let e = check_egress("Authorization: Bearer xyz123abc").expect_err("Bearer must block");
        match e {
            VoiceError::Redacted { pattern } => assert_eq!(pattern, "bearer"),
            other => panic!("expected Redacted, got {other:?}"),
        }
    }

    #[test]
    fn evy_trust_blocks() {
        let e = check_egress("inbound EVY-TRUST: 1:abc:def envelope")
            .expect_err("EVY-TRUST: prefix must block");
        match e {
            VoiceError::Redacted { pattern } => assert_eq!(pattern, "evy-trust"),
            other => panic!("expected Redacted, got {other:?}"),
        }
    }

    #[test]
    fn redacted_error_does_not_echo_input() {
        let e = check_egress("Bearer supersecrettoken-xyz").expect_err("Bearer must block");
        let s = e.to_string();
        // The label appears; the matched bytes do not.
        assert!(s.contains("bearer"), "label missing: {s}");
        assert!(
            !s.contains("supersecrettoken"),
            "error must not echo matched secret: {s}"
        );
    }
}
