//! The `utun` device and the system state that makes it the default route.

pub mod dns;
pub mod net;
pub mod utun;

pub use dns::Dns;
pub use net::{configure_interface, subnet_of, Routes};
pub use utun::Utun;
