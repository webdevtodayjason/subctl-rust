//! Policy mode for a dispatched mandate.
//!
//! `Trusted` runs without gate, `Gated` runs through the policy gate (the
//! v3 TS behaviour ported by `evy-policy`), `Sealed` is hard-blocked.

use serde::{Deserialize, Serialize};

/// How a mandate is gated before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicyMode {
    /// Bypass the gate entirely; dispatch immediately.
    Trusted,
    /// Run through the policy gate; honour its verdict.
    Gated,
    /// Refuse to dispatch; mandate is hard-blocked.
    Sealed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        for mode in [PolicyMode::Trusted, PolicyMode::Gated, PolicyMode::Sealed] {
            let s = serde_json::to_string(&mode).expect("serialize");
            let back: PolicyMode = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn json_shape_is_variant_name() {
        let s = serde_json::to_string(&PolicyMode::Gated).expect("serialize");
        assert_eq!(s, "\"Gated\"");
    }
}
