//! HTTP listener configuration.
//!
//! Settings the operator (or the daemon's config layer) supplies to
//! [`crate::HttpServer::new`]. Defaults mirror v3's dashboard binding
//! (`127.0.0.1:8787`) so existing curl recipes keep working.

use std::path::PathBuf;

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
/// `static_dir`, if `Some`, points at a directory whose contents are
/// served as the operator console at `GET /` (Phase 4 Slice A).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Bind host. Default `"127.0.0.1"`.
    pub host: String,
    /// Bind port. Default `8787` (matches v3 dashboard).
    pub port: u16,
    /// CORS allowlist. Default `["http://localhost:8787"]`.
    pub allow_origins: Vec<String>,
    /// Optional directory whose contents are served as the operator
    /// console at `GET /` (with `index.html` resolved automatically).
    /// `None` (the default) disables the static surface and leaves the
    /// HTTP server as a JSON/SSE-only API.
    ///
    /// In production the daemon binary resolves this relative to its
    /// install layout; tests typically point at
    /// `concat!(env!("CARGO_MANIFEST_DIR"), "/static")`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_dir: Option<PathBuf>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
            allow_origins: vec![format!("http://{DEFAULT_HOST}:{DEFAULT_PORT}")],
            static_dir: None,
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
            static_dir: None,
        }
    }

    /// Builder: attach a static directory whose contents serve as the
    /// operator console at `GET /`. Returns `self` for chaining.
    ///
    /// ```
    /// use std::path::PathBuf;
    /// use evy_comms::HttpConfig;
    /// let c = HttpConfig::default().with_static_dir("/var/lib/evy/web");
    /// assert_eq!(c.static_dir, Some(PathBuf::from("/var/lib/evy/web")));
    /// ```
    #[must_use]
    pub fn with_static_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.static_dir = Some(dir.into());
        self
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
        assert!(
            c.static_dir.is_none(),
            "default config should not serve static files — daemon opts in",
        );
    }

    #[test]
    fn ephemeral_uses_port_zero_and_empty_origins() {
        let c = HttpConfig::ephemeral();
        assert_eq!(c.port, 0);
        assert!(c.allow_origins.is_empty());
        assert!(c.static_dir.is_none());
    }

    #[test]
    fn with_static_dir_sets_path() {
        let c = HttpConfig::default().with_static_dir("/srv/evy/web");
        assert_eq!(c.static_dir, Some(PathBuf::from("/srv/evy/web")));
    }

    #[test]
    fn with_static_dir_overrides_previous_value() {
        let c = HttpConfig::default()
            .with_static_dir("/one")
            .with_static_dir("/two");
        assert_eq!(c.static_dir, Some(PathBuf::from("/two")));
    }

    #[test]
    fn with_static_dir_chains_on_ephemeral() {
        let c = HttpConfig::ephemeral().with_static_dir("/srv/evy/web");
        assert_eq!(c.port, 0);
        assert_eq!(c.static_dir, Some(PathBuf::from("/srv/evy/web")));
    }

    #[test]
    fn roundtrips_through_serde_json() {
        let c = HttpConfig::default();
        let s = serde_json::to_string(&c).unwrap();
        let back: HttpConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.host, c.host);
        assert_eq!(back.port, c.port);
        assert_eq!(back.allow_origins, c.allow_origins);
        assert_eq!(back.static_dir, c.static_dir);
    }

    #[test]
    fn roundtrips_with_static_dir_through_serde_json() {
        let c = HttpConfig::default().with_static_dir("/srv/evy/web");
        let s = serde_json::to_string(&c).unwrap();
        assert!(
            s.contains("static_dir"),
            "expected static_dir in JSON when Some, got {s}",
        );
        let back: HttpConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.static_dir, c.static_dir);
    }

    #[test]
    fn none_static_dir_is_omitted_from_json() {
        let c = HttpConfig::default();
        let s = serde_json::to_string(&c).unwrap();
        assert!(
            !s.contains("static_dir"),
            "default None should be skipped by serde, got {s}",
        );
    }
}
