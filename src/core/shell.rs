/// A word the shell reads back unchanged: as is when it holds only safe characters, else in
/// single quotes. Branch names come from other people's PRs and may hold `;` or `$(`.
pub fn quote(word: &str) -> String {
    let safe = |ch: char| ch.is_ascii_alphanumeric() || "_-./@:+,=%".contains(ch);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn commands_quote_what_the_shell_would_read() {
        assert_eq!(quote("feat/a-b"), "feat/a-b");
        assert_eq!(quote("a;b"), "'a;b'");
        assert_eq!(quote("c$(d)"), "'c$(d)'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
