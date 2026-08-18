//! Locating the RNDIS function in a device's descriptors.
//!
//! Matching is by interface descriptor, never VID/PID, so it works across phone
//! vendors. The control/data pairing follows the CDC Union functional
//! descriptor when present, as Linux's `cdc_parse_cdc_header` does.

use crate::usb::descriptor::{ConfigDescriptor, Direction, InterfaceDescriptor, TransferType};

/// CDC class-specific interface descriptor type (`CS_INTERFACE`).
const CS_INTERFACE: u8 = 0x24;
/// `bDescriptorSubtype` of the Union functional descriptor.
const CDC_UNION: u8 = 0x06;
/// CDC Data interface class.
const CLASS_CDC_DATA: u8 = 0x0A;

/// Interface signatures that identify an RNDIS control interface, matching the
/// `id_table` of Linux `rndis_host.c`.
const CONTROL_SIGNATURES: &[(u8, u8, u8)] = &[
    // Wireless controller / RNDIS — what Android's gadget advertises.
    (0xE0, 0x01, 0x03),
    // Communications / ACM / vendor-specific — Microsoft's original encoding.
    (0x02, 0x02, 0xFF),
];

/// Everything needed to drive one RNDIS function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RndisFunction {
    pub config_value: u8,
    pub control_interface: u8,
    pub data_interface: u8,
    pub data_alt_setting: u8,
    /// Carries RESPONSE_AVAILABLE notifications. Absent on devices that omit
    /// the status endpoint; the control layer then polls instead.
    pub interrupt_in: Option<u8>,
    pub bulk_in: u8,
    pub bulk_out: u8,
    pub bulk_max_packet_size: u16,
}

/// Find the first RNDIS function across all configurations.
pub fn find_rndis(configs: &[ConfigDescriptor]) -> Option<RndisFunction> {
    configs.iter().find_map(find_rndis_in_config)
}

fn find_rndis_in_config(config: &ConfigDescriptor) -> Option<RndisFunction> {
    config
        .interfaces
        .iter()
        .filter(|i| i.alt_setting == 0 && is_control_signature(i))
        .find_map(|control| build(config, control))
}

fn is_control_signature(i: &InterfaceDescriptor) -> bool {
    CONTROL_SIGNATURES.contains(&(i.class, i.subclass, i.protocol))
}

fn build(config: &ConfigDescriptor, control: &InterfaceDescriptor) -> Option<RndisFunction> {
    let data_number = union_subordinate(&control.extra).unwrap_or(control.number + 1);

    // Pick the alt setting that actually exposes the bulk pair; RNDIS normally
    // uses alt 0, but tolerate gadgets that hide the endpoints behind alt 1.
    let (data, bulk_in, bulk_out) = config
        .interfaces
        .iter()
        .filter(|i| i.number == data_number && i.class == CLASS_CDC_DATA)
        .find_map(|i| {
            let bin = i.endpoint(TransferType::Bulk, Direction::In)?;
            let bout = i.endpoint(TransferType::Bulk, Direction::Out)?;
            Some((i, *bin, *bout))
        })?;

    Some(RndisFunction {
        config_value: config.value,
        control_interface: control.number,
        data_interface: data.number,
        data_alt_setting: data.alt_setting,
        interrupt_in: control
            .endpoint(TransferType::Interrupt, Direction::In)
            .map(|e| e.address),
        bulk_in: bulk_in.address,
        bulk_out: bulk_out.address,
        bulk_max_packet_size: bulk_in.max_packet_size.max(bulk_out.max_packet_size),
    })
}

/// `bSubordinateInterface0` of the CDC Union functional descriptor.
fn union_subordinate(extra: &[u8]) -> Option<u8> {
    let mut rest = extra;
    while let Some(&len) = rest.first() {
        // A zero or truncated length would not advance; stop rather than spin.
        if len < 2 || len as usize > rest.len() {
            return None;
        }
        let (desc, tail) = rest.split_at(len as usize);
        if desc[1] == CS_INTERFACE && desc.len() >= 5 && desc[2] == CDC_UNION {
            return Some(desc[4]);
        }
        rest = tail;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::descriptor::EndpointDescriptor;

    fn ep(address: u8, transfer_type: TransferType) -> EndpointDescriptor {
        EndpointDescriptor {
            address,
            transfer_type,
            max_packet_size: 512,
        }
    }

    fn control_iface(number: u8, sig: (u8, u8, u8), extra: Vec<u8>) -> InterfaceDescriptor {
        InterfaceDescriptor {
            number,
            alt_setting: 0,
            class: sig.0,
            subclass: sig.1,
            protocol: sig.2,
            endpoints: vec![ep(0x81, TransferType::Interrupt)],
            extra,
        }
    }

    fn data_iface(number: u8) -> InterfaceDescriptor {
        InterfaceDescriptor {
            number,
            alt_setting: 0,
            class: CLASS_CDC_DATA,
            subclass: 0,
            protocol: 0,
            endpoints: vec![ep(0x82, TransferType::Bulk), ep(0x02, TransferType::Bulk)],
            extra: vec![],
        }
    }

    /// Header + Call Management + ACM + Union(0, 1), as an Android gadget sends it.
    fn android_cdc_extra() -> Vec<u8> {
        vec![
            0x05,
            CS_INTERFACE,
            0x00,
            0x10,
            0x01, // Header
            0x05,
            CS_INTERFACE,
            0x01,
            0x00,
            0x01, // Call Management
            0x04,
            CS_INTERFACE,
            0x02,
            0x00, // ACM
            0x05,
            CS_INTERFACE,
            CDC_UNION,
            0x00,
            0x01, // Union: master 0, slave 1
        ]
    }

    #[test]
    fn matches_android_rndis_gadget() {
        let config = ConfigDescriptor {
            value: 1,
            interfaces: vec![
                control_iface(0, (0xE0, 0x01, 0x03), android_cdc_extra()),
                data_iface(1),
            ],
        };

        assert_eq!(
            find_rndis(&[config]),
            Some(RndisFunction {
                config_value: 1,
                control_interface: 0,
                data_interface: 1,
                data_alt_setting: 0,
                interrupt_in: Some(0x81),
                bulk_in: 0x82,
                bulk_out: 0x02,
                bulk_max_packet_size: 512,
            })
        );
    }

    #[test]
    fn matches_microsoft_comm_acm_vendor_signature() {
        let config = ConfigDescriptor {
            value: 1,
            interfaces: vec![
                control_iface(0, (0x02, 0x02, 0xFF), android_cdc_extra()),
                data_iface(1),
            ],
        };
        assert!(find_rndis(&[config]).is_some());
    }

    #[test]
    fn union_descriptor_wins_over_adjacency() {
        // Union names interface 3, which is not the interface following the
        // control one. Interface 1 is a decoy CDC-data interface.
        let mut extra = android_cdc_extra();
        let last = extra.len() - 1;
        extra[last] = 3;

        let config = ConfigDescriptor {
            value: 1,
            interfaces: vec![
                control_iface(0, (0xE0, 0x01, 0x03), extra),
                data_iface(1),
                data_iface(3),
            ],
        };

        let f = find_rndis(&[config]).expect("matched");
        assert_eq!(f.data_interface, 3);
    }

    #[test]
    fn falls_back_to_adjacent_interface_without_union() {
        let config = ConfigDescriptor {
            value: 1,
            interfaces: vec![control_iface(2, (0xE0, 0x01, 0x03), vec![]), data_iface(3)],
        };

        let f = find_rndis(&[config]).expect("matched");
        assert_eq!(f.control_interface, 2);
        assert_eq!(f.data_interface, 3);
    }

    #[test]
    fn picks_alt_setting_that_exposes_the_bulk_pair() {
        let mut alt0 = data_iface(1);
        alt0.endpoints.clear();
        let mut alt1 = data_iface(1);
        alt1.alt_setting = 1;

        let config = ConfigDescriptor {
            value: 1,
            interfaces: vec![
                control_iface(0, (0xE0, 0x01, 0x03), android_cdc_extra()),
                alt0,
                alt1,
            ],
        };

        let f = find_rndis(&[config]).expect("matched");
        assert_eq!(f.data_alt_setting, 1);
    }

    #[test]
    fn finds_rndis_in_a_non_first_configuration() {
        let mtp_only = ConfigDescriptor {
            value: 1,
            interfaces: vec![InterfaceDescriptor {
                number: 0,
                alt_setting: 0,
                class: 0x06,
                subclass: 0x01,
                protocol: 0x01,
                endpoints: vec![],
                extra: vec![],
            }],
        };
        let tether = ConfigDescriptor {
            value: 2,
            interfaces: vec![
                control_iface(0, (0xE0, 0x01, 0x03), android_cdc_extra()),
                data_iface(1),
            ],
        };

        let f = find_rndis(&[mtp_only, tether]).expect("matched");
        assert_eq!(f.config_value, 2);
    }

    #[test]
    fn ignores_non_rndis_devices() {
        // CDC-ECM: communications class, ECM subclass — macOS handles these natively.
        let config = ConfigDescriptor {
            value: 1,
            interfaces: vec![control_iface(0, (0x02, 0x06, 0x00), vec![]), data_iface(1)],
        };
        assert_eq!(find_rndis(&[config]), None);
    }

    #[test]
    fn rejects_control_interface_with_no_data_interface() {
        let config = ConfigDescriptor {
            value: 1,
            interfaces: vec![control_iface(0, (0xE0, 0x01, 0x03), android_cdc_extra())],
        };
        assert_eq!(find_rndis(&[config]), None);
    }

    #[test]
    fn union_parser_rejects_malformed_lengths() {
        assert_eq!(
            union_subordinate(&[0x00, CS_INTERFACE, CDC_UNION, 0, 1]),
            None
        );
        // Length runs past the end of the buffer.
        assert_eq!(
            union_subordinate(&[0x40, CS_INTERFACE, CDC_UNION, 0, 1]),
            None
        );
        // Union descriptor truncated to 4 bytes has no subordinate field.
        assert_eq!(union_subordinate(&[0x04, CS_INTERFACE, CDC_UNION, 0]), None);
    }
}
