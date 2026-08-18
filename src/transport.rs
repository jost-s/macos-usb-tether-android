//! `ControlTransport` over the USB control endpoint of the RNDIS control
//! interface, with the interrupt endpoint as the response signal.

use std::time::Duration;

use crate::rndis::error::{Error as RndisError, Result as RndisResult};
use crate::rndis::{wire, ControlTransport};
use crate::usb::{ControlSetup, InEndpoint, TransferType, UsbInterface};
use log::trace;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
/// Reads kept queued on the interrupt endpoint.
const NOTIFY_DEPTH: usize = 2;

pub struct UsbControlTransport {
    interface: Box<dyn UsbInterface>,
    interface_number: u16,
    notify: Option<Box<dyn InEndpoint>>,
}

impl UsbControlTransport {
    pub fn new(
        interface: Box<dyn UsbInterface>,
        interface_number: u8,
        interrupt_in: Option<u8>,
    ) -> RndisResult<Self> {
        let notify = match interrupt_in {
            Some(address) => {
                let mut ep = interface
                    .open_in(address, TransferType::Interrupt)
                    .map_err(|e| RndisError::Transport(e.to_string()))?;
                for _ in 0..NOTIFY_DEPTH {
                    ep.submit(8);
                }
                Some(ep)
            }
            None => None,
        };

        Ok(Self {
            interface,
            interface_number: interface_number as u16,
            notify,
        })
    }

    fn setup(&self, request: u8) -> ControlSetup {
        ControlSetup {
            request,
            value: 0,
            index: self.interface_number,
        }
    }
}

impl ControlTransport for UsbControlTransport {
    fn send(&mut self, msg: &[u8]) -> RndisResult<()> {
        self.interface
            .control_out(
                self.setup(wire::REQ_SEND_ENCAPSULATED),
                msg,
                CONTROL_TIMEOUT,
            )
            .map_err(|e| RndisError::Transport(e.to_string()))
    }

    fn receive(&mut self) -> RndisResult<Vec<u8>> {
        self.interface
            .control_in(
                self.setup(wire::REQ_GET_ENCAPSULATED),
                wire::MAX_CONTROL_MSG as u16,
                CONTROL_TIMEOUT,
            )
            .map_err(|e| RndisError::Transport(e.to_string()))
    }

    fn await_response(&mut self, timeout: Duration) -> RndisResult<()> {
        let Some(notify) = self.notify.as_mut() else {
            // No status endpoint: fall back to polling GET_ENCAPSULATED_RESPONSE.
            std::thread::sleep(timeout);
            return Ok(());
        };

        match notify.wait(timeout) {
            Some(Ok(data)) => {
                notify.submit(8);
                match wire::u32_at(&data, 0) {
                    Ok(wire::RESPONSE_AVAILABLE) => {}
                    Ok(other) => trace!("unexpected notification 0x{other:08x}"),
                    // A short notification is harmless; the caller polls anyway.
                    Err(_) => trace!("short notification ({} bytes)", data.len()),
                }
            }
            Some(Err(e)) => {
                notify.submit(8);
                return Err(RndisError::Transport(e.to_string()));
            }
            // Timed out. The caller still tries a read, matching how Linux
            // polls the control channel.
            None => {}
        }
        Ok(())
    }
}
