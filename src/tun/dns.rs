//! The tunnel's network service, published through SCDynamicStore.
//!
//! Publishing a service beats writing `/etc/resolv.conf`, which macOS
//! regenerates. It also earns the default route: macOS installs one via the
//! service's router while it ranks primary, and withdraws it in favour of a VPN
//! that outranks us. The servers go in as a catch-all supplemental resolver, so
//! they apply to every query without displacing anyone as primary.

use std::net::Ipv4Addr;

use anyhow::{bail, Result};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use log::{info, warn};
use system_configuration::dynamic_store::{SCDynamicStore, SCDynamicStoreBuilder};

/// Service id under `State:/Network/Service/`, ours alone.
const SERVICE_ID: &str = "muta";

pub struct Dns {
    store: SCDynamicStore,
    keys: Vec<String>,
}

impl Dns {
    /// Publish the tunnel as a network service with `servers` as its resolver.
    pub fn install(
        interface: &str,
        address: Ipv4Addr,
        router: Ipv4Addr,
        servers: &[Ipv4Addr],
        domain: Option<&str>,
    ) -> Result<Self> {
        let Some(store) = SCDynamicStoreBuilder::new("muta").build() else {
            bail!("could not open SCDynamicStore (is the daemon running as root?)");
        };

        let mut dns = Dns {
            store,
            keys: Vec::new(),
        };

        let ipv4 = dict(&[
            ("Addresses", array_of(&[address.to_string()])),
            ("SubnetMasks", array_of(&["255.255.255.255".to_string()])),
            ("Router", CFString::new(&router.to_string()).as_CFType()),
            ("InterfaceName", CFString::new(interface).as_CFType()),
        ]);
        dns.set(&format!("State:/Network/Service/{SERVICE_ID}/IPv4"), ipv4)?;

        // The service above is what carries the default route, so a lease
        // without resolvers still leaves a usable tunnel.
        if servers.is_empty() {
            warn!("lease carried no DNS servers; leaving the resolver alone");
            return Ok(dns);
        }

        let mut entries = vec![
            (
                "ServerAddresses",
                array_of(&servers.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            ),
            // An empty match domain makes these servers apply to all queries.
            ("SupplementalMatchDomains", array_of(&["".to_string()])),
        ];
        if let Some(domain) = domain {
            entries.push(("DomainName", CFString::new(domain).as_CFType()));
            entries.push(("SearchDomains", array_of(&[domain.to_string()])));
        }
        dns.set(
            &format!("State:/Network/Service/{SERVICE_ID}/DNS"),
            dict(&entries),
        )?;

        info!("DNS via {servers:?} on {interface}");
        Ok(dns)
    }

    fn set(&mut self, key: &str, value: CFDictionary<CFString, CFType>) -> Result<()> {
        if !self.store.set(key, value.to_untyped()) {
            bail!("SCDynamicStore rejected {key}");
        }
        self.keys.push(key.to_string());
        Ok(())
    }

    /// Remove our entries, restoring the previous resolver. Safe to call twice.
    pub fn remove(&mut self) {
        for key in self.keys.drain(..) {
            if !self.store.remove(&key as &str) {
                warn!("could not remove {key} from SCDynamicStore");
            }
        }
    }
}

impl Drop for Dns {
    fn drop(&mut self) {
        self.remove();
    }
}

fn dict(entries: &[(&str, CFType)]) -> CFDictionary<CFString, CFType> {
    CFDictionary::from_CFType_pairs(
        &entries
            .iter()
            .map(|(k, v)| (CFString::new(k), v.clone()))
            .collect::<Vec<_>>(),
    )
}

fn array_of(values: &[String]) -> CFType {
    CFArray::from_CFTypes(&values.iter().map(|v| CFString::new(v)).collect::<Vec<_>>()).as_CFType()
}
