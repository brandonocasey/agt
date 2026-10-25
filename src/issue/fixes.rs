//! Closes local issues that a merged PR/MR names, as `Fixes local-ab12cd` in its title or body.
//! Issues on a forge are left alone: the forge closes those itself, and `sync` brings the close.

use serde_json::{Value, json};

use super::local;
use crate::core::output::Output;
use crate::git::Repo;
use crate::store::{Namespace, Store};

const KEYWORDS: [&str; 9] = [
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

/// Adds the keys of the local issues that `text` fixes and this call closed to a JSON `output`
/// as `closed_issues`. A merge has already happened, so a failure to close one is a line on
/// stderr, never an error.
pub fn close_fixed(output: Output, text: &str) -> Output {
    let closed = close_named(&keys(text));
    match output {
        Output::Json(Value::Object(mut fields)) if !closed.is_empty() => {
            fields.insert("closed_issues".into(), json!(closed));
            Output::Json(Value::Object(fields))
        }
        output => output,
    }
}

fn close_named(keys: &[String]) -> Vec<String> {
    if keys.is_empty() {
        return Vec::new();
    }
    let store = Repo::discover().and_then(|repo| Store::open_repo(Namespace::Issues, &repo, false));
    let store = match store {
        Ok(store) => store,
        Err(error) => {
            eprintln!("agt: could not close the issues it fixes ({error})");
            return Vec::new();
        }
    };
    let mut closed = Vec::new();
    for key in keys {
        match local::close_if_open(&store, key) {
            Ok(true) => closed.push(key.clone()),
            Ok(false) => {}
            Err(error) => eprintln!("agt: could not close {key} ({error})"),
        }
    }
    closed
}

/// The local issue keys that follow a closing keyword in `text`, in order and without repeats.
/// Only `local-` keys count: a bare `#12` names an issue on the forge.
fn keys(text: &str) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut found: Vec<String> = Vec::new();
    for pair in words.windows(2) {
        let keyword = pair[0]
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_ascii_lowercase();
        if !KEYWORDS.contains(&keyword.as_str()) {
            continue;
        }
        let key = pair[1]
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_ascii_lowercase();
        let token = key.strip_prefix("local-").unwrap_or_default();
        if !token.is_empty()
            && token.chars().all(|c| c.is_ascii_alphanumeric())
            && !found.contains(&key)
        {
            found.push(key);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_keys_after_closing_keywords() {
        assert_eq!(keys("Fixes local-ab12cd"), ["local-ab12cd"]);
        assert_eq!(keys("closes: local-ab12cd."), ["local-ab12cd"]);
        assert_eq!(
            keys("Resolved `local-ab12cd`, (fix local-zz9)"),
            ["local-ab12cd", "local-zz9"]
        );
        assert_eq!(
            keys("Fixes\nlocal-ab12cd and Fixes local-ab12cd"),
            ["local-ab12cd"]
        );
    }

    #[test]
    fn ignores_other_references() {
        assert!(keys("Fixes #12 and fixes o/r#3").is_empty());
        assert!(keys("See local-ab12cd").is_empty());
        assert!(keys("prefixes local-ab12cd").is_empty());
        assert!(keys("Fixes local-").is_empty());
        assert!(keys("Fixes").is_empty());
    }
}
