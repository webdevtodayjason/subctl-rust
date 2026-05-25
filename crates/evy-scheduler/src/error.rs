//! Scheduler-specific error helpers.
//!
//! The workspace error type is [`evy_core::Error`]; the scheduler does not
//! introduce a parallel `Error` enum. Instead, these helpers funnel
//! scheduler-shaped failures (cron-parse, sqlx, missing-job) into the
//! existing variants so callers keep a single `Result<T>` type.
//!
//! - cron expression problems → [`Error::InvalidMandate`] (closest fit
//!   today; a dedicated `InvalidCron` variant on `evy-core` is requested
//!   in the slice-C report and can replace this later without breaking
//!   call sites).
//! - sqlx / migrate failures → [`Error::Io`] wrapping an
//!   [`std::io::Error`] with the original sqlx message preserved.
//! - missing-job-id → [`Error::InvalidMandate`].

use std::io;

use evy_core::Error;

/// Wrap a cron-parse failure as the workspace error.
pub(crate) fn invalid_cron(expr: &str, reason: impl std::fmt::Display) -> Error {
    Error::InvalidMandate(format!("invalid cron `{expr}`: {reason}"))
}

/// Wrap a sqlx error as an io error so it threads through `Error::Io`.
pub(crate) fn from_sqlx(err: sqlx::Error) -> Error {
    Error::Io(io::Error::other(format!("sqlx: {err}")))
}

/// Wrap a sqlx::migrate::MigrateError the same way.
pub(crate) fn from_migrate(err: sqlx::migrate::MigrateError) -> Error {
    Error::Io(io::Error::other(format!("migrate: {err}")))
}

/// "No job with this id" — close enough to `InvalidMandate` for now.
pub(crate) fn job_not_found(id: impl std::fmt::Debug) -> Error {
    Error::InvalidMandate(format!("job not found: {id:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_cron_carries_expression() {
        let e = invalid_cron("not a cron", "garbage");
        let s = e.to_string();
        assert!(s.contains("not a cron"));
        assert!(s.contains("garbage"));
    }

    #[test]
    fn from_sqlx_yields_io_variant() {
        let err = sqlx::Error::RowNotFound;
        assert!(matches!(from_sqlx(err), Error::Io(_)));
    }
}
