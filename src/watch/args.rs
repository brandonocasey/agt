//! agent-watch's own argument parser: its messages, limits and exit status 64 are part of the
//! contract the skills rely on.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

pub const DEFAULT_INTERVAL_MS: u64 = 15_000;
pub const DEFAULT_MAX_INTERVAL_MS: u64 = 120_000;
pub const DEFAULT_DEADLINE_SECONDS: u64 = 3_600;
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Github,
    Gitlab,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub platform: Platform,
    pub repo: String,
    pub number: u64,
    pub head: String,
    pub key: String,
}

#[derive(Debug)]
pub struct Options {
    pub targets: Vec<Target>,
    pub deadline_seconds: u64,
    pub interval_ms: u64,
    pub max_interval_ms: u64,
    pub help: bool,
}

pub fn usage() -> String {
    [
        "Usage: agt watch --target TARGET [--target ...]",
        "                 [--deadline-seconds N] [--interval-ms N]",
        "                 [--max-interval-ms N]",
        "",
        "TARGET is OWNER/REPO#PR@HEAD_SHA for a GitHub pull request, or",
        "GROUP[/SUBGROUP...]/PROJECT!IID@HEAD_SHA for a GitLab merge request.",
        "",
        "Watch required GitHub checks with read-only gh queries, or the GitLab",
        "head pipeline with read-only glab api queries. The GitLab host comes",
        "from GITLAB_HOST, else from `glab config get host`. Other platforms",
        "are not supported.",
    ]
    .join("\n")
}

fn parse_integer(value: &str, option: &str, allow_zero: bool) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{option} must be an integer"));
    }
    match value.parse::<u64>() {
        Ok(number) if number <= MAX_SAFE_INTEGER && (allow_zero || number >= 1) => Ok(number),
        _ => Err(format!("{option} is out of range")),
    }
}

static GITHUB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s/#]+/[^\s/#]+)#([1-9][0-9]*)@([0-9A-Za-z._-]+)$").expect("valid pattern")
});
static GITLAB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s/#!@]+(?:/[^\s/#!@]+)+)!([1-9][0-9]*)@([0-9A-Za-z._-]+)$")
        .expect("valid pattern")
});

pub fn parse_target(value: &str) -> Result<Target, String> {
    let invalid = || format!("invalid target: {value}");
    for (platform, pattern, separator) in [
        (Platform::Github, &*GITHUB, '#'),
        (Platform::Gitlab, &*GITLAB, '!'),
    ] {
        if let Some(found) = pattern.captures(value) {
            let number = found[2].parse::<u64>().map_err(|_| invalid())?;
            if number > MAX_SAFE_INTEGER {
                return Err(invalid());
            }
            return Ok(Target {
                platform,
                repo: found[1].to_owned(),
                number,
                head: found[3].to_owned(),
                key: format!("{}{separator}{}", &found[1], &found[2]),
            });
        }
    }
    Err(invalid())
}

pub fn parse_args(argv: &[String]) -> Result<Options, String> {
    let mut options = Options {
        targets: Vec::new(),
        deadline_seconds: DEFAULT_DEADLINE_SECONDS,
        interval_ms: DEFAULT_INTERVAL_MS,
        max_interval_ms: DEFAULT_MAX_INTERVAL_MS,
        help: false,
    };
    let mut index = 0;
    while index < argv.len() {
        let option = argv[index].as_str();
        index += 1;
        if option == "--help" || option == "-h" {
            options.help = true;
            continue;
        }
        let Some(value) = argv.get(index) else {
            return Err(format!("{option} needs a value"));
        };
        index += 1;
        match option {
            "--target" => options.targets.push(parse_target(value)?),
            "--deadline-seconds" => options.deadline_seconds = parse_integer(value, option, true)?,
            "--interval-ms" => options.interval_ms = parse_integer(value, option, false)?,
            "--max-interval-ms" => options.max_interval_ms = parse_integer(value, option, false)?,
            _ => return Err(format!("unknown option: {option}")),
        }
    }
    if !options.help && options.targets.is_empty() {
        return Err("at least one --target is required".into());
    }
    if options.interval_ms > options.max_interval_ms {
        return Err("--interval-ms cannot exceed --max-interval-ms".into());
    }
    let mut keys = HashSet::new();
    for target in &options.targets {
        if !keys.insert(target.key.as_str()) {
            return Err(format!("duplicate target: {}", target.key));
        }
    }
    Ok(options)
}

/// The wait before poll `attempt + 1`: the interval doubled per attempt, capped.
pub fn backoff_delay(attempt: u32, interval_ms: u64, max_interval_ms: u64) -> u64 {
    let scaled = u128::from(interval_ms) << attempt.min(64);
    u64::try_from(scaled.min(u128::from(max_interval_ms))).unwrap_or(max_interval_ms)
}
