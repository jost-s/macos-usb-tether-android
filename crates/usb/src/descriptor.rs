//! Owned, backend-independent USB descriptor snapshots.

use std::fmt;

/// Stable handle for one attached device, used to correlate hotplug events.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub u64);

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId(0x{:x})", self.0)
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:x}", self.0)
    }
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
}

impl DeviceInfo {
    /// `Vendor Product (vid:pid)`, for logs.
    pub fn label(&self) -> String {
        let name = match (&self.manufacturer, &self.product) {
            (Some(m), Some(p)) => format!("{m} {p}"),
            (None, Some(p)) => p.clone(),
            (Some(m), None) => m.clone(),
            (None, None) => "unknown device".to_string(),
        };
        format!("{name} ({:04x}:{:04x})", self.vendor_id, self.product_id)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    In,
    Out,
}

#[derive(Clone, Copy, Debug)]
pub struct EndpointDescriptor {
    pub address: u8,
    pub transfer_type: TransferType,
    pub max_packet_size: u16,
}

impl EndpointDescriptor {
    pub fn direction(&self) -> Direction {
        if self.address & 0x80 != 0 {
            Direction::In
        } else {
            Direction::Out
        }
    }
}

#[derive(Clone, Debug)]
pub struct InterfaceDescriptor {
    pub number: u8,
    pub alt_setting: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<EndpointDescriptor>,
    /// Class-specific descriptors trailing this interface, e.g. the CDC
    /// functional descriptors that name the paired data interface.
    pub extra: Vec<u8>,
}

impl InterfaceDescriptor {
    pub fn endpoint(&self, ty: TransferType, dir: Direction) -> Option<&EndpointDescriptor> {
        self.endpoints
            .iter()
            .find(|e| e.transfer_type == ty && e.direction() == dir)
    }
}

#[derive(Clone, Debug)]
pub struct ConfigDescriptor {
    pub value: u8,
    pub interfaces: Vec<InterfaceDescriptor>,
}
