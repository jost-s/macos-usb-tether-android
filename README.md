# rndis-tether

Userspace Android USB tethering for macOS on Apple Silicon. No kext, no System
Extension, no SIP changes.

macOS ships no RNDIS driver, so the phone's RNDIS interface sits unclaimed and
an ordinary userspace process can take it via IOKit. This daemon does that,
speaks RNDIS to the phone, and presents the result as a `utun` interface with a
DHCP-obtained address and the default route.

## Install

```sh
sudo ./install.sh
```

Then enable USB tethering on the phone. The daemon is a KeepAlive LaunchDaemon,
so it is always resident and connects on plug-in.

```sh
rndis-tetherctl status
tail -f /var/log/rndis-tether.log
```

To run it by hand instead:

```sh
cargo build --release
sudo ./target/release/rndis-tetherd -v
```

To remove:

```sh
sudo launchctl bootout system/dev.jost.rndis-tether
sudo rm /Library/LaunchDaemons/dev.jost.rndis-tether.plist
```

## How it works

RNDIS carries Ethernet frames; `utun` is IP-only. The daemon is the shim, and
keeps every layer-2 concern internal: it answers and issues ARP itself, runs its
own DHCP client over the RNDIS link, and passes only IP packets across the utun
boundary. The kernel therefore sees a plain point-to-point L3 interface.

```
 Android phone                                  macOS
┌──────────────┐   USB   ┌──────────────────────────────────────────────┐
│ RNDIS gadget │◄───────►│ nusb (UsbBackend trait)                      │
│  ctrl iface  │  bulk   │   └─ RNDIS layer  (control SM + data framing)│
│  0xE0/01/03  │  +ctrl  │        └─ L2 shim (Ethernet <-> IP, ARP, MAC)│
│  data iface  │  +intr  │             ├─ DHCP client (IP/gw/dns)       │
│  0x0A        │         │             └─ utun (L3) ── kernel routing   │
└──────────────┘         └──────────────────────────────────────────────┘
```

Routing uses the VPN split-default trick (`0.0.0.0/1` + `128.0.0.0/1`) so the
physical default route is never replaced, and teardown is a clean delete. DNS
goes through SCDynamicStore rather than `/etc/resolv.conf`.

The phone designates the MAC the host must use; the daemon adopts it, exactly as
Linux's `rndis_host.c` assigns the queried address to its own interface.

## Layout

| Crate      | What                                                            |
|------------|-----------------------------------------------------------------|
| `usb`      | `UsbBackend` trait, nusb backend, RNDIS interface matching       |
| `rndis`    | RNDIS messages, control state machine, `PACKET_MSG` framing      |
| `netstack` | Ethernet, ARP, IPv4/UDP, DHCP client                             |
| `tun`      | utun socket, interface/route config, DNS                         |
| `daemon`   | `rndis-tetherd`: hotplug, lifecycle, wiring                      |
| `ctl`      | `rndis-tetherctl`: status over a unix socket                     |

`rndis` and `netstack` are hardware-free and unit-tested against byte fixtures —
the parsing of device- and network-controlled data is where the bugs would bite.

## USB backend

The USB layer sits behind `UsbBackend`. nusb is the default; `--features libusb`
swaps in an rusb (libusb) backend with no other code change.

```sh
cargo build --release --features libusb
```

## Development

```sh
nix develop        # or use direnv
cargo test
cargo clippy --workspace --all-targets
```

`cargo run -p rndis-tether-usb --example probe` dumps descriptors for any
attached RNDIS device without claiming it — useful when a phone is not matched.
