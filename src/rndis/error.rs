use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("message truncated")]
    Truncated,
    #[error("malformed message: {0}")]
    Malformed(&'static str),
    #[error("unexpected message type 0x{0:08x}")]
    UnexpectedMessage(u32),
    #[error("device returned status 0x{0:08x}")]
    Status(u32),
    #[error("no response from device")]
    NoResponse,
    #[error("transport: {0}")]
    Transport(String),
}

pub type Result<T> = std::result::Result<T, Error>;
