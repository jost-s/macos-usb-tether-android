use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("packet truncated")]
    Truncated,
    #[error("malformed packet: {0}")]
    Malformed(&'static str),
    #[error("not for us")]
    NotForUs,
}

pub type Result<T> = std::result::Result<T, Error>;
