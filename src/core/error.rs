use std::fmt::Display;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Error(pub String);

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Error(message.into()))
}

pub fn ctx<E: Display>(what: impl Display) -> impl FnOnce(E) -> Error {
    move |error| Error(format!("{what}: {error}"))
}

impl From<git2::Error> for Error {
    fn from(error: git2::Error) -> Self {
        Self(libgit2_message(&error))
    }
}

/// libgit2's text for `error`.
/// An unparsable config gets git's text: `bad config line N in file PATH`.
pub fn libgit2_message(error: &git2::Error) -> String {
    let message = error.message();
    let located = (error.class() == git2::ErrorClass::Config)
        .then(|| config_location(message))
        .flatten();
    match located {
        Some((path, line)) => format!("bad config line {line} in file {path}"),
        None => message.to_owned(),
    }
}

/// The file and line in libgit2's `(in PATH:LINE)` or `(in PATH:LINE, column COLUMN)` suffix.
fn config_location(message: &str) -> Option<(&str, &str)> {
    let place = message.rsplit_once("(in ")?.1.strip_suffix(')')?;
    let place = place
        .rsplit_once(", column ")
        .filter(|(_, column)| column.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(place, |(place, _)| place);
    place
        .rsplit_once(':')
        .filter(|(_, line)| line.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::libgit2_message;

    #[test]
    fn a_config_file_that_does_not_parse_reads_as_git_says_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        for (text, line) in [
            ("[core\n", 1),
            ("[core]\n\tbad key = 1\n", 2),
            ("[a b]\n", 1),
            ("[core]\n[remote \"x\"y]\n", 2),
            ("[core]\n[remote \"x\n", 2),
        ] {
            std::fs::write(&path, text).unwrap();
            let error = git2::Config::open(&path)
                .and_then(|config| config.get_string("core.name"))
                .unwrap_err();
            assert_eq!(
                libgit2_message(&error),
                format!("bad config line {line} in file {}", path.display()),
                "libgit2 said: {}",
                error.message()
            );
        }
    }
}
