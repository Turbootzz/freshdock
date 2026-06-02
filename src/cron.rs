//! Hand-rolled 5-field cron parser + next-fire computation (Phase 4, P4-2).
//!
//! Fields: `minute hour day-of-month month day-of-week` (dow 0-6, Sunday = 0).
//! Per-field tokens: `*`, `N`, `A-B`, `A-B/n`, `*/n`, `N/n`, and comma lists.
//! Numeric only — no day/month names. Per PLAN §10 Q3 the parser and scheduler
//! loop are hand-rolled; `chrono` is used only for calendar arithmetic.
//!
//! Day-of-month vs day-of-week follows Vixie cron: when both are restricted the
//! match is their union; if one is `*`, only the other constrains.
//!
//! DST: [`CronExpr::next_after`] steps candidate instants by real time and
//! tests each one's *local* wall-clock fields, so a time in a spring-forward
//! gap (e.g. `30 2 * * *`) never matches that day and fires the next. Behaviour
//! inside a transition hour is timezone-dependent; the 04:00 defaults avoid it.

use chrono::{DateTime, Datelike, TimeDelta, TimeZone, Timelike};

/// Upper bound on minute-stepping in [`CronExpr::next_after`]. Four years of
/// minutes comfortably covers the worst-case recurrence (a `29 2` Feb-29
/// schedule lands within one leap cycle); past it we treat the expression as
/// having no next fire.
const MAX_STEP_MINUTES: u32 = 4 * 366 * 24 * 60;

/// A parsed 5-field cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minute: FieldSet,
    hour: FieldSet,
    dom: FieldSet,
    month: FieldSet,
    dow: FieldSet,
}

/// Why a cron expression failed to parse. `field` names the offending field so
/// the message points the user at the right column.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CronError {
    #[error("expected 5 space-separated fields, got {0}")]
    FieldCount(usize),
    #[error("invalid token `{token}` in {field} field")]
    BadToken { field: &'static str, token: String },
    #[error("value {value} out of range {min}..={max} in {field} field")]
    OutOfRange {
        field: &'static str,
        value: u32,
        min: u32,
        max: u32,
    },
    #[error("range start {start} is greater than end {end} in {field} field")]
    InvertedRange {
        field: &'static str,
        start: u32,
        end: u32,
    },
    #[error("step must be greater than zero in {field} field")]
    ZeroStep { field: &'static str },
}

/// The allowed values of one cron field as a bitmask (bit `i` set ⇒ value `i`
/// matches) plus whether the field was a literal `*` (needed for the Vixie
/// DoM/DoW union rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FieldSet {
    mask: u64,
    star: bool,
}

impl FieldSet {
    fn contains(&self, value: u32) -> bool {
        value < 64 && (self.mask & (1u64 << value)) != 0
    }

    /// Parse one field (e.g. `*/15`, `0`, `9-17`, `1,15,30`) bounded to
    /// `[min, max]`.
    fn parse(field: &str, min: u32, max: u32, name: &'static str) -> Result<Self, CronError> {
        let star = field == "*";
        let mut mask = 0u64;

        for part in field.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((r, s)) => {
                    let step: u32 = s.parse().map_err(|_| CronError::BadToken {
                        field: name,
                        token: part.to_string(),
                    })?;
                    if step == 0 {
                        return Err(CronError::ZeroStep { field: name });
                    }
                    (r, step)
                }
                None => (part, 1),
            };

            let (start, end) = if range == "*" {
                (min, max)
            } else if let Some((a, b)) = range.split_once('-') {
                let a = parse_value(a, min, max, name)?;
                let b = parse_value(b, min, max, name)?;
                if a > b {
                    return Err(CronError::InvertedRange {
                        field: name,
                        start: a,
                        end: b,
                    });
                }
                (a, b)
            } else {
                let v = parse_value(range, min, max, name)?;
                // `N/n` (a single number with a step) means "from N to the
                // field max, stepping by n" — Vixie semantics. A bare `N` is
                // just that one value (step defaults to 1, end == start).
                if step == 1 { (v, v) } else { (v, max) }
            };

            let mut i = start;
            while i <= end {
                mask |= 1u64 << i;
                i += step;
            }
        }

        Ok(FieldSet { mask, star })
    }
}

fn parse_value(tok: &str, min: u32, max: u32, name: &'static str) -> Result<u32, CronError> {
    let v: u32 = tok.parse().map_err(|_| CronError::BadToken {
        field: name,
        token: tok.to_string(),
    })?;
    if v < min || v > max {
        return Err(CronError::OutOfRange {
            field: name,
            value: v,
            min,
            max,
        });
    }
    Ok(v)
}

impl CronExpr {
    /// Parse a standard 5-field cron expression. Fields are split on any run of
    /// ASCII whitespace, so single or multiple spaces between fields are both
    /// accepted.
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError::FieldCount(fields.len()));
        }
        Ok(CronExpr {
            minute: FieldSet::parse(fields[0], 0, 59, "minute")?,
            hour: FieldSet::parse(fields[1], 0, 23, "hour")?,
            dom: FieldSet::parse(fields[2], 1, 31, "day-of-month")?,
            month: FieldSet::parse(fields[3], 1, 12, "month")?,
            dow: FieldSet::parse(fields[4], 0, 6, "day-of-week")?,
        })
    }

    /// Does a wall-clock instant with these local fields match? `dow` is
    /// 0=Sunday..6=Saturday. Drives [`next_after`](CronExpr::next_after).
    fn matches(&self, minute: u32, hour: u32, dom: u32, month: u32, dow: u32) -> bool {
        if !self.minute.contains(minute) || !self.hour.contains(hour) || !self.month.contains(month)
        {
            return false;
        }
        // Vixie rule: both restricted ⇒ union; otherwise intersection (the `*`
        // side is always true, so the AND collapses to the restricted side).
        let dom_ok = self.dom.contains(dom);
        let dow_ok = self.dow.contains(dow);
        if !self.dom.star && !self.dow.star {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }

    /// The earliest instant strictly after `after` that matches this
    /// expression, or `None` if none exists within a bounded ~4-year horizon
    /// (e.g. an impossible expression like `0 0 30 2 *`).
    ///
    /// Generic over the timezone so tests can drive it deterministically with a
    /// fixed offset; the scheduler passes `DateTime<Local>` so matching happens
    /// against local wall-clock fields. Candidates advance by real time, which
    /// makes the search inherently DST-correct (see the module docs).
    pub fn next_after<Tz: TimeZone>(&self, after: DateTime<Tz>) -> Option<DateTime<Tz>> {
        // Start at the first whole minute strictly after `after`.
        let mut t = floor_to_minute(after)? + TimeDelta::minutes(1);
        for _ in 0..MAX_STEP_MINUTES {
            if self.matches(
                t.minute(),
                t.hour(),
                t.day(),
                t.month(),
                t.weekday().num_days_from_sunday(),
            ) {
                return Some(t);
            }
            t += TimeDelta::minutes(1);
        }
        None
    }
}

/// Truncate an instant to the start of its (local) minute via real-time
/// arithmetic — no naive-datetime reconstruction, so DST gaps can't make it
/// fail.
fn floor_to_minute<Tz: TimeZone>(dt: DateTime<Tz>) -> Option<DateTime<Tz>> {
    dt.with_second(0)?.with_nanosecond(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    // A fixed, DST-free offset makes every `next_after` assertion deterministic
    // regardless of the test runner's system timezone.
    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<FixedOffset> {
        utc().with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    // --- parsing: acceptance ---

    #[test]
    fn parses_canonical_defaults() {
        for expr in ["0 4 * * *", "0 4 * * 0", "0 4 1 * *", "*/15 * * * *"] {
            assert!(CronExpr::parse(expr).is_ok(), "should parse: {expr}");
        }
    }

    #[test]
    fn parses_ranges_lists_and_steps() {
        for expr in [
            "0 9-17 * * 1-5",
            "0 0,12 * * *",
            "0 0-23/2 * * *",
            "30 5/6 * * *",
        ] {
            assert!(CronExpr::parse(expr).is_ok(), "should parse: {expr}");
        }
    }

    // --- parsing: rejection ---

    #[test]
    fn rejects_wrong_field_count() {
        assert_eq!(CronExpr::parse("* * * *"), Err(CronError::FieldCount(4)));
        assert_eq!(
            CronExpr::parse("* * * * * *"),
            Err(CronError::FieldCount(6))
        );
        assert_eq!(CronExpr::parse(""), Err(CronError::FieldCount(0)));
    }

    #[test]
    fn rejects_out_of_range_values() {
        assert!(matches!(
            CronExpr::parse("60 * * * *"),
            Err(CronError::OutOfRange {
                field: "minute",
                value: 60,
                ..
            })
        ));
        // dow only goes 0-6 (Sunday = 0); 7/8 are rejected.
        assert!(matches!(
            CronExpr::parse("0 4 * * 8"),
            Err(CronError::OutOfRange {
                field: "day-of-week",
                ..
            })
        ));
        assert!(matches!(
            CronExpr::parse("0 4 * * 7"),
            Err(CronError::OutOfRange {
                field: "day-of-week",
                ..
            })
        ));
    }

    #[test]
    fn rejects_inverted_range_zero_step_and_garbage() {
        assert!(matches!(
            CronExpr::parse("5-1 * * * *"),
            Err(CronError::InvertedRange {
                field: "minute",
                ..
            })
        ));
        assert!(matches!(
            CronExpr::parse("*/0 * * * *"),
            Err(CronError::ZeroStep { field: "minute" })
        ));
        assert!(matches!(
            CronExpr::parse("abc * * * *"),
            Err(CronError::BadToken {
                field: "minute",
                ..
            })
        ));
    }

    // --- next_after ---

    #[test]
    fn daily_at_0400_fires_at_next_0400() {
        let e = CronExpr::parse("0 4 * * *").unwrap();
        assert_eq!(
            e.next_after(at(2026, 6, 2, 3, 59)),
            Some(at(2026, 6, 2, 4, 0))
        );
        // Strictly after: at exactly 04:00 the next fire is the following day.
        assert_eq!(
            e.next_after(at(2026, 6, 2, 4, 0)),
            Some(at(2026, 6, 3, 4, 0))
        );
    }

    #[test]
    fn every_15_minutes_steps_and_wraps_the_hour() {
        let e = CronExpr::parse("*/15 * * * *").unwrap();
        assert_eq!(
            e.next_after(at(2026, 6, 2, 10, 7)),
            Some(at(2026, 6, 2, 10, 15))
        );
        assert_eq!(
            e.next_after(at(2026, 6, 2, 10, 45)),
            Some(at(2026, 6, 2, 11, 0))
        );
    }

    #[test]
    fn weekly_sunday_0400_from_a_wednesday() {
        // 2026-06-03 is a Wednesday; the coming Sunday is 2026-06-07.
        let e = CronExpr::parse("0 4 * * 0").unwrap();
        assert_eq!(
            e.next_after(at(2026, 6, 3, 12, 0)),
            Some(at(2026, 6, 7, 4, 0))
        );
    }

    #[test]
    fn monthly_first_at_0400_crosses_month_and_year() {
        let e = CronExpr::parse("0 4 1 * *").unwrap();
        assert_eq!(
            e.next_after(at(2026, 1, 15, 0, 0)),
            Some(at(2026, 2, 1, 4, 0))
        );
        assert_eq!(
            e.next_after(at(2026, 12, 20, 0, 0)),
            Some(at(2027, 1, 1, 4, 0))
        );
    }

    #[test]
    fn end_of_january_rolls_into_february() {
        let e = CronExpr::parse("0 4 1 * *").unwrap();
        assert_eq!(
            e.next_after(at(2026, 1, 31, 5, 0)),
            Some(at(2026, 2, 1, 4, 0))
        );
    }

    #[test]
    fn dom_and_dow_both_restricted_is_a_union() {
        // "13th OR Friday at 00:00" (Vixie union). June 2026: the Fridays are
        // the 5th, 12th, 19th, 26th; the 13th is a Saturday.
        let e = CronExpr::parse("0 0 13 * 5").unwrap();
        // From the 1st, the first Friday (the 5th) wins via day-of-week.
        assert_eq!(
            e.next_after(at(2026, 6, 1, 0, 0)),
            Some(at(2026, 6, 5, 0, 0))
        );
        // Past Friday the 12th, the 13th (a Saturday) matches via day-of-month,
        // proving the union also fires on the non-Friday 13th.
        assert_eq!(
            e.next_after(at(2026, 6, 12, 12, 0)),
            Some(at(2026, 6, 13, 0, 0))
        );
    }

    #[test]
    fn leap_day_resolves_within_the_horizon() {
        // 2025 is not a leap year; the next Feb-29 is 2028.
        let e = CronExpr::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            e.next_after(at(2025, 3, 1, 0, 0)),
            Some(at(2028, 2, 29, 0, 0))
        );
    }

    #[test]
    fn impossible_expression_has_no_next_fire() {
        // Feb-30 never happens.
        let e = CronExpr::parse("0 0 30 2 *").unwrap();
        assert_eq!(e.next_after(at(2026, 1, 1, 0, 0)), None);
    }

    #[test]
    fn matches_respects_business_hours_range() {
        // 0 9-17 * * 1-5 → top of the hour, 09:00–17:00, Mon–Fri.
        let e = CronExpr::parse("0 9-17 * * 1-5").unwrap();
        // 2026-06-01 is a Monday.
        assert_eq!(
            e.next_after(at(2026, 6, 1, 8, 30)),
            Some(at(2026, 6, 1, 9, 0))
        );
        // After 17:00 on Friday (2026-06-05) → Monday 09:00 (2026-06-08).
        assert_eq!(
            e.next_after(at(2026, 6, 5, 17, 30)),
            Some(at(2026, 6, 8, 9, 0))
        );
    }
}
