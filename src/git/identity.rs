//! The author and committer of the commits agt writes: `agt <agt@localhost>`.
//! The `GIT_AUTHOR_*` and `GIT_COMMITTER_*` variables override it, as for `git commit-tree`.

use super::configured;
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

/// The signature for `role`, `agt` unless the environment names another. A date is
/// `<epoch> <±hhmm>` (a leading `@` is allowed) or RFC 3339; git's other date forms are refused.
pub(super) fn signature(role: Role) -> Result<git2::Signature<'static>> {
    build(
        role,
        Some(DEFAULT_NAME.to_owned()),
        Some(DEFAULT_EMAIL.to_owned()),
    )
}

/// The committer of a commit that replays others, as `git rebase` makes it.
/// It comes from the environment, else `user.name` and `user.email` of `repo`,
/// `GIT_CONFIG_COUNT` included.
pub(super) fn committer_in(repo: &git2::Repository) -> Result<git2::Signature<'static>> {
    build(
        Role::Committer,
        configured(repo, "user.name")?,
        configured(repo, "user.email")?,
    )
}

fn build(
    role: Role,
    fallback_name: Option<String>,
    fallback_email: Option<String>,
) -> Result<git2::Signature<'static>> {
    let read = |field: &str| std::env::var(role.variable(field)).ok();
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
