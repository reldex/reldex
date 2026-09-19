//! A tiny in-process TCP forwarding proxy, used by spike S10 to simulate
//! network loss without touching Docker, the container or the host's network.
//!
//! The test process listens on `127.0.0.1:<ephemeral>`, forwards to the real
//! listener, and can change what it does with the two sockets while a session
//! is running. That is the whole reason it exists: `docker pause`, `docker
//! network disconnect` and firewall rules all change state outside the test,
//! and the Phase 0 database has to survive the suite untouched.
//!
//! Standard library only — no new dependency enters the graph for a test
//! helper (`AGENTS.md`, "Code quality").
//!
//! # The failure shapes
//!
//! | Mode | What the peer sees | Real-world analogue |
//! |---|---|---|
//! | [`Mode::Forward`] | a working link | — |
//! | [`Mode::HardDrop`] | `FIN` on both sides, immediately | the peer process dies, a NAT box resets the flow, a stack that notices the cable |
//! | [`Mode::BlackHole`] | nothing at all; the sockets stay open | Wi-Fi drops, a laptop suspends, a firewall discards silently |
//!
//! [`Mode::DropOnClientData`] and [`Mode::DropOnServerData`] are the same hard
//! drop, triggered by the next packet in one direction, so a test can kill the
//! link at a chosen point inside a round trip (S10's "commit in flight").
//!
//! # What a black hole does to the *server*
//!
//! Only the client's view is simulated. In [`Mode::BlackHole`] the proxy keeps
//! its own connection to the database open, so the server still believes the
//! session is alive and keeps its row locks — which is exactly the case S10
//! has to measure. In [`Mode::HardDrop`] the server-side socket is closed too,
//! so the server learns at once.

use std::io::{ErrorKind as IoErrorKind, Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How long a pump blocks in `read` before it re-reads the mode.
///
/// The upper bound on how late a mode switch is noticed, and therefore on the
/// error this helper adds to any "time to detect" measurement. A hard drop does
/// not pay it: [`Proxy::set_mode`] shuts the sockets down itself.
const POLL: Duration = Duration::from_millis(20);

/// The size of one forwarding hop.
const BUFFER: usize = 32 * 1024;

/// What the proxy is doing with the traffic right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Copy both directions.
    Forward,
    /// Copy nothing and close nothing: the link is up and silent.
    BlackHole,
    /// Shut both sockets down now.
    HardDrop,
    /// Forward nothing; hard-drop as soon as the client sends anything.
    DropOnClientData,
    /// Forward the client's request, then hard-drop on the server's first
    /// reply, so the server acted and the client never learns the outcome.
    DropOnServerData,
}

impl Mode {
    /// The atomic representation.
    const fn code(self) -> u8 {
        match self {
            Self::Forward => 0,
            Self::BlackHole => 1,
            Self::HardDrop => 2,
            Self::DropOnClientData => 3,
            Self::DropOnServerData => 4,
        }
    }

    /// The inverse of [`Mode::code`].
    const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Forward,
            1 => Self::BlackHole,
            3 => Self::DropOnClientData,
            4 => Self::DropOnServerData,
            _ => Self::HardDrop,
        }
    }
}

/// Which way a pump is copying.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Towards the database.
    ClientToServer,
    /// Towards the caller.
    ServerToClient,
}

/// State every thread of one proxy shares.
struct Shared {
    /// The current [`Mode`], as [`Mode::code`].
    mode: AtomicU8,
    /// Set when the proxy is dropped.
    stop: AtomicBool,
    /// How many TCP connections the proxy accepted.
    accepted: AtomicU64,
    /// Bytes forwarded towards the database.
    to_server: AtomicU64,
    /// Bytes forwarded towards the caller.
    to_client: AtomicU64,
    /// Clones of every socket this proxy has opened, so a hard drop is
    /// immediate rather than waiting for a pump to return from `read`.
    sockets: Mutex<Vec<TcpStream>>,
}

impl Shared {
    /// The current mode.
    fn mode(&self) -> Mode {
        Mode::from_code(self.mode.load(Ordering::Relaxed))
    }

    /// Shuts every socket this proxy has opened.
    ///
    /// The clones are kept rather than dropped, so a second call still reaches
    /// the same sockets; shutting an already-shut socket is a no-op error that
    /// is ignored here on purpose.
    fn slam(&self) {
        if let Ok(guard) = self.sockets.lock() {
            for socket in guard.iter() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
    }

    /// Registers a socket so [`Shared::slam`] can reach it.
    fn remember(&self, socket: &TcpStream) {
        if let Ok(clone) = socket.try_clone()
            && let Ok(mut guard) = self.sockets.lock()
        {
            guard.push(clone);
        }
    }
}

/// A running forwarding proxy. Dropping it closes everything it opened.
pub struct Proxy {
    /// State shared with the acceptor and the pumps.
    shared: Arc<Shared>,
    /// Where a client connects.
    addr: SocketAddr,
}

impl Proxy {
    /// Starts a proxy in front of `upstream` and returns once it is listening.
    ///
    /// # Errors
    ///
    /// The underlying I/O error when the ephemeral port cannot be bound or
    /// `upstream` cannot be resolved.
    pub fn start(upstream: impl ToSocketAddrs) -> std::io::Result<Self> {
        let upstream: Vec<SocketAddr> = upstream.to_socket_addrs()?.collect();
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let shared = Arc::new(Shared {
            mode: AtomicU8::new(Mode::Forward.code()),
            stop: AtomicBool::new(false),
            accepted: AtomicU64::new(0),
            to_server: AtomicU64::new(0),
            to_client: AtomicU64::new(0),
            sockets: Mutex::new(Vec::new()),
        });

        let accepting = Arc::clone(&shared);
        thread::spawn(move || accept_loop(&listener, &upstream, &accepting));

        Ok(Self { shared, addr })
    }

    /// Where a client connects.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// An Easy Connect string pointing at this proxy.
    pub fn connect_string(&self, service: &str) -> String {
        format!("127.0.0.1:{}/{service}", self.addr.port())
    }

    /// Changes what the proxy does with the traffic.
    ///
    /// [`Mode::HardDrop`] takes effect before this returns; the others take
    /// effect within [`POLL`].
    pub fn set_mode(&self, mode: Mode) {
        self.shared.mode.store(mode.code(), Ordering::Relaxed);
        if mode == Mode::HardDrop {
            self.shared.slam();
        }
    }

    /// How many TCP connections have reached the proxy.
    ///
    /// A session that worked while this stayed at zero went somewhere else — a
    /// listener redirect, for instance — and nothing this helper does would
    /// have affected it.
    pub fn accepted(&self) -> u64 {
        self.shared.accepted.load(Ordering::Relaxed)
    }

    /// Bytes forwarded towards the database.
    pub fn bytes_to_server(&self) -> u64 {
        self.shared.to_server.load(Ordering::Relaxed)
    }

    /// Bytes forwarded towards the caller.
    pub fn bytes_to_client(&self) -> u64 {
        self.shared.to_client.load(Ordering::Relaxed)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.slam();
    }
}

/// Accepts clients until the proxy is dropped.
fn accept_loop(listener: &TcpListener, upstream: &[SocketAddr], shared: &Arc<Shared>) {
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((client, _)) => {
                shared.accepted.fetch_add(1, Ordering::Relaxed);
                match connect_upstream(upstream) {
                    Ok(server) => bridge(&client, &server, shared),
                    Err(_) => {
                        let _ = client.shutdown(Shutdown::Both);
                    }
                }
            }
            Err(error) if error.kind() == IoErrorKind::WouldBlock => thread::sleep(POLL),
            Err(_) => break,
        }
    }
}

/// Opens the far side of the bridge.
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

/// Wires one accepted client to one upstream socket.
fn bridge(client: &TcpStream, server: &TcpStream, shared: &Arc<Shared>) {
    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);
    let _ = client.set_read_timeout(Some(POLL));
    let _ = server.set_read_timeout(Some(POLL));
    shared.remember(client);
    shared.remember(server);

    let (Ok(client_read), Ok(client_write), Ok(server_read), Ok(server_write)) = (
        client.try_clone(),
        client.try_clone(),
        server.try_clone(),
        server.try_clone(),
    ) else {
        let _ = client.shutdown(Shutdown::Both);
        let _ = server.shutdown(Shutdown::Both);
        return;
    };

    let up = Arc::clone(shared);
    thread::spawn(move || pump(client_read, server_write, Direction::ClientToServer, &up));
    let down = Arc::clone(shared);
    thread::spawn(move || pump(server_read, client_write, Direction::ServerToClient, &down));
}

/// Copies one direction until the mode, the socket or the proxy says stop.
///
/// On the way out it shuts down both of its own sockets, so the end of a
/// session closes the pair rather than leaving a half-open bridge behind.
fn pump(mut from: TcpStream, mut to: TcpStream, direction: Direction, shared: &Arc<Shared>) {
    let mut buffer = vec![0_u8; BUFFER];
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let mode = shared.mode();
        if mode == Mode::HardDrop {
            break;
        }
        // Reading and discarding is not what a black hole does: it would drain
        // the peer's send buffer. This direction stops reading entirely.
        let silent = matches!(
            (mode, direction),
            (Mode::BlackHole, _) | (Mode::DropOnClientData, Direction::ServerToClient)
        );
        if silent {
            thread::sleep(POLL);
            continue;
        }

        let read = match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    IoErrorKind::WouldBlock | IoErrorKind::TimedOut | IoErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => break,
        };

        // Re-read the mode **after** the read: this thread was blocked inside
        // `read` for up to POLL while the test switched modes, and the bytes
        // that just arrived belong to the new mode, not the old one. Deciding
        // from the stale value is what let a commit's reply through while the
        // test believed it had already armed the drop.
        let mode = shared.mode();
        if mode == Mode::HardDrop {
            break;
        }
        if mode == Mode::BlackHole {
            // Already off the wire; a black hole loses them.
            continue;
        }

        // The trigger modes: the bytes are read and then thrown away together
        // with the link, so the sender believes it sent and the receiver never
        // sees them.
        let trigger = matches!(
            (mode, direction),
            (Mode::DropOnClientData, Direction::ClientToServer)
                | (Mode::DropOnServerData, Direction::ServerToClient)
        );
        if trigger {
            shared.mode.store(Mode::HardDrop.code(), Ordering::Relaxed);
            break;
        }

        if to.write_all(&buffer[..read]).is_err() {
            break;
        }
        match direction {
            Direction::ClientToServer => {
                shared.to_server.fetch_add(read as u64, Ordering::Relaxed);
            }
            Direction::ServerToClient => {
                shared.to_client.fetch_add(read as u64, Ordering::Relaxed);
            }
        }
    }
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
}
