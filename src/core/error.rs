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

/// libgit2's text for `error`; a config file it cannot parse reads as git says it:
/// `bad config line N in file PATH`.
pub fn libgit2_message(error: &git2::Error) -> String {
    let message = error.message();
    let located = (error.class() == git2::ErrorClass::Config)
        .then(|| {
            message
                .rsplit_once("(in ")?
                .1
                .strip_suffix(')')?
                .rsplit_once(':')
        })
        .flatten();
    match located {
        Some((path, line)) if line.bytes().all(|byte| byte.is_ascii_digit()) => {
            format!("bad config line {line} in file {path}")
        }
        _ => message.to_owned(),
    }
}
