//! Swappable USB layer: a backend-independent trait plus descriptor snapshots
//! and RNDIS interface matching.

pub mod backend;
pub mod descriptor;
pub mod error;
pub mod matcher;
mod nusb_backend;

pub use backend::{
    ControlSetup, HotplugEvent, HotplugWatch, InEndpoint, OutEndpoint, UsbBackend, UsbDevice,
    UsbInterface,
};
pub use descriptor::{DeviceInfo, TransferType};
pub use matcher::{find_rndis, RndisFunction};
pub use nusb_backend::NusbBackend;
