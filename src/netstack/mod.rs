//! The L2 shim: Ethernet framing, ARP, and a DHCP client.
//!
//! RNDIS carries Ethernet but `utun` is IP-only, so every layer-2 concern is
//! handled here rather than by the kernel. Hardware-free and unit-tested.

pub mod arp;
pub mod dhcp;
pub mod error;
pub mod ethernet;
pub mod ipv4;

pub use arp::Arp;
pub use dhcp::Lease;
pub use ethernet::MacAddr;
