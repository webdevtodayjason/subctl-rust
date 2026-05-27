//! Memory-specific error helpers.
//!
//! Like `evy-scheduler`, this crate funnels its failures into the
//! workspace [`evy_core::Error`] enum rather than introducing a parallel
//! `Error` type. Callers keep a single `Result<T>` shape across the
//! workspace.
//!
//! - sqlx / migrate failures → [`Error::Io`] wrapping a
//!   [`std::io::Error`] with the original sqlx message preserved.
//! - playbook frontmatter / yaml parse failures → [`Error::InvalidMandate`]
//!   (closest-fit until `evy-core` grows a dedicated variant — see
//!   REPORT BACK in the slice 2C report).
//! - bad uuid strings read out of sqlite → [`Error::InvalidMandate`].

use std::io;

use evy_core::Error;

/// Wrap a sqlx error as an io error so it threads through `Error::Io`.
pub(crate) fn from_sqlx(err: sqlx::Error) -> Error {
    Error::Io(io::Error::other(format!("sqlx: {err}")))
}

/// Wrap a sqlx::migrate::MigrateError the same way.
pub(crate) fn from_migrate(err: sqlx::migrate::MigrateError) -> Error {
    Error::Io(io::Error::other(format!("migrate: {err}")))
}

/// Bad data found inside an observation/playbook row or file.
pub(crate) fn bad_row(reason: impl std::fmt::Display) -> Error {
    Error::InvalidMandate(format!("memory row: {reason}"))
}

/// Bad data found inside a playbook frontmatter block.
pub(crate) fn bad_playbook(path: impl std::fmt::Debug, reason: impl std::fmt::Display) -> Error {
    Error::InvalidMandate(format!("playbook {path:?}: {reason}"))
}

/// Wrap a reqwest transport error as an io error so it threads through
/// `Error::Io`. Used by the Cognee HTTP client.
pub(crate) fn from_reqwest(err: reqwest::Error) -> Error {
    Error::Io(io::Error::other(format!("cognee http: {err}")))
}

/// Wrap an unexpected Cognee response (non-2xx, body parse failure) as
/// an io error.
pub(crate) fn cognee_status(status: u16, body: impl std::fmt::Display) -> Error {
    Error::Io(io::Error::other(format!("cognee http {status}: {body}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_sqlx_yields_io_variant() {
        let err = sqlx::Error::RowNotFound;
        assert!(matches!(from_sqlx(err), Error::Io(_)));
    }

    #[test]
    fn bad_row_carries_reason() {
        let s = bad_row("garbage uuid `xyz`").to_string();
        assert!(s.contains("garbage uuid"));
    }

    #[test]
    fn bad_playbook_carries_path() {
        let s = bad_playbook("/tmp/foo.md", "missing triggers").to_string();
        assert!(s.contains("foo.md"));
        assert!(s.contains("missing triggers"));
    }
}
