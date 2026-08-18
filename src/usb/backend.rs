//! The `UsbBackend` trait the rest of the daemon codes against.
//!
//! Endpoints are separate objects so each I/O thread can own one and keep
//! several transfers in flight without locking. The trait also erases nusb's
//! type-level endpoint parameters, letting the link layer hold a bulk and an
//! interrupt endpoint in the same shape.

use std::time::Duration;

use crate::usb::descriptor::{ConfigDescriptor, DeviceId, DeviceInfo, TransferType};
use crate::usb::error::Result;

/// A class request addressed to an interface, which is the only kind RNDIS
/// uses: SEND_ENCAPSULATED_COMMAND and GET_ENCAPSULATED_RESPONSE.
#[derive(Clone, Copy, Debug)]
pub struct ControlSetup {
    pub request: u8,
    pub value: u16,
    pub index: u16,
}

#[derive(Debug)]
pub enum HotplugEvent {
    Connected(DeviceInfo),
    Disconnected(DeviceId),
}

pub trait UsbBackend: Send + Sync + 'static {
    fn list(&self) -> Result<Vec<DeviceInfo>>;
    fn open(&self, id: DeviceId) -> Result<Box<dyn UsbDevice>>;
    fn watch(&self) -> Result<Box<dyn HotplugWatch>>;
}

pub trait HotplugWatch: Send {
    /// Blocks up to `timeout` for the next event.
    fn next_event(&mut self, timeout: Duration) -> Option<HotplugEvent>;
}

pub trait UsbDevice: Send + Sync {
    fn configurations(&self) -> Result<Vec<ConfigDescriptor>>;
    fn active_configuration(&self) -> Result<u8>;
    fn set_configuration(&self, value: u8) -> Result<()>;
    fn claim_interface(&self, number: u8) -> Result<Box<dyn UsbInterface>>;
}

pub trait UsbInterface: Send + Sync {
    fn set_alt_setting(&self, alt: u8) -> Result<()>;
    fn control_out(&self, setup: ControlSetup, data: &[u8], timeout: Duration) -> Result<()>;
    fn control_in(&self, setup: ControlSetup, len: u16, timeout: Duration) -> Result<Vec<u8>>;
    fn open_in(&self, address: u8, ty: TransferType) -> Result<Box<dyn InEndpoint>>;
    fn open_out(&self, address: u8, ty: TransferType) -> Result<Box<dyn OutEndpoint>>;
}

/// A queue of read transfers. Keep several submitted to cover USB round-trip
/// latency; `wait` returns them in completion order.
pub trait InEndpoint: Send {
    /// `len` is rounded up to a multiple of the max packet size, as USB requires.
    fn submit(&mut self, len: usize);
    fn wait(&mut self, timeout: Duration) -> Option<Result<Vec<u8>>>;
}

/// A queue of write transfers. `submit` never blocks; completions must be
/// reaped with `wait` to bound the number in flight.
pub trait OutEndpoint: Send {
    fn max_packet_size(&self) -> usize;
    fn pending(&self) -> usize;
    fn submit(&mut self, data: Vec<u8>);
    fn wait(&mut self, timeout: Duration) -> Option<Result<()>>;
}
