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
