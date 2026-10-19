//! Issue references in free text and branch names.

use crate::forge::service::SERVICES;

/// `(repository, number)` for every `#12`, `owner/repo#12` and issue URL in `text`. The
/// repository is `None` for a bare `#12`.
pub fn references(text: &str) -> Vec<(Option<String>, u64)> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    for (at, _) in text.match_indices('#') {
        let digits: String = text[at + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let Ok(number) = digits.parse::<u64>() else {
            continue;
        };
        let mut start = at;
        while start > 0 && is_path_char(bytes[start - 1]) {
            start -= 1;
        }
        let repo = &text[start..at];
        found.push(((repo.contains('/')).then(|| repo.to_owned()), number));
    }
    for marker in ["/-/issues/", "/issues/"] {
        for (at, _) in text.match_indices(marker) {
            let digits: String = text[at + marker.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            let Ok(number) = digits.parse::<u64>() else {
                continue;
            };
            let before = &text[..at];
            // `/-/issues/` already matched as a whole.
            if marker == "/issues/" && before.ends_with("/-") {
                continue;
            }
            let url = before
                .rsplit(|c: char| c.is_whitespace() || matches!(c, '(' | '<' | '['))
                .next()
                .unwrap_or_default();
            let path = url.split_once("://").map_or(url, |(_, rest)| rest);
            let repo = path.split_once('/').map(|(_, repo)| repo.to_owned());
            found.push((repo, number));
        }
    }
    found
}

fn is_path_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'.' | b'-')
}

/// The issue numbers a branch name carries: a segment that starts with digits (`12-fix`,
/// `feat/12-fix`) or follows `issue-`, `issues-`, a service's key prefix (`gh-`, `gl-`) or `#`.
pub fn branch_numbers(branch: &str) -> Vec<u64> {
    let mut found = Vec::new();
    let prefixes: Vec<String> = ["issues-", "issue-"]
        .into_iter()
        .map(str::to_owned)
        .chain(SERVICES.iter().map(|kind| format!("{}-", kind.key_prefix)))
        .chain(["#".to_owned()])
        .collect();
    for segment in branch.split('/') {
        let mut rest = segment;
        for prefix in prefixes.iter().map(String::as_str) {
            if let Some(stripped) = segment.strip_prefix(prefix) {
                rest = stripped;
                break;
            }
        }
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let boundary = rest[digits.len()..]
            .chars()
            .next()
            .is_none_or(|c| matches!(c, '-' | '_' | '.'));
        if let (Ok(number), true) = (digits.parse::<u64>(), boundary) {
            found.push(number);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_bare_qualified_and_url_references() {
        let found = references(
            "Fix #12 and g/sub/p#7, see https://gitlab.com/g/p/-/issues/9 and https://github.com/o/r/issues/3.",
        );
        assert!(found.contains(&(None, 12)));
        assert!(found.contains(&(Some("g/sub/p".into()), 7)));
        assert!(found.contains(&(Some("g/p".into()), 9)));
        assert!(found.contains(&(Some("o/r".into()), 3)));
        assert!(references("no refs, 12 or # 5").is_empty());
    }

    #[test]
    fn finds_numbers_in_branch_names() {
        assert_eq!(branch_numbers("12-fix-login"), [12]);
        assert_eq!(branch_numbers("feat/34-thing"), [34]);
        assert_eq!(branch_numbers("fix/issue-56"), [56]);
        assert_eq!(branch_numbers("gh-7-x"), [7]);
        assert!(branch_numbers("feature/v2-api").is_empty());
        assert!(branch_numbers("2fa-login").is_empty());
    }
}
