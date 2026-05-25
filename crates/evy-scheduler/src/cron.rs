//! Cron expression parsing.
//!
//! Operators write 5-field standard cron syntax (`minute hour day month
//! weekday`). The upstream `cron` crate parses 6- or 7-field expressions
//! (seconds + standard + optional year), so we normalize 5-field input to
//! 7-field by prepending `0` (seconds = 0) and appending `*` (year = any)
//! before handing it to `cron::Schedule`.
//!
//! Phase 1 deliberately rejects 6- and 7-field inputs to keep the
//! operator-facing surface narrow. Extending later only needs a new
//! validation branch here.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use cron::Schedule;
use evy_core::Result;

use crate::error;

/// Validate a 5-field cron expression. Returns `Ok(())` if the expression
/// parses cleanly; an `Error::InvalidMandate` otherwise.
pub fn validate(expr: &str) -> Result<()> {
    let _ = parse_5_field(expr)?;
    Ok(())
}

/// Compute the next fire time strictly after `from`.
///
/// Returns `Error::InvalidMandate` if `expr` is not a valid 5-field cron;
/// returns `None` if the expression has no further fire times (the upstream
/// crate signals this by exhausting the iterator — very rare in practice
/// for unrestricted year ranges, but we propagate honestly).
pub fn next_fire_after(expr: &str, from: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let schedule = parse_5_field(expr)?;
    Ok(schedule.after(&from).next())
}

fn parse_5_field(expr: &str) -> Result<Schedule> {
    let trimmed = expr.trim();
    let fields = trimmed.split_whitespace().count();
    if fields != 5 {
        return Err(error::invalid_cron(
            expr,
            format!("expected 5 fields (minute hour day month weekday), got {fields}"),
        ));
    }
    let seven = format!("0 {trimmed} *");
    Schedule::from_str(&seven).map_err(|e| error::invalid_cron(expr, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn validate_accepts_every_minute() {
        validate("* * * * *").expect("every minute should parse");
    }

    #[test]
    fn validate_rejects_six_fields() {
        let err = validate("*/30 * * * * *").unwrap_err();
        assert!(err.to_string().contains("got 6"));
    }

    #[test]
    fn validate_rejects_garbage() {
        let err = validate("definitely not cron").unwrap_err();
        assert!(err.to_string().contains("invalid cron"));
    }

    #[test]
    fn next_fire_after_advances_past_from() {
        let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let next = next_fire_after("* * * * *", from)
            .expect("parse")
            .expect("has next");
        assert!(next > from);
        // every-minute schedule with seconds=0 should fire at the next
        // minute boundary, i.e. 00:01:00.
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 0).unwrap());
    }

    #[test]
    fn hourly_schedule_lands_on_hour() {
        let from = Utc.with_ymd_and_hms(2026, 1, 1, 12, 30, 0).unwrap();
        let next = next_fire_after("0 * * * *", from)
            .expect("parse")
            .expect("has next");
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 13, 0, 0).unwrap());
    }
}
