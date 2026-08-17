# Userspace RNDIS Tethering Driver for macOS (Apple Silicon)

## Context

You want to tether an Android phone's internet over USB on an Apple Silicon Mac.
macOS ships **no RNDIS driver** (only CDC-ECM), and Android's USB tethering speaks
**RNDIS** (Microsoft's Remote NDIS). The classic fix, HoRNDIS, is a **kext** and is
dead on Apple Silicon. The reason a *userspace* driver is possible at all: because
macOS has no in-kernel RNDIS driver, the phone's RNDIS USB interface sits **unclaimed**,
so a normal userspace process can claim it via IOKit — **no kext, no System Extension,
no SIP changes.**

The existing userspace repos (`ReRNDIS`, `android-usb-tether-macos`, `TetherKit`) are
either GPL kext-replacements or visibly AI-generated code of unknown rigor. We will
**build from scratch**, correctness-first, and validate the protocol against
**authoritative references only**: the Linux kernel `drivers/net/usb/rndis_host.c` +
`cdc.h`, the Microsoft `[MS-RNDIS]` spec, and HoRNDIS's control-flow sequence. The
Clauded repos are read-only inspiration, never a foundation.

**Outcome:** a robust, efficient Rust daemon that auto-detects the phone when USB
tethering is switched on, brings up a `utun` interface with a DHCP-obtained address,
and makes the phone the default route — started automatically via a LaunchDaemon.

## Goals / Non-goals

- **Goal:** Reliable Android RNDIS tethering on Apple Silicon (and Intel, for free),
  auto-connect on plug-in, good sustained throughput, clean teardown on unplug.
- **Goal:** USB backend swappable behind a trait (nusb default, rusb/libusb fallback).
- **Non-goal (v1):** iPhone tethering (that's a different, proprietary usbmuxd/CDC-NCM
  path), IPv6-only networks, GUI menu-bar app (CLI daemon + `ctl` only for now).

## Architecture

```
 Android phone                                  macOS
┌──────────────┐   USB   ┌──────────────────────────────────────────────┐
│ RNDIS gadget │◄───────►│ nusb (UsbBackend trait)                       │
│  ctrl iface  │  bulk   │   └─ RNDIS layer  (control SM + data framing) │
│  0xE0/01/03  │  +ctrl  │        └─ L2 shim (Ethernet <-> IP, ARP, MAC) │
│  data iface  │  +intr  │             ├─ DHCP client (get IP/gw/dns)    │
│  0x0A        │         │             └─ utun (L3) ── kernel routing    │
└──────────────┘         └──────────────────────────────────────────────┘
```

**The key insight (L2↔L3 bridge).** RNDIS carries **Ethernet (L2)** frames; `utun` is
**IP-only (L3)**. The daemon is the shim between them and keeps all L2 concerns
*internal*: it answers/issues **ARP** itself, runs its own **DHCP client** over the
RNDIS side to learn IP/gateway/DNS, and only ever passes clean IP packets across the
utun boundary. The kernel therefore sees an ordinary point-to-point L3 interface with
an address and a default route — this is why `utun` works despite the layer mismatch,
and it's cleaner than the `feth`+bpf+AF_NDRV alternative.

## Tech stack & repo layout

- **Language:** Rust — chosen deliberately over C (decision settled, not to be re-litigated).
  C is modestly simpler for the OS plumbing, but this driver's real risk is (a) parsing
  device/network-controlled binary data (RNDIS `DataOffset`/`DataLength`, DHCP options,
  ARP/Ethernet — a documented overread/overflow class with real RNDIS CVEs) and (b) USB
  buffer lifetimes across hot-unplug. Rust's bounds-checking and ownership neutralize both
  by construction, exactly where it matters.
- **USB:** `nusb` (pure-Rust, talks to IOKit directly, async).
  **Everything else:** `libc` + raw BSD sockets; `system-configuration` crate for
  DNS/route via SCDynamicStore.
- **The libusb fallback, clarified.** The USB layer sits behind a `UsbBackend` trait.
  The fallback if nusb underperforms is **`rusb` — which is a thin Rust binding over the
  exact same `libusb` C library** a hand-written C driver would use, hitting identical
  IOKit calls with identical performance. So "swap to C + libusb" is achieved *inside
  Rust* by switching one module (`nusb` → `rusb`), keeping 100% of the RNDIS/DHCP/ARP/
  utun/daemon code. A full C rewrite is **unnecessary, not merely inconvenient** — the
  only Rust-specific risk is nusb, and the trait isolates it entirely.
- **Optional de-risking spike (throwaway):** before building the real daemon, a ~50-line
  C+libusb (or Rust+rusb) program that opens the phone and confirms it answers RNDIS
  `INITIALIZE`. Proves the USB/protocol path end-to-end with minimal investment; discarded.
- **Dev env:** **Nix flake** (no brew). Rust toolchain via `oxalica/rust-overlay`;
  provides `libusb` too, only needed when the `libusb` fallback feature is built.

Proposed location: `/Users/jost/Desktop/dev/rndis-tether/` (Cargo workspace):

```
rndis-tether/
  flake.nix  .envrc                     # nix dev shell (rust, libusb for fallback)
  Cargo.toml                            # workspace
  crates/
    usb/         # UsbBackend trait + nusb impl (default) + rusb impl (feature "libusb")
    rndis/       # RNDIS messages, control state machine, PACKET_MSG framing — PURE, unit-tested
    netstack/    # Ethernet framing, ARP responder/resolver, minimal DHCP client — PURE, unit-tested
    tun/         # utun socket create/rw + interface/route/DNS config (libc, system-configuration)
    daemon/      # bin `rndis-tetherd`: hotplug watch, lifecycle, wires layers together
    ctl/         # bin `rndis-tetherctl`: status/connect/disconnect over a unix socket
  launchd/dev.jost.rndis-tether.plist   # KeepAlive LaunchDaemon (runs as root)
  install.sh                            # cargo build --release, copy bin + plist, launchctl load
```

Splitting `rndis` and `netstack` as **pure, hardware-free crates** is deliberate: the
protocol logic (the part that must be *correct*) is fully unit-testable against captured
byte sequences without a phone attached.

## Component detail

### 1. `usb` — swappable backend
- `trait UsbBackend`: `list()/watch()` (hotplug), `open`, `claim_interface`,
  `control_transfer`, `submit_bulk_in/out` (async, pooled), `interrupt_in`.
- Default impl over **nusb** (`nusb::watch_devices`, `Interface::bulk_in/out`,
  queue-based async for a submission pool). Fallback impl over **rusb** (= Rust bindings
  to the same `libusb` C library) behind `--features libusb`. Daemon codes only against
  the trait — nusb is never load-bearing, and switching to libusb is a one-module change,
  not a rewrite.

### 2. Device discovery & hotplug (`daemon`)
- Match the **RNDIS interface descriptor pair**, not VID/PID: control interface
  class `0xE0`/sub `0x01`/proto `0x03` + paired CDC-data interface class `0x0A`
  (fall back to matching the IAD). Works across phone vendors. The device only
  appears once the user enables "USB tethering" on the phone — that's the hotplug
  trigger. Handle multi-config devices (select the config exposing RNDIS).

### 3. `rndis` — protocol layer (validate vs Linux `rndis_host.c`)
- **Control (via SEND_ENCAPSULATED_COMMAND `0x21/0x00` / GET_ENCAPSULATED_RESPONSE
  `0xA1/0x01`, response signalled by RESPONSE_AVAILABLE on the interrupt EP):**
  `INITIALIZE_MSG`→cmplt (negotiate MaxTransferSize), `QUERY_MSG` OID_802_3_CURRENT_ADDRESS
  (learn phone MAC), `SET_MSG` OID_GEN_CURRENT_PACKET_FILTER = `0x2F` to bring the link
  up, periodic `KEEPALIVE_MSG`, and handle device-initiated `INDICATE_STATUS_MSG`
  (media connect/disconnect) + keepalives.
- **Data:** `PACKET_MSG` (type `0x1`) header {MessageLength, DataOffset, DataLength} +
  Ethernet frame; support **multi-packet aggregation** in one bulk transfer for throughput.

### 4. `netstack` — L2 shim
- Synthesize a stable local MAC for the Mac side. **ARP:** reply to requests for our IP;
  resolve/cache the gateway MAC. **Framing:** wrap outbound utun IP packets in Ethernet
  (dst=gw MAC, src=our MAC) → RNDIS; unwrap inbound → strip Ethernet, forward IP to utun.
- **DHCP client:** DISCOVER→OFFER→REQUEST→ACK to Android's tether server
  (typically gw `192.168.42.129`), yielding IP/netmask/gateway/DNS/lease. Renew on timer.

### 5. `tun` — utun + system config
- Create `utun` via `socket(PF_SYSTEM, SOCK_DGRAM, SYSPROTO_CONTROL)` +
  `com.apple.net.utun_control` (remember the **4-byte AF header** on every read/write).
- Configure: `ifconfig utunN <ip> <gw> up`, MTU 1500. **Default route via the VPN
  split-default trick** (`0.0.0.0/1` + `128.0.0.0/1` through the tunnel) so we take over
  routing without clobbering the physical default — clean restore on teardown. **DNS**
  via SCDynamicStore (`system-configuration`), not editing resolv.conf.

### 6. `daemon` lifecycle + auto-connect
- **Privilege model:** utun + route + DNS require **root**; USB claim of an unclaimed
  interface does not. Daemon runs as **root under a KeepAlive LaunchDaemon**, so it's
  always resident and reacts to hotplug itself (no per-plug launchd trigger).
- On attach: claim → RNDIS bring-up → DHCP → utun up → routes/DNS. On detach or error:
  tear down utun, restore routes/DNS, wait for next attach. `ctl` binary reports status
  over a unix socket.

## Verification (end-to-end)

1. **No hardware:** `cargo test` — RNDIS message encode/decode and DHCP/ARP parsing
   against captured byte fixtures; `cargo clippy`.
2. **Bring-up:** plug phone, enable USB tethering, run `sudo rndis-tetherd --foreground -v`.
   Confirm in logs: device matched → INITIALIZE cmplt → phone MAC learned → link filter set
   → DHCP ACK with IP/gw/dns → `utunN` up with address → default route installed.
3. **Traffic:** `ping 8.8.8.8`, then `curl -sI https://example.com` (proves DNS + routing).
   `ifconfig utunN` and `netstat -rn` show expected state.
4. **Throughput/CPU:** `iperf3`/speedtest; tune async bulk pool sizes; watch CPU. If nusb
   underperforms, flip to `--features libusb` (rusb) behind the same trait and re-measure.
5. **Hotplug/robustness:** unplug/replug and toggle tethering off/on — confirm clean
   teardown (routes/DNS restored) and automatic reconnect. Then install the LaunchDaemon
   (`./install.sh`) and confirm auto-connect from a cold plug-in with no terminal open.

## Milestones (each independently verifiable)

1. Nix shell + workspace skeleton; `UsbBackend` trait + nusb enumerate/match RNDIS device (print descriptors).
2. RNDIS control bring-up (INIT / QUERY MAC / SET filter) + keepalive; log "link up".
3. RX path: parse RNDIS PACKET_MSG → Ethernet; ARP responder + DHCP client obtain a lease (log only, no utun yet).
4. utun up; wire IP↔RNDIS both directions; manual route → `ping` works.
5. Automated routing (split-default) + DNS via SCDynamicStore.
6. Daemon lifecycle + nusb hotplug + LaunchDaemon auto-connect + `ctl` status.
7. Robustness pass: error recovery, teardown/restore, async pool tuning for throughput.

## Open risks to confirm during build

- **nusb macOS async throughput/hotplug maturity** — the *only* Rust-specific risk, and
  the whole reason for the swappable trait. Mitigation is `rusb` (the same libusb a C
  driver would use), reached by changing one module — no language/rewrite decision needed.
  De-risk early with the optional spike above.
- **USB permission prompts** on recent macOS for userspace claim of the interface (expected
  none for an unclaimed RNDIS iface, but verify early).
- **DHCP vs static** — Android's `192.168.42.x` range is predictable; if a phone's DHCP is
  flaky we can fall back to static config as a safety net.
