//! Bounding a connect in time (contract gap C-5, upstream gap U-15).
//!
//! `ConnectionParams::with_connect_timeout` is part of the vendor-neutral
//! contract and this driver used to ignore it, because `oracledb`
//! 26.0.0-beta.3 cannot bound a connect at all: `client/mod.rs` opens the
//! socket with a bare `TcpStream::connect`, and its own
//! `ConnectOptions::tcp_connect_timeout` is declared, defaulted to `None` and
//! never read. Spike S10 measured what that costs — 22.0 s against an
//! unroutable address, still outstanding after 30 s against a link that
//! completes the handshake and then forwards nothing.
//!
//! The owner's decision (results file §9 item 8, `SPEC.md` §8) is to honour the
//! limit here instead: run upstream's blocking connect on a helper thread and
//! stop waiting for it once the limit passes.
//!
//! # What "giving up" does and does not mean
//!
//! Nothing interrupts the abandoned attempt — nothing can, which is the whole
//! of U-15 — so it is left to finish or fail on its own. What this module
//! guarantees instead is that **a session that arrives late is never adopted**:
//! [`Handoff`] is a single rendezvous under one mutex, so either the caller
//! takes the result or the helper thread gets it back, never both and never
//! neither. A connection handed back is closed on that helper thread, which is
//! the only thread that has ever touched it (ADR-0002 D1/D2).
//!
//! The cost is one thread per abandoned attempt, alive until upstream's connect
//! returns. That is bounded by how often a user retries, and it is the price of
//! a bounded connect until U-15 is fixed upstream.

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use oracledb::{Config, Connection};
use reldex_db_driver_api::{ConnectionParams, DbError, DbResult, ErrorKind, ExtensionValue};

/// How long this driver spends on a connect when the caller set no limit.
///
/// Fifteen seconds (owner decision 2026-09-19, `SPEC.md` §8): long enough for a
/// listener that is merely slow — spike S1 measured a 119 ms median and spike
/// S8 a 155 ms TCPS connect on the Phase 0 container — and far short of the
/// operating system's own 22 s SYN budget, which is what an unbounded connect
/// inherits.
///
/// Override it per connection with
/// [`ConnectionParams::with_connect_timeout`], or remove the bound entirely
/// with [`EXT_CONNECT_TIMEOUT_UNBOUNDED`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Extension key: wait for a connect **indefinitely**, with no limit at all.
///
/// `ConnectionParams::connect_timeout` is an `Option<Duration>`, and its `None`
/// already means "the caller expressed no preference" — which this driver
/// answers with [`DEFAULT_CONNECT_TIMEOUT`]. It therefore cannot also mean "no
/// limit", and the owner's decision requires "no limit" to be expressible
/// (`SPEC.md` §8). Rather than change the vendor-neutral contract for one
/// driver's need, the third state lives here, in the driver's own extension
/// bag, exactly like every other Oracle-specific switch.
///
/// Only `ExtensionValue::Flag(true)` removes the bound. Every other shape —
/// `Flag(false)`, a `Text("true")`, an integer, a typo in the key — leaves the
/// limit in force, because a malformed opt-in must resolve to the safe side;
/// this mirrors [`crate::EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE`] and
/// [`crate::EXT_ALLOW_UNENFORCED_SERVER_CERT_DN`].
///
/// With no limit, a link that completes the TCP handshake and then goes silent
/// makes `connect` wait forever, because nothing in this driver or in
/// `oracledb` will end it (U-15, U-17). A UI that offers this must say so.
pub const EXT_CONNECT_TIMEOUT_UNBOUNDED: &str = "oracle.connect_timeout_unbounded";

/// The limit that applies to one connect attempt; `None` means no limit.
pub(crate) fn limit(params: &ConnectionParams) -> Option<Duration> {
    if matches!(
        params.extensions().get(EXT_CONNECT_TIMEOUT_UNBOUNDED),
        Some(ExtensionValue::Flag(true))
    ) {
        return None;
    }
    Some(params.connect_timeout().unwrap_or(DEFAULT_CONNECT_TIMEOUT))
}

/// The limit passed before the attempt produced anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Expired;

/// A single-use rendezvous between a caller that may give up and a worker that
/// may finish late.
///
/// Exactly one of two things happens, decided under one mutex so there is no
/// window between them:
///
/// - [`Handoff::wait`] returns the value, or
/// - [`Handoff::wait`] marks the attempt abandoned and every later
///   [`Handoff::deliver`] hands the value straight back to the worker.
///
/// That is what makes "a late connection is never adopted" a property of the
/// code rather than a race that is usually won.
pub(crate) struct Handoff<T> {
    state: Mutex<State<T>>,
    delivered: Condvar,
}

struct State<T> {
    value: Option<T>,
    abandoned: bool,
}

impl<T> Handoff<T> {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                value: None,
                abandoned: false,
            }),
            delivered: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<T>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits up to `limit` for the worker's result.
    ///
    /// On expiry the attempt is marked abandoned **before this returns**, so a
    /// result produced a microsecond later is refused rather than handed over.
    /// A zero `limit` is not a special case: it simply expires immediately.
    pub(crate) fn wait(&self, limit: Duration) -> Result<T, Expired> {
        let started = Instant::now();
        let mut state = self.lock();
        loop {
            if let Some(value) = state.value.take() {
                return Ok(value);
            }
            let elapsed = started.elapsed();
            if elapsed >= limit {
                state.abandoned = true;
                return Err(Expired);
            }
            let (next, _) = self
                .delivered
                .wait_timeout(state, limit - elapsed)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }

    /// Offers the worker's result to whoever is waiting.
    ///
    /// Returns the value back in `Err` when nobody is: the caller's limit
    /// passed, so disposing of it is the worker's job, on the worker's own
    /// thread.
    pub(crate) fn deliver(&self, value: T) -> Result<(), T> {
        let mut state = self.lock();
        if state.abandoned {
            return Err(value);
        }
        state.value = Some(value);
        drop(state);
        self.delivered.notify_one();
        Ok(())
    }
}

/// Names one helper thread, so a stack in a debugger or a crash dump says what
/// it is and which attempt it belongs to.
fn next_attempt() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Runs `oracledb::connect` on a helper thread and waits at most `limit`.
///
/// `map_error` converts an upstream failure on that thread, so no `oracledb`
/// type crosses the rendezvous and this function can hand back a plain
/// [`DbError`].
pub(crate) fn connect_within(
    config: Config,
    limit: Duration,
    map_error: impl FnOnce(&oracledb::Error) -> DbError + Send + 'static,
) -> DbResult<Connection> {
    let handoff = Handoff::new();
    let worker = Arc::clone(&handoff);
    thread::Builder::new()
        .name(format!("reldex-oracle-connect-{}", next_attempt()))
        .spawn(move || {
            // `catch_unwind` is the contract this driver owes its caller: a
            // panic in a dependency must arrive as an error, not as an unwind
            // through a thread boundary. It is also known not to be enough for
            // this particular dependency — `oracledb` locks a poisoned mutex
            // while unwinding, so a panic inside a round trip aborts the
            // process (U-4) — which is a reason to keep panicking inputs out,
            // not a reason to drop the guard.
            let attempt = panic::catch_unwind(AssertUnwindSafe(|| oracledb::connect(config)));
            let result = match attempt {
                Ok(Ok(connection)) => Ok(connection),
                Ok(Err(error)) => Err(map_error(&error)),
                // `payload.as_ref()`, not `&payload`: a `&Box<dyn Any + Send>`
                // unsizes to `dyn Any + Send` as the *box*, so the downcast to
                // the panic's own `&str` never matches and every message reads
                // "a non-string payload".
                Err(payload) => Err(panic_error(payload.as_ref())),
            };
            if let Err(Ok(mut connection)) = worker.deliver(result) {
                // The caller gave up. This session is never handed to anybody:
                // close it here, on the thread that opened it.
                let _ = connection.close();
            }
        })
        .map_err(|error| {
            DbError::internal(format!(
                "this driver could not start the helper thread that bounds a connect: {error}"
            ))
        })?;

    match handoff.wait(limit) {
        Ok(result) => result,
        Err(Expired) => Err(expired(limit)),
    }
}

/// The failure a caller sees when the limit passed.
///
/// [`ErrorKind::Connection`], not [`ErrorKind::Timeout`]: `Timeout` in this
/// contract means "a deadline armed on a statement fired"
/// (`Statement::with_deadline`), and there is no statement and no session here.
/// It is the same category [`crate::error::map_connect`] gives every other
/// failure to reach the database, which is what lets a caller treat "could not
/// connect" as one thing.
fn expired(limit: Duration) -> DbError {
    DbError::new(
        ErrorKind::Connection,
        format!(
            "the database could not be reached within this connection's connect limit of \
             {limit:.1?}, so the attempt was abandoned. It is left to finish or fail on its \
             own — the Oracle crate this driver wraps cannot bound a connect or stop one \
             that has started (upstream gap U-15) — and if a session does open late it is \
             closed rather than used. Raise the limit on the connection profile, or set the \
             connection extension \"{EXT_CONNECT_TIMEOUT_UNBOUNDED}\" to wait indefinitely"
        ),
    )
    // Transient by nature: nothing was opened, so there is no transaction and
    // no session state that retrying could damage.
    .with_retryable(true)
}

fn panic_error(payload: &(dyn std::any::Any + Send)) -> DbError {
    let message = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string payload".to_owned());
    DbError::internal(format!(
        "the Oracle crate panicked while opening a connection: {message}"
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    use reldex_db_driver_api::{Credentials, Endpoint, Extensions, Secret};

    use super::*;

    fn params() -> ConnectionParams {
        ConnectionParams::new(
            Endpoint::ConnectString("127.0.0.1:1521/RELDEX".to_owned()),
            Credentials::UserPassword {
                username: "reldex".to_owned(),
                password: Secret::new("unused"),
            },
        )
    }

    fn unbounded(value: ExtensionValue) -> ConnectionParams {
        let mut extensions = Extensions::new();
        extensions.set(EXT_CONNECT_TIMEOUT_UNBOUNDED, value);
        params().with_extensions(extensions)
    }

    #[test]
    fn an_unset_connect_timeout_means_the_drivers_default_not_no_limit() {
        assert_eq!(limit(&params()), Some(DEFAULT_CONNECT_TIMEOUT));
        assert_eq!(DEFAULT_CONNECT_TIMEOUT, Duration::from_secs(15));
    }

    #[test]
    fn a_caller_set_connect_timeout_replaces_the_default() {
        let asked = Duration::from_millis(250);
        assert_eq!(limit(&params().with_connect_timeout(asked)), Some(asked));
    }

    #[test]
    fn only_the_flag_removes_the_bound_and_a_malformed_opt_in_keeps_it() {
        assert_eq!(limit(&unbounded(ExtensionValue::Flag(true))), None);
        // The safe side, exactly as the other opt-ins resolve: a typo must not
        // silently turn a bounded connect into one that can hang forever.
        for value in [
            ExtensionValue::Flag(false),
            ExtensionValue::Text("true".to_owned()),
            ExtensionValue::Integer(1),
        ] {
            assert_eq!(
                limit(&unbounded(value.clone())),
                Some(DEFAULT_CONNECT_TIMEOUT),
                "{value:?}"
            );
        }
    }

    #[test]
    fn no_limit_beats_a_connect_timeout_that_was_also_set() {
        // The flag is the explicit statement; a timeout left on an imported
        // profile is not. Saying so here means the two can never disagree
        // silently.
        let params =
            unbounded(ExtensionValue::Flag(true)).with_connect_timeout(Duration::from_secs(2));
        assert_eq!(limit(&params), None);
    }

    #[test]
    fn a_handoff_that_expired_hands_a_late_value_back_instead_of_adopting_it() {
        // The property the whole design rests on, tested without a database and
        // without a race: once `wait` has given up, `deliver` cannot succeed,
        // so a session that opens late is disposed of by its own thread.
        let handoff: Arc<Handoff<String>> = Handoff::new();
        assert_eq!(handoff.wait(Duration::ZERO), Err(Expired));
        assert_eq!(
            handoff.deliver("a session that arrived too late".to_owned()),
            Err("a session that arrived too late".to_owned())
        );
        // And it stays refused, however many times the worker tries.
        assert!(handoff.deliver("again".to_owned()).is_err());
    }

    #[test]
    fn a_handoff_that_is_delivered_in_time_hands_the_value_to_the_caller() {
        let handoff: Arc<Handoff<String>> = Handoff::new();
        let worker = Arc::clone(&handoff);
        let (ready, started) = mpsc::channel();
        let thread = thread::spawn(move || {
            let _ = ready.send(());
            worker.deliver("in time".to_owned())
        });
        started.recv().expect("the worker thread starts");
        assert_eq!(
            handoff.wait(Duration::from_secs(5)),
            Ok("in time".to_owned())
        );
        assert!(
            thread.join().expect("the worker thread finishes").is_ok(),
            "delivery must succeed while somebody is still waiting"
        );
    }

    /// A listener that completes the TCP handshake and then says nothing: the
    /// shape of a firewall that drops after the SYN, and the case spike S10
    /// found unbounded (U-15). It needs no database.
    fn black_hole() -> (String, mpsc::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let address = listener
            .local_addr()
            .expect("the bound address")
            .to_string();
        let (stop, stopped) = mpsc::channel();
        thread::spawn(move || {
            // Hold every accepted socket open, reading nothing, until the test
            // says it is done. Dropping the sockets would let the client see a
            // close, which is the opposite of a black hole.
            let mut held: Vec<TcpStream> = Vec::new();
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => held.push(stream),
                    Err(_) => break,
                }
                if stopped.try_recv().is_ok() {
                    break;
                }
            }
            drop(held);
        });
        (address, stop)
    }

    #[test]
    fn a_connect_into_a_black_hole_is_bounded_by_the_limit_and_says_so() {
        let (address, _stop) = black_hole();
        let config = Config::default()
            .set_connect_string(&format!("{address}/RELDEX"))
            .expect("a well-formed Easy Connect string")
            .set_credentials("reldex", "unused");

        let asked = Duration::from_millis(400);
        let started = Instant::now();
        let outcome = connect_within(config, asked, |error| {
            DbError::new(ErrorKind::Connection, error.to_string())
        });
        let elapsed = started.elapsed();

        let error = match outcome {
            Ok(_) => panic!("a black-holed listener cannot produce a session"),
            Err(error) => error,
        };
        assert_eq!(
            error.kind(),
            ErrorKind::Connection,
            "a connect that ran out of time is a connection failure, not a fired statement \
             deadline: {error}"
        );
        assert!(error.is_retryable(), "{error}");
        assert!(
            error.message().contains("connect limit"),
            "the message must name the limit: {error}"
        );
        assert!(
            error.message().contains("abandoned"),
            "the message must say the attempt was abandoned: {error}"
        );
        assert!(
            elapsed >= asked,
            "the limit was honoured early, after {elapsed:?}"
        );
        // Generous, because this runs on a loaded developer machine and the
        // point is that the wait is *bounded*, not that it is precise. Without
        // the bound this connect does not return at all (S10: still outstanding
        // after 30 s).
        assert!(
            elapsed < asked + Duration::from_secs(5),
            "the limit did not bound the connect: {elapsed:?} for a {asked:?} limit"
        );
    }

    #[test]
    fn a_zero_limit_gives_up_immediately_rather_than_waiting() {
        let (address, _stop) = black_hole();
        let config = Config::default()
            .set_connect_string(&format!("{address}/RELDEX"))
            .expect("a well-formed Easy Connect string")
            .set_credentials("reldex", "unused");

        let started = Instant::now();
        let outcome = connect_within(config, Duration::ZERO, |error| {
            DbError::new(ErrorKind::Connection, error.to_string())
        });
        assert!(outcome.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a zero limit must not wait"
        );
    }

    #[test]
    fn a_panic_in_the_helper_thread_is_reported_rather_than_unwinding() {
        // `connect_within` cannot be made to panic on demand, so the containment
        // is exercised through the same rendezvous it uses: a worker that
        // panics delivers nothing, and the caller's bounded wait still returns.
        let handoff: Arc<Handoff<DbResult<()>>> = Handoff::new();
        let worker = Arc::clone(&handoff);
        let thread = thread::spawn(move || {
            let attempt = panic::catch_unwind(AssertUnwindSafe(|| -> DbResult<()> {
                panic!("upstream fell over while connecting")
            }));
            let result = match attempt {
                Ok(result) => result,
                Err(payload) => Err(panic_error(payload.as_ref())),
            };
            let _ = worker.deliver(result);
        });
        let reported = handoff
            .wait(Duration::from_secs(5))
            .expect("the panic is delivered as a value, not as an unwind");
        thread.join().expect("the helper thread does not unwind");

        let error = match reported {
            Ok(()) => panic!("expected the contained panic"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::DriverInternal);
        assert!(
            error.message().contains("fell over while connecting"),
            "{error}"
        );
    }

    #[test]
    fn a_black_hole_listener_really_does_accept_and_then_say_nothing() {
        // Guards the fixture itself: a test that "proves" a timeout against a
        // listener that is actually refusing connections proves nothing.
        let (address, _stop) = black_hole();
        let mut stream = TcpStream::connect(&address).expect("the listener accepts");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("a read timeout");
        let mut buffer = [0_u8; 1];
        assert!(
            stream.read(&mut buffer).is_err(),
            "the fixture answered something; it is not a black hole"
        );
    }
}
