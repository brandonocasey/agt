//! The committer of the commits agt replays.
//! The `GIT_COMMITTER_*` variables override `user.name` and `user.email`, as for `git rebase`.

use super::configured;
use crate::core::clock;
use crate::core::error::{Error, Result};

/// The committer of a commit that replays others, as `git rebase` makes it.
/// It comes from the environment, else `user.name` and `user.email` of `repo`,
/// `GIT_CONFIG_COUNT` included.
pub(super) fn committer_in(repo: &git2::Repository) -> Result<git2::Signature<'static>> {
    let fallback_name = configured(repo, "user.name")?;
    let fallback_email = configured(repo, "user.email")?;
    let read = |field: &str| std::env::var(format!("GIT_COMMITTER_{field}")).ok();
    let unknown = |what: &str| Error(format!("committer identity unknown: {what} is not set"));
    let name = read("NAME")
        .or(fallback_name)
        .ok_or_else(|| unknown("user.name"))?;
    let email = read("EMAIL")
        .or(fallback_email)
        .ok_or_else(|| unknown("user.email"))?;
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
