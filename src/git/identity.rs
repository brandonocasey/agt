//! The author and committer of the commits agt writes: `agt <agt@localhost>` now, unless the
//! `GIT_AUTHOR_*` and `GIT_COMMITTER_*` variables name another, as they do for `git commit-tree`.

use crate::core::clock;
use crate::core::error::{Error, Result};

const DEFAULT_NAME: &str = "agt";
const DEFAULT_EMAIL: &str = "agt@localhost";

/// Which signature of a commit to build.
#[derive(Clone, Copy)]
pub(super) enum Role {
    Author,
    Committer,
}

impl Role {
    fn variable(self, field: &str) -> String {
        let role = match self {
            Self::Author => "AUTHOR",
            Self::Committer => "COMMITTER",
        };
        format!("GIT_{role}_{field}")
    }
}

/// The signature for `role`. A date is `<epoch> <±hhmm>` (a leading `@` is allowed) or RFC 3339;
/// git's other date forms are refused.
pub(super) fn signature(role: Role) -> Result<git2::Signature<'static>> {
    let read = |field: &str| std::env::var(role.variable(field)).ok();
    let name = read("NAME").unwrap_or_else(|| DEFAULT_NAME.to_owned());
    let email = read("EMAIL").unwrap_or_else(|| DEFAULT_EMAIL.to_owned());
    Ok(match read("DATE") {
        Some(date) => git2::Signature::new(&name, &email, &parse_date(&date)?)?,
        None => git2::Signature::now(&name, &email)?,
    })
}

fn parse_date(text: &str) -> Result<git2::Time> {
    let invalid = || Error(format!("invalid date format: {text}"));
    let raw = text.strip_prefix('@').unwrap_or(text);
    if let Some((epoch, zone)) = raw.split_once(' ') {
        let seconds = epoch.parse::<i64>().map_err(|_| invalid())?;
        return Ok(git2::Time::new(
            seconds,
            zone_minutes(zone, false).ok_or_else(invalid)?,
        ));
    }
    let seconds = clock::parse(text).map_err(|_| invalid())?;
    let zone = if text.ends_with(['Z', 'z']) {
        Some(0)
    } else {
        text.get(text.len().saturating_sub(6)..)
            .and_then(|zone| zone_minutes(zone, true))
    };
    Ok(git2::Time::new(seconds, zone.ok_or_else(invalid)?))
}

/// The minutes east of UTC that `zone` (`+hhmm`, or `+hh:mm` with `colon`) names.
fn zone_minutes(zone: &str, colon: bool) -> Option<i32> {
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = zone.get(1..)?;
    let (hours, minutes) = if colon {
        digits.split_once(':')?
    } else {
        digits.split_at_checked(2)?
    };
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    Some(sign * (hours.parse::<i32>().ok()? * 60 + minutes.parse::<i32>().ok()?))
}

#[cfg(test)]
mod tests {
    use super::parse_date;

    #[test]
    fn dates_come_out_in_the_zone_they_name() {
        for (text, seconds, offset) in [
            ("1700000000 +0200", 1_700_000_000, 120),
            ("@1700000000 -0530", 1_700_000_000, -330),
            ("2026-01-01T00:00:00Z", 1_767_225_600, 0),
            ("2026-01-02T03:04:05+01:00", 1_767_319_445, 60),
        ] {
            let time = parse_date(text).unwrap();
            assert_eq!(
                (time.seconds(), time.offset_minutes()),
                (seconds, offset),
                "{text}"
            );
        }
    }

    #[test]
    fn a_date_in_a_form_git_alone_reads_is_refused() {
        for text in [
            "yesterday",
            "Tue, 14 Nov 2023 22:13:20 +0000",
            "1700000000",
            "1700000000 0200",
        ] {
            let error = parse_date(text).unwrap_err().to_string();
            assert!(error.contains("invalid date format"), "{text}: {error}");
        }
    }
}
