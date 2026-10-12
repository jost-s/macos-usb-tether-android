# Changelog

## [0.1.3](https://github.com/jost-s/macos-usb-tether-android/compare/v0.1.2...v0.1.3) (2026-10-12)


### Bug Fixes

* **usb:** clear a stalled bulk endpoint and resume ([5279b2e](https://github.com/jost-s/macos-usb-tether-android/commit/5279b2ea6db86aadc0c843bb406e5d55b4deebb5))


### Continuous Integration

* refactor to release please ([d42f6b3](https://github.com/jost-s/macos-usb-tether-android/commit/d42f6b36ce18a510f1323fac1073f40deede1975))

## [0.1.2](https://github.com/jost-s/macos-usb-tether-android/compare/v0.1.1...v0.1.2) - 2026-08-21

### Added

- *(daemon)* say when a phone is attached but not tethering

## [0.1.1](https://github.com/jost-s/macos-usb-tether-android/compare/v0.1.0...v0.1.1) - 2026-08-20

### Fixed

- *(dns)* stop answering every query while a VPN is up
- *(net)* yield routing to a VPN layered on the tether

### Other

- describe how routing ranks against VPNs and Wi-Fi

## [0.1.0](https://github.com/jost-s/macos-usb-tether-android/releases/tag/v0.1.0) - 2026-08-18

### Added

- *(ci)* release when the prepare-release pull request is merged
- *(ci)* publish to crates.io and attach a universal macOS binary
- *(daemon)* report frames-per-transfer so TX batching is measurable
- *(usb)* add rusb fallback backend behind the libusb feature
- *(tun)* add utun, split-default routing, and DNS via SCDynamicStore
- *(daemon)* add link layer with RX/TX threads, ARP, and DHCP
- *(netstack)* add Ethernet framing, ARP, IPv4/UDP, and a DHCP client
- *(daemon)* bring up the RNDIS link over the USB control endpoint
- *(rndis)* add wire format, packet framing, and control state machine
- *(usb)* add backend trait, nusb backend, and RNDIS interface matching

### Fixed

- *(ci)* open the release pull request on demand
- correct the signal handler cast and the declared MSRV
- *(service)* retry launchctl bootstrap while the old job exits
- *(daemon)* make the ctl socket reachable by non-root and report frames sent
- *(daemon)* fail fast without root instead of retrying utun forever
- *(daemon)* use the gadget's designated host MAC

### Other

- collapse the workspace into a single muta crate
- remove the planning document from the repository
- *(ci)* name the workflow and jobs for what they do
- use com.github.jost-s namespace for the service label and repo URL
- add LICENSE, CI, and rewrite README around muta
- collapse to a single muta binary with subcommands
- *(daemon)* size TX batch buffers from actual use, not the transfer maximum
- *(daemon)* batch TX frames per device limits, writev to utun, deeper RX queue
