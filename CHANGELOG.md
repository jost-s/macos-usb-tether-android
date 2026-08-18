# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/jost-s/macos-usb-tether-android/releases/tag/v0.1.0) - 2026-08-18

### Added

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
