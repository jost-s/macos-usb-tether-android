//! The L2 shim in motion: threads that move frames between the RNDIS
//! endpoints, ARP, DHCP, and the IP sink.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::{debug, error, info, trace, warn};
use rndis_tether_netstack::dhcp::{self, DhcpClient, Event as DhcpEvent, Lease};
use rndis_tether_netstack::ethernet::{self, MacAddr, ETHERTYPE_ARP, ETHERTYPE_IPV4};
use rndis_tether_netstack::ipv4::{self, UdpDatagram};
use rndis_tether_netstack::Arp;
use rndis_tether_rndis::{packet, wire};
use rndis_tether_usb::{InEndpoint, OutEndpoint};

/// Reads kept queued on the bulk IN endpoint to cover USB round-trip latency.
const RX_DEPTH: usize = 16;
/// Cap on writes in flight before the TX thread waits for a completion.
const TX_DEPTH: usize = 8;
/// How long threads block before re-checking the shutdown flag.
const TICK: Duration = Duration::from_millis(200);
/// DHCP retransmit backoff, capped.
const DHCP_RETRY_MIN: Duration = Duration::from_secs(1);
const DHCP_RETRY_MAX: Duration = Duration::from_secs(16);

/// Where inbound IP packets go. Replaced by the utun writer once it exists.
pub trait IpSink: Send + Sync {
    fn deliver(&self, packet: &[u8]);
}

/// The RX thread starts before the tunnel exists, so the real sink is swapped
/// in once utun is up. Packets arriving before that are counted and dropped.
#[derive(Default)]
pub struct SwitchableSink {
    inner: Mutex<Option<Arc<dyn IpSink>>>,
    pub delivered: AtomicU64,
    pub bytes_in: AtomicU64,
    pub dropped: AtomicU64,
}

impl SwitchableSink {
    pub fn attach(&self, sink: Arc<dyn IpSink>) {
        *self.inner.lock().expect("sink lock") = Some(sink);
    }

    pub fn detach(&self) {
        *self.inner.lock().expect("sink lock") = None;
    }
}

impl IpSink for SwitchableSink {
    fn deliver(&self, packet: &[u8]) {
        match &*self.inner.lock().expect("sink lock") {
            Some(sink) => {
                self.delivered.fetch_add(1, Ordering::Relaxed);
                self.bytes_in
                    .fetch_add(packet.len() as u64, Ordering::Relaxed);
                sink.deliver(packet);
            }
            None => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// What the device said it can accept in one bulk transfer.
#[derive(Clone, Copy, Debug)]
pub struct TxLimits {
    pub max_transfer_size: usize,
    /// `MaxPacketsPerTransfer`. The stock Linux gadget reports 1, which
    /// disables batching; vendors that patched theirs report more.
    pub max_packets: usize,
    pub alignment: usize,
}

#[derive(Debug)]
pub enum LinkEvent {
    Bound(Box<Lease>),
    /// The link failed; the daemon should tear down and wait for a re-attach.
    Failed(String),
}

/// Queue of Ethernet frames waiting for the bulk OUT endpoint.
#[derive(Clone)]
pub struct FrameSender(Sender<Vec<u8>>);

impl FrameSender {
    pub fn send(&self, frame: Vec<u8>) {
        // A closed channel means the link is being torn down.
        let _ = self.0.send(frame);
    }
}

pub struct Link {
    pub host_mac: MacAddr,
    /// Frames handed to the bulk OUT endpoint.
    pub sent: Arc<AtomicU64>,
    pub bytes_out: Arc<AtomicU64>,
    pub arp: Arc<Mutex<Arp>>,
    pub tx: FrameSender,
    pub events: Receiver<LinkEvent>,
    shutdown: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Link {
    /// Start the RX and TX threads and begin DHCP.
    pub fn start(
        bulk_in: Box<dyn InEndpoint>,
        bulk_out: Box<dyn OutEndpoint>,
        host_mac: MacAddr,
        limits: TxLimits,
        rx_transfer_size: u32,
        sink: Arc<dyn IpSink>,
    ) -> Self {
        info!("host MAC {host_mac} on the RNDIS link");

        let arp = Arc::new(Mutex::new(Arp::new(host_mac, Ipv4Addr::UNSPECIFIED)));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (frame_tx, frame_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let tx = FrameSender(frame_tx);
        let sent = Arc::new(AtomicU64::new(0));
        let bytes_out = Arc::new(AtomicU64::new(0));

        let threads = vec![
            spawn(
                "rndis-tx",
                tx_loop(
                    bulk_out,
                    frame_rx,
                    limits,
                    sent.clone(),
                    bytes_out.clone(),
                    shutdown.clone(),
                ),
            ),
            spawn(
                "rndis-rx",
                rx_loop(
                    bulk_in,
                    rx_transfer_size,
                    RxContext {
                        host_mac,
                        arp: arp.clone(),
                        tx: tx.clone(),
                        sink,
                        events: event_tx,
                    },
                    shutdown.clone(),
                ),
            ),
        ];

        Self {
            host_mac,
            sent,
            bytes_out,
            arp,
            tx,
            events: event_rx,
            shutdown,
            threads,
        }
    }

    pub fn shutdown(self) {
        self.shutdown.store(true, Ordering::Relaxed);
        drop(self.tx);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(body)
        .expect("spawning a link thread")
}

/// Packs outbound frames into as few bulk transfers as the device allows.
struct Tx {
    bulk_out: Box<dyn OutEndpoint>,
    limits: TxLimits,
    max_packet: usize,
    batch: Vec<u8>,
    packets: u64,
    sent: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
}

impl Tx {
    /// Whether `frame` still fits the transfer being built.
    fn fits(&self, frame: &[u8]) -> bool {
        self.packets < self.limits.max_packets as u64
            && self.batch.len() + wire::DATA_HEADER_LEN + frame.len()
                <= self.limits.max_transfer_size
    }

    fn push(&mut self, frame: &[u8]) {
        packet::append(&mut self.batch, frame, self.limits.alignment);
        self.packets += 1;
    }

    fn flush(&mut self) {
        if self.packets == 0 {
            return;
        }
        // A transfer that is an exact multiple of the packet size would need a
        // zero-length packet to terminate; one byte past the last msg_len is
        // cheaper, and every parser stops before a partial header.
        if self.batch.len() % self.max_packet == 0 {
            self.batch.push(0);
        }

        // Bound the queue so a stalled endpoint cannot grow it without limit.
        while self.bulk_out.pending() >= TX_DEPTH {
            if self.bulk_out.wait(TICK).is_none() && self.shutdown.load(Ordering::Relaxed) {
                return;
            }
        }

        self.bytes
            .fetch_add(self.batch.len() as u64, Ordering::Relaxed);
        self.sent.fetch_add(self.packets, Ordering::Relaxed);
        // Submitting hands the buffer away, so leave a fresh one of the same
        // size rather than letting the next batch start from zero capacity.
        let full = std::mem::replace(
            &mut self.batch,
            Vec::with_capacity(self.limits.max_transfer_size),
        );
        self.bulk_out.submit(full);
        self.packets = 0;
        reap(self.bulk_out.as_mut(), Duration::ZERO);
    }
}

fn tx_loop(
    bulk_out: Box<dyn OutEndpoint>,
    frames: Receiver<Vec<u8>>,
    limits: TxLimits,
    sent: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
) -> impl FnOnce() {
    move || {
        let mut tx = Tx {
            max_packet: bulk_out.max_packet_size().max(1),
            batch: Vec::with_capacity(limits.max_transfer_size),
            bulk_out,
            limits,
            packets: 0,
            sent,
            bytes,
            shutdown: shutdown.clone(),
        };

        while !shutdown.load(Ordering::Relaxed) {
            let frame = match frames.recv_timeout(TICK) {
                Ok(f) => f,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    reap(tx.bulk_out.as_mut(), Duration::ZERO);
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            // Fill one transfer with whatever is already queued, then keep
            // draining so a burst costs one transfer per batch, not per frame.
            let mut next = Some(frame);
            while let Some(frame) = next.take() {
                if !tx.fits(&frame) {
                    tx.flush();
                    if !tx.fits(&frame) {
                        warn!(
                            "dropping a {}-byte frame: over the device limit",
                            frame.len()
                        );
                        break;
                    }
                }
                tx.push(&frame);
                next = frames.try_recv().ok();
            }
            tx.flush();
        }

        // Let anything already queued finish before the endpoint is dropped.
        reap(tx.bulk_out.as_mut(), Duration::from_millis(500));
    }
}

fn reap(bulk_out: &mut dyn OutEndpoint, timeout: Duration) {
    while bulk_out.pending() > 0 {
        match bulk_out.wait(timeout) {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                debug!("bulk OUT transfer failed: {e}");
                break;
            }
            None => break,
        }
    }
}

struct RxContext {
    host_mac: MacAddr,
    arp: Arc<Mutex<Arp>>,
    tx: FrameSender,
    sink: Arc<dyn IpSink>,
    events: Sender<LinkEvent>,
}

fn rx_loop(
    mut bulk_in: Box<dyn InEndpoint>,
    transfer_size: u32,
    ctx: RxContext,
    shutdown: Arc<AtomicBool>,
) -> impl FnOnce() {
    move || {
        for _ in 0..RX_DEPTH {
            bulk_in.submit(transfer_size as usize);
        }

        // The host MAC is stable, so seed DHCP's transaction id from it.
        let mut dhcp = DhcpClient::new(
            ctx.host_mac,
            u32::from_be_bytes([
                ctx.host_mac.0[2],
                ctx.host_mac.0[3],
                ctx.host_mac.0[4],
                ctx.host_mac.0[5],
            ]),
        );
        let mut dhcp_state = DhcpTimer::new();
        send_dhcp(&ctx, &dhcp.discover());

        while !shutdown.load(Ordering::Relaxed) {
            match bulk_in.wait(TICK) {
                Some(Ok(data)) => {
                    bulk_in.submit(transfer_size as usize);
                    handle_transfer(&ctx, &mut dhcp, &mut dhcp_state, &data);
                }
                Some(Err(e)) => {
                    bulk_in.submit(transfer_size as usize);
                    if e.is_fatal() {
                        let _ = ctx.events.send(LinkEvent::Failed(e.to_string()));
                        return;
                    }
                    debug!("bulk IN transfer failed: {e}");
                }
                None => {}
            }

            if let Some(msg) = dhcp_state.due(&mut dhcp) {
                send_dhcp(&ctx, &msg);
            }
        }
    }
}

/// Retransmit backoff for whatever DHCP is currently waiting on.
struct DhcpTimer {
    next: Instant,
    backoff: Duration,
    renew_at: Option<Instant>,
}

impl DhcpTimer {
    fn new() -> Self {
        Self {
            next: Instant::now() + DHCP_RETRY_MIN,
            backoff: DHCP_RETRY_MIN,
            renew_at: None,
        }
    }

    fn bound(&mut self, lease: &Lease) {
        self.backoff = DHCP_RETRY_MIN;
        self.renew_at = Some(Instant::now() + lease.renewal_time);
        self.next = self.renew_at.unwrap();
    }

    fn restart(&mut self) {
        self.backoff = DHCP_RETRY_MIN;
        self.next = Instant::now();
        self.renew_at = None;
    }

    /// The next message to send, if a timer has expired.
    fn due(&mut self, dhcp: &mut DhcpClient) -> Option<Vec<u8>> {
        if Instant::now() < self.next {
            return None;
        }

        if self.renew_at.is_some() {
            self.renew_at = None;
            self.next = Instant::now() + DHCP_RETRY_MIN;
            self.backoff = DHCP_RETRY_MIN;
            return dhcp.renew();
        }

        self.backoff = (self.backoff * 2).min(DHCP_RETRY_MAX);
        self.next = Instant::now() + self.backoff;
        // Nothing pending means the previous attempt was abandoned; start over.
        Some(dhcp.retransmit().unwrap_or_else(|| dhcp.discover()))
    }
}

/// Wrap a DHCP message in UDP/IP/Ethernet and queue it.
fn send_dhcp(ctx: &RxContext, msg: &[u8]) {
    let datagram = UdpDatagram {
        src_ip: Ipv4Addr::UNSPECIFIED,
        dst_ip: Ipv4Addr::BROADCAST,
        src_port: dhcp::CLIENT_PORT,
        dst_port: dhcp::SERVER_PORT,
        payload: msg,
    };
    let ip = ipv4::build_udp(&datagram, 0);
    ctx.tx.send(ethernet::build(
        MacAddr::BROADCAST,
        ctx.host_mac,
        ETHERTYPE_IPV4,
        &ip,
    ));
}

fn handle_transfer(ctx: &RxContext, dhcp: &mut DhcpClient, timer: &mut DhcpTimer, data: &[u8]) {
    for result in packet::decode(data) {
        let frame_bytes = match result {
            Ok(f) => f,
            Err(e) => {
                warn!("malformed RNDIS packet: {e}");
                return;
            }
        };
        let frame = match ethernet::parse(frame_bytes) {
            Ok(f) => f,
            Err(e) => {
                trace!("malformed Ethernet frame: {e}");
                continue;
            }
        };
        // Our own MAC is synthesized, so anything not addressed to us or to
        // broadcast is the phone talking to someone else.
        if frame.dst != ctx.host_mac && !frame.dst.is_multicast() {
            continue;
        }

        match frame.ethertype {
            ETHERTYPE_ARP => match ctx.arp.lock().expect("ARP lock").handle(frame.payload) {
                Ok(Some(reply)) => ctx.tx.send(reply),
                Ok(None) => {}
                Err(e) => trace!("bad ARP packet: {e}"),
            },
            ETHERTYPE_IPV4 => handle_ipv4(ctx, dhcp, timer, frame.src, frame.payload),
            other => trace!("ignoring ethertype 0x{other:04x}"),
        }
    }
}

fn handle_ipv4(
    ctx: &RxContext,
    dhcp: &mut DhcpClient,
    timer: &mut DhcpTimer,
    src_mac: MacAddr,
    payload: &[u8],
) {
    // DHCP replies are ours to consume; everything else goes to the tunnel.
    if let Ok(d) = ipv4::parse_udp(payload) {
        if d.dst_port == dhcp::CLIENT_PORT && d.src_port == dhcp::SERVER_PORT {
            match dhcp.handle(d.payload) {
                Ok(DhcpEvent::Send(msg)) => send_dhcp(ctx, &msg),
                Ok(DhcpEvent::Bound(lease)) => {
                    timer.bound(&lease);
                    let mut arp = ctx.arp.lock().expect("ARP lock");
                    arp.set_host_ip(lease.ip);
                    // The reply came straight from the phone, so its source
                    // address is the gateway's MAC.
                    arp.insert(d.src_ip, src_mac);
                    drop(arp);
                    let _ = ctx.events.send(LinkEvent::Bound(lease));
                }
                Ok(DhcpEvent::Nak) => timer.restart(),
                Ok(DhcpEvent::Ignored) => {}
                Err(e) => debug!("bad DHCP message: {e}"),
            }
            return;
        }
    }

    ctx.sink.deliver(payload);
}

/// Non-blocking drain of link events.
pub fn try_next_event(events: &Receiver<LinkEvent>) -> Option<LinkEvent> {
    match events.try_recv() {
        Ok(e) => Some(e),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            error!("link event channel closed");
            None
        }
    }
}
