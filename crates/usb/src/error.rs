use thiserror::Error;

#[derive(Debug, Error)]
pub enum UsbError {
    #[error("device disconnected")]
    Disconnected,
    #[error("transfer timed out")]
    Timeout,
    #[error("endpoint stalled")]
    Stall,
    #[error("device not found")]
    NotFound,
    #[error("{0}")]
    Other(String),
}

impl UsbError {
    /// Whether the device is gone and the session should be torn down.
    pub fn is_fatal(&self) -> bool {
        matches!(self, UsbError::Disconnected | UsbError::NotFound)
    }
}

pub type Result<T> = std::result::Result<T, UsbError>;
