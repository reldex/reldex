//! A user-space TCP relay that adds a fixed delay (and, optionally, a
//! bandwidth limit) to every chunk it forwards, and counts Oracle Net (TNS)
//! packets as they pass. It is how M5.6 puts "a real network" between the
//! client and the local 19c container without admin rights: nothing touches
//! Docker, the container, the host's network stack or any system setting.
//!
//! Standard library only (`AGENTS.md`, "Code quality").
//!
//! # What it models, and what it does not
//!
//! - **Propagation delay.** Every chunk read from one side is written to the
//!   other side `one_way` later. A request/response pair therefore pays
//!   `2 × one_way` of round-trip time on top of loopback. Chunks are pipelined:
//!   a response of many packets is delayed once, not once per packet, which is
//!   what a real long link does.
//! - **Optional serialization delay** (`bits_per_second`): a chunk cannot leave
//!   before the previous one has "finished sending" at that rate. This is a
//!   crude bottleneck link, not a queueing model.
//! - **Not modelled:** loss, reordering, jitter, TCP's congestion window and
//!   slow start over a long path, and receive-window limits. The relay reads
//!   eagerly and buffers without bound, so the sender never waits for the
//!   window a real bandwidth-delay product would impose. Each leg is loopback
//!   TCP, whose own behaviour is unchanged. The numbers are therefore a
//!   **lower bound** on what the same round-trip time costs on a real WAN.
//! - **Timer resolution.** The delay is a `thread::sleep`, which on Windows
//!   uses a high-resolution waitable timer; the benchmark measures the
//!   resulting round-trip time with `ping` on every run rather than trusting
//!   the configured value.
//!
//! # Packet counting
//!
//! Each direction parses TNS framing as it streams by: an 8-byte header whose
//! first two bytes are the packet length until the listener's `ACCEPT`, and
//! whose first four bytes are the length after it (large-SDU framing, which
//! `oracledb` switches to unconditionally once connected). The `ACCEPT`
//! packet also carries the negotiated SDU, which is recorded. If the framing
//! ever stops making sense the counters stop and say so, rather than report
//! nonsense.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How much one read takes off a socket at most.
const CHUNK: usize = 64 * 1024;

/// How often the acceptor checks whether the relay was dropped.
const ACCEPT_POLL: Duration = Duration::from_millis(5);

/// TNS packet type `ACCEPT`.
const PACKET_ACCEPT: u8 = 2;
/// TNS packet type `DATA`.
const PACKET_DATA: u8 = 6;
/// The TNS header is always eight bytes.
const HEADER: usize = 8;
/// Where the negotiated SDU sits in an `ACCEPT` packet (big-endian `u32`):
/// header 8, version 2 (big-endian `u16` right after the header), 12 skipped,
/// flags 1, 9 skipped — the same offsets `oracledb`'s
/// `ConnectMessage::process_accept_packet` reads.
const ACCEPT_SDU_AT: usize = 32;

/// The link one relay imposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkModel {
    /// Added to every chunk in each direction; the round trip gains twice this.
    pub one_way: Duration,
    /// Serialization rate of the bottleneck, or `None` for no limit.
    pub bits_per_second: Option<u64>,
}

/// Counters for one relayed TCP connection, updated as bytes are read.
#[derive(Debug, Default)]
pub struct ConnectionStats {
    to_client_bytes: AtomicU64,
    to_server_bytes: AtomicU64,
    to_client_packets: AtomicU64,
    to_client_data_packets: AtomicU64,
    to_server_packets: AtomicU64,
    largest_to_client_packet: AtomicU64,
    accepted_sdu: AtomicU64,
    accepted_version: AtomicU64,
    framing_lost: AtomicBool,
    /// Set once the `ACCEPT` has been seen: both directions then frame with a
    /// four-byte length.
    large_lengths: AtomicBool,
}

/// A copy of [`ConnectionStats`] at one instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Bytes forwarded towards the client.
    pub to_client_bytes: u64,
    /// Bytes forwarded towards the server.
    pub to_server_bytes: u64,
    /// TNS packets towards the client, of any type.
    pub to_client_packets: u64,
    /// TNS `DATA` packets towards the client.
    pub to_client_data_packets: u64,
    /// TNS packets towards the server.
    pub to_server_packets: u64,
    /// The largest packet seen towards the client.
    pub largest_to_client_packet: u64,
    /// The SDU the listener accepted, or 0 before the `ACCEPT`.
    pub accepted_sdu: u64,
    /// The TNS protocol version the listener accepted, or 0 before the
    /// `ACCEPT`. `oracledb` asks the server to mark the end of each response
    /// only from version 319 (Oracle Database 23ai) on (U-19).
    pub accepted_version: u64,
    /// Whether packet framing was lost (the packet counters are then partial).
    pub framing_lost: bool,
}

impl StatsSnapshot {
    /// What happened between `earlier` and `self`. Gauges (largest packet,
    /// SDU, framing) keep `self`'s value.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            to_client_bytes: self.to_client_bytes.saturating_sub(earlier.to_client_bytes),
            to_server_bytes: self.to_server_bytes.saturating_sub(earlier.to_server_bytes),
            to_client_packets: self
                .to_client_packets
                .saturating_sub(earlier.to_client_packets),
            to_client_data_packets: self
                .to_client_data_packets
                .saturating_sub(earlier.to_client_data_packets),
            to_server_packets: self
                .to_server_packets
                .saturating_sub(earlier.to_server_packets),
            ..self
        }
    }
}

impl ConnectionStats {
    /// The counters now.
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            to_client_bytes: self.to_client_bytes.load(Ordering::Acquire),
            to_server_bytes: self.to_server_bytes.load(Ordering::Acquire),
            to_client_packets: self.to_client_packets.load(Ordering::Acquire),
            to_client_data_packets: self.to_client_data_packets.load(Ordering::Acquire),
            to_server_packets: self.to_server_packets.load(Ordering::Acquire),
            largest_to_client_packet: self.largest_to_client_packet.load(Ordering::Acquire),
            accepted_sdu: self.accepted_sdu.load(Ordering::Acquire),
            accepted_version: self.accepted_version.load(Ordering::Acquire),
            framing_lost: self.framing_lost.load(Ordering::Acquire),
        }
    }
}

/// Which way a pump copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    ToServer,
    ToClient,
}

/// A running relay. Dropping it closes every socket it opened.
pub struct Relay {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<Arc<ConnectionStats>>>>,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
}

impl Relay {
    /// Starts a relay in front of `upstream` and returns once it listens on an
    /// ephemeral loopback port.
    ///
    /// # Errors
    ///
    /// The I/O error when the port cannot be bound or `upstream` does not
    /// resolve.
    pub fn start(upstream: impl ToSocketAddrs, model: LinkModel) -> std::io::Result<Self> {
        let upstream: Vec<SocketAddr> = upstream.to_socket_addrs()?.collect();
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let sockets = Arc::new(Mutex::new(Vec::new()));
        {
            let stop = Arc::clone(&stop);
            let connections = Arc::clone(&connections);
            let sockets = Arc::clone(&sockets);
            thread::spawn(move || {
                accept_loop(&listener, &upstream, model, &stop, &connections, &sockets);
            });
        }
        Ok(Self {
            addr,
            stop,
            connections,
            sockets,
        })
    }

    /// Where a client connects.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// How many connections the relay has accepted.
    pub fn accepted(&self) -> usize {
        self.connections.lock().map_or(0, |list| list.len())
    }

    /// The counters of the most recently accepted connection.
    pub fn latest(&self) -> Option<Arc<ConnectionStats>> {
        self.connections
            .lock()
            .ok()
            .and_then(|list| list.last().cloned())
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(list) = self.sockets.lock() {
            for socket in list.iter() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
    }
}

fn accept_loop(
    listener: &TcpListener,
    upstream: &[SocketAddr],
    model: LinkModel,
    stop: &AtomicBool,
    connections: &Mutex<Vec<Arc<ConnectionStats>>>,
    sockets: &Mutex<Vec<TcpStream>>,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((client, _)) => {
                let Ok(server) = connect_upstream(upstream) else {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                };
                let stats = Arc::new(ConnectionStats::default());
                if let Ok(mut list) = connections.lock() {
                    list.push(Arc::clone(&stats));
                }
                bridge(client, server, model, &stats, sockets);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => break,
        }
    }
}

fn connect_upstream(upstream: &[SocketAddr]) -> std::io::Result<TcpStream> {
    let mut last = None;
    for address in upstream {
        match TcpStream::connect_timeout(address, Duration::from_secs(10)) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no upstream address")))
}

fn bridge(
    client: TcpStream,
    server: TcpStream,
    model: LinkModel,
    stats: &Arc<ConnectionStats>,
    sockets: &Mutex<Vec<TcpStream>>,
) {
    // The accepted socket inherits the listener's non-blocking mode on
    // Windows; the pumps want blocking reads.
    let _ = client.set_nonblocking(false);
    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);
    if let Ok(mut list) = sockets.lock() {
        if let Ok(clone) = client.try_clone() {
            list.push(clone);
        }
        if let Ok(clone) = server.try_clone() {
            list.push(clone);
        }
    }
    let (Ok(client_read), Ok(server_read)) = (client.try_clone(), server.try_clone()) else {
        let _ = client.shutdown(Shutdown::Both);
        let _ = server.shutdown(Shutdown::Both);
        return;
    };
    pipe(client_read, server, model, Direction::ToServer, stats);
    pipe(server_read, client, model, Direction::ToClient, stats);
}

/// One chunk waiting for its release time.
struct Chunk {
    due: Instant,
    bytes: Vec<u8>,
}

/// Starts the reader and the delayed writer for one direction.
fn pipe(
    from: TcpStream,
    to: TcpStream,
    model: LinkModel,
    direction: Direction,
    stats: &Arc<ConnectionStats>,
) {
    let (sender, receiver) = mpsc::channel::<Chunk>();
    let reading = Arc::clone(stats);
    thread::spawn(move || read_side(from, &sender, model, direction, &reading));
    thread::spawn(move || write_side(to, &receiver));
}

fn read_side(
    mut from: TcpStream,
    sender: &mpsc::Sender<Chunk>,
    model: LinkModel,
    direction: Direction,
    stats: &ConnectionStats,
) {
    let mut buffer = vec![0_u8; CHUNK];
    let mut framing = Framing::default();
    let mut link_free_at = Instant::now();
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let arrived = Instant::now();
        let bytes = &buffer[..read];
        framing.feed(bytes, direction, stats);
        let counter = match direction {
            Direction::ToServer => &stats.to_server_bytes,
            Direction::ToClient => &stats.to_client_bytes,
        };
        counter.fetch_add(read as u64, Ordering::AcqRel);
        let leaves = match model.bits_per_second {
            None => arrived,
            Some(rate) => {
                let start = arrived.max(link_free_at);
                let nanos = (read as u128 * 8 * 1_000_000_000) / u128::from(rate.max(1));
                link_free_at =
                    start + Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
                link_free_at
            }
        };
        let chunk = Chunk {
            due: leaves + model.one_way,
            bytes: bytes.to_vec(),
        };
        if sender.send(chunk).is_err() {
            break;
        }
    }
    let _ = from.shutdown(Shutdown::Read);
}

fn write_side(mut to: TcpStream, receiver: &mpsc::Receiver<Chunk>) {
    for chunk in receiver {
        let now = Instant::now();
        if chunk.due > now {
            thread::sleep(chunk.due - now);
        }
        if to.write_all(&chunk.bytes).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

/// Incremental TNS framing for one direction.
#[derive(Default)]
struct Framing {
    header: Vec<u8>,
    /// Bytes of the current packet's body still to pass.
    remaining: usize,
    /// The first bytes of an `ACCEPT` packet, kept until the SDU is read.
    accept: Option<Vec<u8>>,
    lost: bool,
}

impl Framing {
    fn feed(&mut self, mut bytes: &[u8], direction: Direction, stats: &ConnectionStats) {
        while !self.lost && !bytes.is_empty() {
            if self.remaining > 0 {
                let take = self.remaining.min(bytes.len());
                if let Some(accept) = self.accept.as_mut() {
                    let want = (ACCEPT_SDU_AT + 4).saturating_sub(accept.len()).min(take);
                    accept.extend_from_slice(&bytes[..want]);
                    if accept.len() >= ACCEPT_SDU_AT + 4 {
                        let sdu = u32::from_be_bytes([
                            accept[ACCEPT_SDU_AT],
                            accept[ACCEPT_SDU_AT + 1],
                            accept[ACCEPT_SDU_AT + 2],
                            accept[ACCEPT_SDU_AT + 3],
                        ]);
                        stats.accepted_sdu.store(u64::from(sdu), Ordering::Release);
                        let version = u16::from_be_bytes([accept[HEADER], accept[HEADER + 1]]);
                        stats
                            .accepted_version
                            .store(u64::from(version), Ordering::Release);
                        self.accept = None;
                    }
                }
                self.remaining -= take;
                if self.remaining == 0 {
                    // A packet too short to hold the SDU leaves nothing to
                    // carry into the next one.
                    self.accept = None;
                }
                bytes = &bytes[take..];
                continue;
            }
            let want = HEADER - self.header.len();
            let take = want.min(bytes.len());
            self.header.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.header.len() < HEADER {
                continue;
            }
            let large = stats.large_lengths.load(Ordering::Acquire);
            let length = if large {
                u32::from_be_bytes([
                    self.header[0],
                    self.header[1],
                    self.header[2],
                    self.header[3],
                ]) as usize
            } else {
                usize::from(u16::from_be_bytes([self.header[0], self.header[1]]))
            };
            let packet_type = self.header[4];
            if !(HEADER..=64 * 1024 * 1024).contains(&length) {
                self.lost = true;
                stats.framing_lost.store(true, Ordering::Release);
                break;
            }
            match direction {
                Direction::ToServer => {
                    stats.to_server_packets.fetch_add(1, Ordering::AcqRel);
                }
                Direction::ToClient => {
                    stats.to_client_packets.fetch_add(1, Ordering::AcqRel);
                    if packet_type == PACKET_DATA {
                        stats.to_client_data_packets.fetch_add(1, Ordering::AcqRel);
                    }
                    stats
                        .largest_to_client_packet
                        .fetch_max(length as u64, Ordering::AcqRel);
                    if packet_type == PACKET_ACCEPT && !large {
                        stats.large_lengths.store(true, Ordering::Release);
                        self.accept = Some(self.header.clone());
                    }
                }
            }
            self.remaining = length - HEADER;
            self.header.clear();
        }
    }
}
