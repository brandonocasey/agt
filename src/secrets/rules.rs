//! The scan rules, one table. A rule names a token format; `value` picks the part of a match
//! that must also look like a real secret (the whole match when the pattern has no group).

use std::sync::LazyLock;

use regex::Regex;

struct Spec {
    name: &'static str,
    pattern: &'static str,
    /// Called with capture group 1 when the pattern has one; false rejects the match.
    accept: fn(&str) -> bool,
}

fn always(_: &str) -> bool {
    true
}

/// A value that names a variable, holds a placeholder or is too short to be a credential.
fn real_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let env_name = value
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    value.chars().count() >= 8
        && !value.starts_with(['$', '<', '{', '%'])
        && !env_name
        && ![
            "changeme",
            "redacted",
            "placeholder",
            "example",
            "xxx",
            "your_",
            "your-",
            "dummy",
        ]
        .iter()
        .any(|word| lower.contains(word))
}

pub const GENERIC: &str = "generic-assignment";

const SPECS: &[Spec] = &[
    Spec {
        name: "github-token",
        pattern: r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})",
        accept: always,
    },
    Spec {
        name: "gitlab-token",
        pattern: r"\bglpat-[A-Za-z0-9_-]{20,}",
        accept: always,
    },
    Spec {
        name: "slack-token",
        pattern: r"\bxox[abpr]-[A-Za-z0-9-]{10,}",
        accept: always,
    },
    Spec {
        name: "aws-key-id",
        pattern: r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
        accept: always,
    },
    Spec {
        name: "private-key",
        pattern: r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----",
        accept: always,
    },
    Spec {
        name: "jwt",
        pattern: r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        accept: always,
    },
    Spec {
        name: GENERIC,
        pattern: r#"(?i)(?:key|token|secret|passw(?:or)?d|pwd)[\w.-]*["']?\s*[:=]\s*["']([^"'\s]+)["']"#,
        accept: real_value,
    },
    // The same rule for `.env`, `export X=...` and YAML lines: one bare token ending the line.
    Spec {
        name: GENERIC,
        pattern: r"(?i)(?:key|token|secret|passw(?:or)?d|pwd)[\w.-]*\s*[:=]\s*([A-Za-z0-9_\-+/=.]{16,})\s*(?:#.*)?$",
        accept: real_value,
    },
];

pub struct Rule {
    pub name: &'static str,
    regex: Regex,
    accept: fn(&str) -> bool,
}

pub static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    SPECS
        .iter()
        .map(|spec| Rule {
            name: spec.name,
            regex: Regex::new(spec.pattern).expect("rule patterns are valid"),
            accept: spec.accept,
        })
        .collect()
});

impl Rule {
    pub fn matches(&self, line: &str) -> bool {
        self.regex.captures_iter(line).any(|found| {
            let part = found
                .get(1)
                .unwrap_or_else(|| found.get(0).expect("whole match"));
            (self.accept)(part.as_str())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(line: &str) -> Vec<&'static str> {
        RULES
            .iter()
            .filter(|r| r.matches(line))
            .map(|r| r.name)
            .collect()
    }

    #[test]
    fn generic_rule_skips_non_secrets() {
        for line in [
            r#"password = """#,
            r#"token = "<redacted>""#,
            r#"api_key = "${API_KEY}""#,
            r#"api_key: "$API_KEY""#,
            r#"secret = "changeme""#,
            r#"secret = "xxxxxxxxxxxx""#,
            r#"token = "GITHUB_TOKEN""#,
            r#"token = "short""#,
        ] {
            assert!(hits(line).is_empty(), "{line}");
        }
        for line in [
            "password = getPassword()",
            "token: changeme",
            "KEY=$OTHER",
            "API_KEY=short",
            "API_KEY=SOME_ENV_VAR_NAME_HERE",
            "KEY=abcdefghijklmnop qrst",
        ] {
            assert!(hits(line).is_empty(), "{line}");
        }
        for line in [
            "API_KEY=Zq8vR2mN7xL4pK9sT3wY6bH1",
            "export SECRET_TOKEN=Zq8vR2mN7xL4pK9sT3wY6bH1 # prod",
            "api_key: Zq8vR2mN7xL4pK9sT3wY6bH1",
        ] {
            assert_eq!(hits(line), ["generic-assignment"], "{line}");
        }
        assert_eq!(
            hits(r#"db_password = "hunter2hunter2""#),
            ["generic-assignment"]
        );
    }

    #[test]
    fn named_formats_match() {
        assert_eq!(
            hits(&format!("{}{}", "AKIA", "ABCDEFGHIJKLMNOP")),
            ["aws-key-id"]
        );
        assert_eq!(hits("-----BEGIN RSA PRIVATE KEY-----"), ["private-key"]);
    }
}
