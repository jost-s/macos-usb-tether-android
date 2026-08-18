# muta

Android USB tethering for macOS, in userspace. No kext, no System Extension,
no SIP changes.

macOS ships no RNDIS driver, and Android's USB tethering speaks RNDIS. The
classic fix, HoRNDIS, was a kext and is dead on Apple Silicon. The reason a
*userspace* driver works at all is the same reason the problem exists: because
macOS has no in-kernel RNDIS driver, the phone's RNDIS interface sits
**unclaimed**, so a normal process can take it via IOKit.

`muta` claims it, speaks RNDIS to the phone, and presents the result as a `utun`
interface with a DHCP-obtained address and the default route.

## Install

With a Rust toolchain:

```sh
cargo install muta
```

Without one, take the prebuilt binary. It runs on both Apple Silicon and Intel:

```sh
curl -fsSL https://github.com/jost-s/macos-usb-tether-android/releases/latest/download/muta-universal-apple-darwin.tar.gz | tar xz
```

Download it with `curl`, not a browser. The binaries are unsigned, and a browser
marks downloads with `com.apple.quarantine`, after which Gatekeeper refuses to
run them; `curl` and `tar` do not. If you already downloaded through a browser,
clear it with `xattr -d com.apple.quarantine muta`.

Then enable **USB tethering** on the phone (on stock Android: Settings →
Network & internet → Hotspot & tethering → USB tethering).

### Try it

Runs in the foreground; Ctrl-C restores your routes and DNS.

```sh
sudo muta run -v
```

### Keep it

Installs a LaunchDaemon, so it starts at boot — before anyone logs in — and
waits for the phone. Plug in, enable tethering, and it connects on its own.

```sh
sudo muta install
muta status              # no root needed
tail -f /var/log/muta.log
sudo muta uninstall
```

`muta install` copies the binary to a root-owned `/usr/local/libexec/muta/`
rather than `/usr/local/bin`, which is group-writable by `admin` on stock macOS
— a root-executed binary must not sit somewhere a non-root process can replace
it. It refuses to install if it cannot secure that directory.

## How it works

RNDIS carries Ethernet frames; `utun` is IP-only. `muta` is the shim between
them and keeps every layer-2 concern internal: it answers and issues ARP itself,
runs its own DHCP client over the RNDIS link, and passes only IP packets across
the utun boundary. The kernel therefore sees an ordinary point-to-point L3
interface.

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
physical default route is never replaced and teardown is a clean delete. DNS
goes through SCDynamicStore, not `/etc/resolv.conf`.

Two details govern interoperability with the phone:

- **The phone designates the host's MAC.** `OID_802_3_PERMANENT_ADDRESS` returns
  the address the gadget expects *us* to use, and `muta` adopts it — exactly as
  Linux's `rndis_host.c` assigns the queried address to its own interface.
  Synthesizing our own instead means broadcast works and unicast silently does not.
- **Transmit batching is bounded by what the device advertises.** The stock Linux
  gadget parses exactly one packet per USB transfer and reports
  `MaxPacketsPerTransfer = 1`; some vendors patch theirs to aggregate. `muta`
  batches only up to the advertised limit, so it is correct on both.

Device matching is by interface descriptor (`0xE0/0x01/0x03` plus a paired CDC
data interface, or the Microsoft `0x02/0x02/0xFF` encoding), never by VID/PID,
so it works across vendors. The control/data pairing follows the CDC Union
functional descriptor.

## Layout

| Module     | What                                                       |
|------------|------------------------------------------------------------|
| `usb`      | `UsbBackend` trait, nusb backend, RNDIS interface matching  |
| `rndis`    | RNDIS messages, control state machine, `PACKET_MSG` framing |
| `netstack` | Ethernet, ARP, IPv4/UDP, DHCP client                        |
| `tun`      | utun socket, interface/route config, DNS                    |
| top level  | hotplug, lifecycle, service install                         |

`rndis` and `netstack` are hardware-free and unit-tested against byte fixtures.
That is deliberate: parsing device- and network-controlled binary data is where
the bugs bite, and it is testable without a phone attached. Neither imports
`usb` or `tun`, which is what keeps them that way.

## Development

```sh
nix develop        # or use direnv
cargo test
cargo clippy --all-targets
```

`muta probe` dumps descriptors for any attached RNDIS device without claiming
it — start there if a phone is not matched.

## Status

Built and verified against a Xiaomi Redmi Note 10S on Apple Silicon: bring-up,
DHCP, routing, DNS, throughput, hot unplug and reconnect. Intel builds and runs,
but has not been tested against hardware. iPhone tethering is out of scope — it
is a different, proprietary protocol.

## License

MIT
