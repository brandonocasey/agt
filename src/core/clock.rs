use std::fs;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::error::{Error, Result, fail};

// Test-only override so age pruning can be checked without waiting 90 days.
const NOW_OVERRIDE: &str = "AGT_NOW";

/// Seconds `sleep` skipped while the clock is overridden.
static SLEPT: AtomicI64 = AtomicI64::new(0);

/// Waits `secs`. With the clock overridden it advances that clock instead, so loops that poll
/// by time run at once under test.
pub fn sleep(secs: u64) {
    if std::env::var_os(NOW_OVERRIDE).is_some() {
        SLEPT.fetch_add(i64::try_from(secs).unwrap_or(i64::MAX), Ordering::Relaxed);
    } else {
        std::thread::sleep(Duration::from_secs(secs));
    }
}

pub fn now_secs() -> Result<i64> {
    if let Some(value) = std::env::var_os(NOW_OVERRIDE) {
        let value = value.to_string_lossy();
        return value
            .parse::<i64>()
            .map(|secs| secs + SLEPT.load(Ordering::Relaxed))
            .map_err(|_| Error(format!("{NOW_OVERRIDE} must be Unix seconds, got {value}")));
    }
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error(format!("system clock is before 1970: {error}")))?;
    i64::try_from(elapsed.as_secs()).map_err(|_| Error("system clock is out of range".into()))
}

pub fn now() -> Result<String> {
    Ok(format(now_secs()?))
}

/// Seconds in a day.
pub const SECS_PER_DAY: i64 = 86_400;

/// Nanoseconds since the Unix epoch, for names that must differ between runs; 0 before 1970.
pub fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

/// When `meta` was last modified, in Unix seconds; `None` when unknown or before 1970.
pub fn mtime_secs(meta: &fs::Metadata) -> Option<i64> {
    let elapsed = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(elapsed.as_secs()).ok()
}

/// The current time as `YYYY-MM-DDTHH:MM:SS.mmmZ`, the form `Date.toISOString()` prints.
pub fn now_iso_millis() -> Result<String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error(format!("system clock is before 1970: {error}")))?;
    let secs = i64::try_from(elapsed.as_secs())
        .map_err(|_| Error("system clock is out of range".into()))?;
    let stamp = format(secs);
    Ok(format!(
        "{}.{:03}Z",
        stamp.trim_end_matches('Z'),
        elapsed.subsec_millis()
    ))
}

pub fn format(secs: i64) -> String {
    let days = secs.div_euclid(SECS_PER_DAY);
    let rest = secs.rem_euclid(SECS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Parses an RFC 3339 timestamp (`T` separator, optional fraction, `Z` or `±HH:MM`) into Unix
/// seconds.
pub fn parse(text: &str) -> Result<i64> {
    let invalid = || Error(format!("invalid timestamp: {text}"));
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return Err(invalid());
    }
    let number = |part: &str| -> Result<i64> {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        part.parse().map_err(|_| invalid())
    };
    let field = |range: std::ops::Range<usize>| number(text.get(range).unwrap_or_default());
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    let mut rest = text.get(19..).ok_or_else(invalid)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(invalid());
        }
        rest = fraction.get(digits..).ok_or_else(invalid)?;
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first() {
                Some(b'+') => 1,
                Some(b'-') => -1,
                _ => return Err(invalid()),
            };
            let (hours, minutes) = rest
                .get(1..)
                .and_then(|zone| zone.split_once(':'))
                .ok_or_else(invalid)?;
            if hours.len() != 2 || minutes.len() != 2 {
                return Err(invalid());
            }
            sign * (number(hours)? * 3600 + number(minutes)? * 60)
        }
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return fail(format!("invalid timestamp: {text}"));
    }
    Ok(
        days_from_civil(year, month, day) * SECS_PER_DAY + hour * 3600 + minute * 60 + second
            - offset,
    )
}

/// Rewrites a forge timestamp in the store's `YYYY-MM-DDTHH:MM:SSZ` form.
pub fn normalize(text: &str) -> Result<String> {
    Ok(format(parse(text)?))
}

// Proleptic Gregorian conversions from https://howardhinnant.github.io/date_algorithms.html
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for secs in [0, 951_782_400, 1_792_108_800, 4_102_444_799] {
            assert_eq!(parse(&format(secs)).unwrap(), secs);
        }
        assert_eq!(format(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn rejects_other_forms() {
        assert!(parse("2026-10-16 12:00:00Z").is_err());
        assert!(parse("2026-13-16T12:00:00Z").is_err());
        assert!(parse("2026-10-16T12:00:00").is_err());
    }

    #[test]
    fn normalizes_fractions_and_offsets() {
        assert_eq!(
            normalize("2026-10-12T21:42:34.471Z").unwrap(),
            "2026-10-12T21:42:34Z"
        );
        assert_eq!(
            normalize("2026-10-12T23:42:34+02:00").unwrap(),
            "2026-10-12T21:42:34Z"
        );
    }
}
