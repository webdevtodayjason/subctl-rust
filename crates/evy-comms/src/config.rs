//! HTTP listener configuration.
//!
//! Settings the operator (or the daemon's config layer) supplies to
//! [`crate::HttpServer::new`]. Defaults mirror v3's dashboard binding
//! (`127.0.0.1:8787`) so existing curl recipes keep working.

use serde::{Deserialize, Serialize};

/// Default bind host. Loopback only — the HTTP surface is the operator's
/// console, not a public API.
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// Default bind port. Matches v3 dashboard so muscle memory still works.
pub const DEFAULT_PORT: u16 = 8787;

/// HTTP listener configuration.
///
/// `host` is a string so the daemon can pass either an IP literal or
/// `"localhost"`; resolution happens at bind time. `allow_origins` is
/// the explicit CORS allowlist — empty disables CORS entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Bind host. Default `"127.0.0.1"`.
    pub host: String,
    /// Bind port. Default `8787` (matches v3 dashboard).
    pub port: u16,
    /// CORS allowlist. Default `["http://localhost:8787"]`.
    pub allow_origins: Vec<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
            allow_origins: vec![format!("http://{DEFAULT_HOST}:{DEFAULT_PORT}")],
        }
    }
}

impl HttpConfig {
    /// Convenience: build a config bound to an ephemeral port (port 0)
    /// with no CORS allowlist. Used by integration tests so they can
    /// learn the actual port after `bind`.
    #[must_use]
    pub fn ephemeral() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: 0,
            allow_origins: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_v3_dashboard() {
        let c = HttpConfig::default();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 8787);
        assert_eq!(c.allow_origins, vec!["http://127.0.0.1:8787".to_owned()]);
    }

    #[test]
    fn ephemeral_uses_port_zero_and_empty_origins() {
        let c = HttpConfig::ephemeral();
        assert_eq!(c.port, 0);
        assert!(c.allow_origins.is_empty());
    }

    #[test]
    fn roundtrips_through_serde_json() {
        let c = HttpConfig::default();
        let s = serde_json::to_string(&c).unwrap();
        let back: HttpConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.host, c.host);
        assert_eq!(back.port, c.port);
        assert_eq!(back.allow_origins, c.allow_origins);
    }
}
