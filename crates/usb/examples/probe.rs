//! Enumerate USB devices, dump descriptors, and report which one exposes RNDIS.
//!
//! `cargo run -p muta-usb --example probe [--watch]`

use muta_usb::{find_rndis, HotplugEvent, NusbBackend, UsbBackend};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let backend = NusbBackend::new();

    scan(&backend);

    if std::env::args().any(|a| a == "--watch") {
        println!("\nwatching for hotplug events (ctrl-c to stop)...");
        let mut watch = backend.watch()?;
        loop {
            match watch.next_event(Duration::from_secs(3600)) {
                Some(HotplugEvent::Connected(info)) => {
                    println!("\n+ connected: {}", info.label());
                    scan(&backend);
                }
                Some(HotplugEvent::Disconnected(id)) => println!("\n- disconnected: {id}"),
                None => {}
            }
        }
    }

    Ok(())
}

fn scan(backend: &NusbBackend) {
    let devices = match backend.list() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("list failed: {e}");
            return;
        }
    };

    let mut found = false;
    for info in devices {
        let device = match backend.open(info.id) {
            Ok(d) => d,
            Err(e) => {
                println!("{}: cannot open ({e})", info.label());
                continue;
            }
        };
        let configs = match device.configurations() {
            Ok(c) => c,
            Err(e) => {
                println!("{}: cannot read configs ({e})", info.label());
                continue;
            }
        };

        let rndis = find_rndis(&configs);
        if rndis.is_none() {
            continue;
        }
        found = true;

        println!("\n=== {} id={} ===", info.label(), info.id);
        for config in &configs {
            println!("  config {}", config.value);
            for iface in &config.interfaces {
                println!(
                    "    iface {}.{} class {:02x}/{:02x}/{:02x}",
                    iface.number, iface.alt_setting, iface.class, iface.subclass, iface.protocol
                );
                for ep in &iface.endpoints {
                    println!(
                        "      ep 0x{:02x} {:?} {:?} mps {}",
                        ep.address,
                        ep.transfer_type,
                        ep.direction(),
                        ep.max_packet_size
                    );
                }
            }
        }
        println!("  RNDIS: {:#?}", rndis.unwrap());
    }

    if !found {
        println!("no RNDIS device found — enable USB tethering on the phone");
    }
}
