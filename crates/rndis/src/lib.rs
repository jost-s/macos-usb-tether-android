//! RNDIS protocol: control state machine and PACKET_MSG framing.
//!
//! Hardware-free and fully unit-tested; the USB transport is injected.

pub mod control;
pub mod error;
pub mod packet;
pub mod wire;

pub use control::{ControlTransport, LinkState, Rndis, Session, HOST_MAX_TRANSFER_SIZE};
pub use error::{Error, Result};
