//! Swappable USB layer: a backend-independent trait plus descriptor snapshots
//! and RNDIS interface matching.

pub mod backend;
pub mod descriptor;
pub mod error;
pub mod matcher;
mod nusb_backend;

pub use backend::{
    ControlSetup, ControlType, HotplugEvent, HotplugWatch, InEndpoint, OutEndpoint, Recipient,
    UsbBackend, UsbDevice, UsbInterface,
};
pub use descriptor::{
    ConfigDescriptor, DeviceId, DeviceInfo, Direction, EndpointDescriptor, InterfaceDescriptor,
    TransferType,
};
pub use error::{Result, UsbError};
pub use matcher::{find_rndis, RndisFunction};
pub use nusb_backend::NusbBackend;

/// The backend the daemon uses. `--features libusb` swaps in rusb without any
/// other code change.
#[cfg(not(feature = "libusb"))]
pub fn default_backend() -> impl UsbBackend {
    NusbBackend::new()
}
