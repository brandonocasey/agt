use std::fs;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;

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

/// The latest of the RFC 3339 `stamps` in Unix seconds; `None` without any.
pub fn latest<'a>(stamps: impl IntoIterator<Item = &'a str>) -> Result<Option<i64>> {
    let mut latest = None;
    for stamp in stamps {
        latest = latest.max(Some(parse(stamp)?));
    }
    Ok(latest)
}

/// A duration as its largest whole unit: `45s`, `12m`, `3h`, `2d`.
pub fn age(secs: i64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / SECS_PER_DAY),
    }
}

/// How long before `now` the instant `then` was, as `3d ago`; a `then` in the future reads
/// `0s ago`.
pub fn ago(now: i64, then: i64) -> String {
    format!("{} ago", age((now - then).max(0)))
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
    if !days_in_month(year, month).is_some_and(|length| (1..=length).contains(&day))
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

/// Microseconds since the epoch for a transcript timestamp. Unlike [`parse`] it also takes a
/// space for the `T`, no seconds, no zone (read as UTC) and zones such as `+0100` or `+01`.
pub fn parse_micros(stamp: &str) -> Option<i128> {
    let (date, time) = stamp.split_once(['T', ' '])?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let split = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(split);
    let (whole, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut parts = whole.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute) = (parts.next()??, parts.next()??);
    let second = parts.next().flatten().unwrap_or(0);
    let micros: i64 = format!("{:0<6}", fraction.chars().take(6).collect::<String>())
        .parse()
        .ok()?;
    let offset = match zone {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
            let hours: i64 = digits.get(..2)?.parse().ok()?;
            let minutes: i64 = digits.get(2..4).unwrap_or("0").parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let secs =
        days_from_civil(year, month, day) * SECS_PER_DAY + hour * 3600 + minute * 60 + second
            - offset;
    Some(i128::from(secs) * 1_000_000 + i128::from(micros))
}

/// The broken-down local time of `secs`.
pub fn local_tm(secs: i64) -> libc::tm {
    // SAFETY: `localtime_r` writes only into the `tm` it is given.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let time: libc::time_t = secs as libc::time_t;
        libc::localtime_r(&raw const time, &raw mut tm);
        tm
    }
}

/// Unix seconds of a local `tm`, normalizing its fields; daylight saving is worked out.
pub fn mktime_local(tm: &mut libc::tm) -> i64 {
    tm.tm_isdst = -1;
    // SAFETY: `mktime` reads and normalizes the `tm` it is given.
    unsafe { libc::mktime(tm) }
}

/// The local date `days` days before `now`, as `YYYY-MM-DD`.
pub fn days_back(now: i64, days: i64) -> String {
    let mut tm = local_tm(now);
    tm.tm_mday -= days as libc::c_int;
    mktime_local(&mut tm);
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4})-(\d{1,2})-(\d{1,2})$").expect("valid pattern"));

/// Local midnight of a `YYYY-MM-DD` date in Unix seconds, or `None` when it is not a real date.
pub fn local_midnight(date: &str) -> Option<f64> {
    let found = DATE.captures(date)?;
    let [year, month, day] = [1, 2, 3].map(|index| found[index].parse::<i32>().unwrap_or(0));
    let length = days_in_month(year.into(), month.into())?;
    if !(1..=length).contains(&i64::from(day)) {
        return None;
    }
    // SAFETY: an all-zero `tm` is a valid value to fill in.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    Some(mktime_local(&mut tm) as f64)
}

/// The days of `month` (1 to 12) in `year`; `None` for any other month.
fn days_in_month(year: i64, month: i64) -> Option<i64> {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 if leap => Some(29),
        2 => Some(28),
        _ => None,
    }
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
        assert!(parse("2026-02-31T12:00:00Z").is_err());
        assert!(parse("2026-04-31T12:00:00Z").is_err());
        assert!(parse("2026-02-29T12:00:00Z").is_err());
        assert!(parse("2024-02-29T12:00:00Z").is_ok());
        assert!(parse("2026-10-16T12:00:00").is_err());
    }

    #[test]
    fn ago_names_the_largest_whole_unit() {
        assert_eq!(ago(100, 100), "0s ago");
        assert_eq!(ago(100, 41), "59s ago");
        assert_eq!(ago(300, 0), "5m ago");
        assert_eq!(ago(3 * 3600 + 59, 0), "3h ago");
        assert_eq!(ago(9 * SECS_PER_DAY + 5, 0), "9d ago");
        assert_eq!(ago(0, 50), "0s ago");
    }

    #[test]
    fn transcript_timestamps_parse_to_microseconds() {
        assert_eq!(parse_micros("1970-01-01T00:00:01.5Z"), Some(1_500_000));
        assert_eq!(parse_micros("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_micros("1970-01-01 00:01"), Some(60_000_000));
        assert_eq!(parse_micros("nope"), None);
    }

    #[test]
    fn dates_must_be_real() {
        assert!(local_midnight("2026-02-29").is_none());
        assert!(local_midnight("2024-02-29").is_some());
        assert!(local_midnight("2026-1-5").is_some());
        assert!(local_midnight("26-01-05").is_none());
        assert!(local_midnight("2026-13-01").is_none());
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
