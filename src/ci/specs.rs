//! Reads failing specs out of job logs (Playwright, WebdriverIO, Karma, Jest) and groups
//! sharded jobs into families.

use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;

macro_rules! pattern {
    ($name:ident, $text:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($text).expect("valid pattern"));
    };
}

pattern!(ANSI, r"\x1b\[[0-9;]*[A-Za-z]");
pattern!(RUNNER_PREFIX, r"^\d{4}-\d\d-\d\dT[\d:.]+Z \d+[OE]\+?\s?");
pattern!(
    PW_COUNT,
    r"^\s+\d+ (failed|flaky|passed|skipped|interrupted|did not run)\b"
);
// Playwright pads failure headers with a ─ ruler whose length depends on the line number; drop it.
pattern!(
    PW_TEST,
    r"^\s+(?:\d+\) )?\[[^\]]+\] › (\S+?):\d+:\d+ › (.+?)[\s─]*$"
);
pattern!(
    WDIO,
    r"(FAILED|PASSED) in \S+ - file://(\S+)(?: \((\d+) retries\))?"
);
pattern!(KARMA, r"^\S.*?\([^)]*\) (.+?) FAILED$");
// The host pattern has a negative lookahead for `Console`, which is checked by hand.
pattern!(JEST, r"^\s*● (.+?)\s*$");
pattern!(SHARD, r"(:\s*\[\d+\]|\s+\d+/\d+)$");

/// Shards and parallel copies of one job share a family: `test-pw: [3]` and `test-x 4/10`.
pub fn family(name: &str) -> String {
    SHARD.replace(name, "").into_owned()
}

/// Maps spec to `failed` or `flaky` from a job log.
pub fn parse_specs(trace: &str, project_path: &str) -> IndexMap<String, &'static str> {
    let mut specs: IndexMap<String, &'static str> = IndexMap::new();
    let mut section: Option<&'static str> = None;
    let prefix = format!("{project_path}/");
    for raw in trace.lines() {
        let plain = ANSI.replace_all(raw, "");
        let line = RUNNER_PREFIX.replace(&plain, "");
        let line = line.trim_end_matches('\r');
        if let Some(count) = PW_COUNT.captures(line) {
            section = match &count[1] {
                "failed" => Some("failed"),
                "flaky" => Some("flaky"),
                _ => None,
            };
            continue;
        }
        if let (Some(kind), Some(test)) = (section, PW_TEST.captures(line)) {
            specs.insert(format!("{} › {}", &test[1], &test[2]), kind);
            continue;
        }
        if section.is_some() && !line.trim().is_empty() {
            section = None;
        }
        if let Some(wdio) = WDIO.captures(line) {
            let file = &wdio[2];
            let path = file
                .split_once(prefix.as_str())
                .map_or(file, |(_, rest)| rest);
            if &wdio[1] == "FAILED" {
                specs.insert(path.to_owned(), "failed");
            } else if wdio
                .get(3)
                .is_some_and(|retries| retries.as_str().parse::<u64>().is_ok_and(|n| n > 0))
            {
                specs.entry(path.to_owned()).or_insert("flaky");
            }
            continue;
        }
        if let Some(karma) = KARMA.captures(line)
            && !line.starts_with("Executed")
        {
            specs.insert(karma[1].to_owned(), "failed");
            continue;
        }
        if let Some(jest) = JEST.captures(line)
            && !jest[1].starts_with("Console")
        {
            specs.insert(jest[1].to_owned(), "failed");
        }
    }
    specs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_drop_shard_suffixes() {
        assert_eq!(family("test-pw: [3]"), "test-pw");
        assert_eq!(family("test-pw:[12]"), "test-pw");
        assert_eq!(family("test-x 4/10"), "test-x");
        assert_eq!(family("lint"), "lint");
        assert_eq!(family("build 2"), "build 2");
    }

    #[test]
    fn reads_playwright_failures_and_flaky_sections() {
        let log = "\
2026-01-02T03:04:05.123456Z 01O \u{1b}[31m  2 failed\u{1b}[0m
2026-01-02T03:04:05.123456Z 01O     [chromium] › tests/a.spec.ts:10:5 › login works ──────────
2026-01-02T03:04:05.123456Z 01O     [chromium] › tests/b.spec.ts:7:1 › other thing
2026-01-02T03:04:05.123456Z 01O   1 flaky
2026-01-02T03:04:05.123456Z 01O     [chromium] › tests/c.spec.ts:3:1 › sometimes
2026-01-02T03:04:05.123456Z 01O   5 passed (1m)
    [chromium] › tests/d.spec.ts:3:1 › ignored after the counts
";
        let specs = parse_specs(log, "group/project");
        let list: Vec<_> = specs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(
            list,
            [
                ("tests/a.spec.ts › login works", "failed"),
                ("tests/b.spec.ts › other thing", "failed"),
                ("tests/c.spec.ts › sometimes", "flaky"),
            ]
        );
    }

    #[test]
    fn reads_webdriverio_karma_and_jest_failures() {
        let log = "\
[0-0] FAILED in chrome - file:///builds/group/project/e2e/a.e2e.ts
[0-1] PASSED in chrome - file:///builds/group/project/e2e/b.e2e.ts (2 retries)
[0-2] PASSED in chrome - file:///builds/group/project/e2e/c.e2e.ts (0 retries)
Chrome Headless 1.0 (Linux) widget renders FAILED
Executed 3 of 4 (1 FAILED)
  ● Console
  ● suite › does a thing
";
        let specs = parse_specs(log, "group/project");
        let list: Vec<_> = specs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(
            list,
            [
                ("e2e/a.e2e.ts", "failed"),
                ("e2e/b.e2e.ts", "flaky"),
                ("widget renders", "failed"),
                ("suite › does a thing", "failed"),
            ]
        );
    }
}
