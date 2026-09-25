//! Shared helpers for `reldex-db-core`'s integration tests. Not a test binary
//! itself (no `tests/support.rs`), so each test file pulls it in with
//! `mod support;`.
//!
//! Each test binary only uses a subset of these, so unused-item warnings here
//! are expected and not a signal of dead production code.
//!
//! # Running one test on both reply paths
//!
//! A request answered through [`reldex_db_core::Completion`] and the same
//! request answered as a [`reldex_db_core::SessionEvent`] are the same worker
//! command with a different reply channel, so every behavioural test should
//! hold on both. [`Session`] is that shim: its methods have the shapes
//! `DatabaseSession`'s do and return an [`Answered`] you `wait()` on, so a
//! test body reads the same either way, and [`both_paths`] turns one test
//! function into the two `#[test]`s that run it.
//!
//! Tests that are *about* `Completion` itself — one that polls, holds a reply
//! while something else runs, or drops it on purpose — use
//! [`open`] directly and stay on that path, because the thing they test has no
//! event-path equivalent.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::sync::mpsc;
use std::thread;

use reldex_db_core::{
    CloseDisposition, CloseError, CompletedOperation, ConnectionParams, DatabaseDriver,
    DatabaseSession, DbError, DbResult, EventCaps, EventQueue, ExecuteOutcome, FetchRequest,
    FetchedBatch, LobHandle, RequestId, ResultId, SavepointName, SegmentReply, ServerOutputSetting,
    SessionEvent, SessionId, SessionLimits, SessionManager, SessionRegistry, Statement,
    event_channel,
};
use reldex_db_driver_api::{Credentials, Endpoint};
use reldex_driver_mock::{MockDriver, Scenario};

/// A fresh, empty scenario with sane test defaults (see [`Scenario::new`]).
#[must_use]
pub(crate) fn scenario() -> Arc<Scenario> {
    Scenario::new()
}

/// The driver a test opens sessions through.
#[must_use]
pub(crate) fn driver(scenario: &Arc<Scenario>) -> Arc<dyn DatabaseDriver> {
    Arc::new(MockDriver::new(Arc::clone(scenario)))
}

/// The connection parameters a test opens sessions with. The mock ignores
/// them; they exist because the contract requires them.
#[must_use]
pub(crate) fn params() -> ConnectionParams {
    ConnectionParams::new(
        Endpoint::ConnectString("mock".to_owned()),
        Credentials::External,
    )
}

/// A registry feeding one fresh queue, which is the shape an application has.
#[must_use]
pub(crate) fn registry() -> (SessionRegistry, EventQueue) {
    registry_with(SessionLimits::new(), EventCaps::new())
}

/// [`registry`] with non-default limits and caps.
#[must_use]
pub(crate) fn registry_with(
    limits: SessionLimits,
    caps: EventCaps,
) -> (SessionRegistry, EventQueue) {
    let (sink, queue) = event_channel(caps);
    (
        SessionRegistry::new(SessionManager::new().with_limits(limits), sink),
        queue,
    )
}

/// Every event `seen` holds for `session`, in delivery order.
pub(crate) fn of_session(seen: &[SessionEvent], session: SessionId) -> Vec<&SessionEvent> {
    seen.iter()
        .filter(|event| event.session() == session)
        .collect()
}

/// The `transaction_possibly_lost` flag from `session`'s `Terminal`, or `None`
/// if it has not arrived.
pub(crate) fn terminal_loss(seen: &[SessionEvent], session: SessionId) -> Option<bool> {
    seen.iter().find_map(|event| match event {
        SessionEvent::Terminal {
            session: named,
            transaction_possibly_lost,
            ..
        } if *named == session => Some(*transaction_possibly_lost),
        _ => None,
    })
}

/// Drains until `session`'s `Terminal` has arrived, keeping everything else.
pub(crate) fn drain_until_terminal_of(queue: &EventQueue, session: SessionId) -> Vec<SessionEvent> {
    drain_until(queue, |seen| {
        seen.iter()
            .any(|event| event.is_terminal() && event.session() == session)
    })
}

/// Opens a session against `scenario` through the real `db-core` session
/// layer (spawns a worker thread and connects on it).
#[must_use]
pub(crate) fn open(scenario: &Arc<Scenario>) -> DatabaseSession {
    open_with(scenario, SessionManager::new())
}

/// Opens a session with non-default [`SessionLimits`].
#[must_use]
pub(crate) fn open_with_limits(scenario: &Arc<Scenario>, limits: SessionLimits) -> DatabaseSession {
    open_with(scenario, SessionManager::new().with_limits(limits))
}

fn open_with(scenario: &Arc<Scenario>, manager: SessionManager) -> DatabaseSession {
    manager
        .open_session(driver(scenario), params())
        .expect("session should open against the mock driver")
}

/// Runs `f` on its own thread and fails the test if it does not finish within
/// `timeout`, rather than letting a hang block the whole suite forever.
pub(crate) fn with_timeout_guard(timeout: Duration, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(timeout) {
        Ok(()) => {
            handle.join().expect("guarded thread should not panic");
        }
        Err(_) => panic!("operation did not complete within {timeout:?}; it hung"),
    }
}

/// Shorthand for a non-zero row/byte count in test call sites.
#[must_use]
pub(crate) fn n(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("test-provided count must be non-zero")
}

/// A short bound for waits that must complete quickly in a correct
/// implementation, without being so tight that normal scheduling jitter
/// causes a flaky failure.
#[must_use]
pub(crate) fn short_timeout() -> Duration {
    Duration::from_secs(5)
}

/// How long a test waits for an event before declaring the run hung.
///
/// Deliberately generous: this is a hang guard, never a timing assertion. A
/// correct implementation reaches every one of these in microseconds.
pub(crate) const HANG_GUARD: Duration = Duration::from_secs(60);

/// Opens a session whose events go to a fresh queue, for tests that drive the
/// event path directly rather than through [`Session`].
#[must_use]
pub(crate) fn open_events(scenario: &Arc<Scenario>) -> (DatabaseSession, EventQueue) {
    open_events_with(scenario, SessionLimits::new(), EventCaps::new())
}

/// [`open_events`] with non-default limits and caps.
#[must_use]
pub(crate) fn open_events_with(
    scenario: &Arc<Scenario>,
    limits: SessionLimits,
    caps: EventCaps,
) -> (DatabaseSession, EventQueue) {
    let session = open_with_limits(scenario, limits);
    let (sink, queue) = event_channel(caps);
    session
        .bind_events(sink)
        .expect("a fresh session binds once");
    (session, queue)
}

/// Opens `count` sessions that all feed **one** queue, which is the shape an
/// application has: one consumer, many workers.
#[must_use]
pub(crate) fn open_events_fan_in(
    scenario: &Arc<Scenario>,
    count: usize,
) -> (Vec<DatabaseSession>, EventQueue) {
    let (sink, queue) = event_channel(EventCaps::new());
    let sessions = (0..count)
        .map(|_| {
            let session = open(scenario);
            session
                .bind_events(sink.clone())
                .expect("a fresh session binds once");
            session
        })
        .collect();
    (sessions, queue)
}

/// Drains until `enough` says the events collected so far are what the test
/// was waiting for, under [`HANG_GUARD`]. Never asserts on timing.
pub(crate) fn drain_until(
    queue: &EventQueue,
    enough: impl Fn(&[SessionEvent]) -> bool,
) -> Vec<SessionEvent> {
    let deadline = Instant::now() + HANG_GUARD;
    let mut seen: Vec<SessionEvent> = Vec::new();
    while !enough(&seen) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the events the test was waiting for never arrived; got {seen:#?}"
        );
        if let Some(event) = queue.wait_timeout(remaining) {
            seen.push(event);
        }
    }
    seen
}

/// Drains exactly `count` events.
pub(crate) fn drain_n(queue: &EventQueue, count: usize) -> Vec<SessionEvent> {
    drain_until(queue, |seen| seen.len() >= count)
}

/// Waits until `ready` says so, under [`HANG_GUARD`].
///
/// For a side effect that is produced by *another* thread and is therefore
/// not observable the instant the effect that triggered it is: a waker call
/// happens after the producer has let go of the lock the consumer was woken
/// from, so asserting on its count straight after a drain returns is a race.
/// This is never a timing assertion — a correct implementation gets here in
/// microseconds and the guard only stops a hang from blocking the suite.
pub(crate) fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within the hang guard"
        );
        thread::yield_now();
    }
}

/// Drains until this session's `Terminal` has been delivered.
pub(crate) fn drain_to_terminal(queue: &EventQueue) -> Vec<SessionEvent> {
    drain_until(queue, |seen| seen.iter().any(SessionEvent::is_terminal))
}

/// The `(request, kind)` pairs of the reply events in `seen`, in order.
pub(crate) fn reply_requests(seen: &[SessionEvent]) -> Vec<u64> {
    seen.iter()
        .filter(|event| event.is_reply())
        .filter_map(|event| event.request().map(|request| request.0))
        .collect()
}

/// Which reply channel a [`Session`] submits through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyPath {
    /// One [`reldex_db_core::Completion`] per request.
    Completion,
    /// One [`reldex_db_core::SessionEvent`] per request, through an
    /// [`EventQueue`].
    Events,
}

/// An answer that has already arrived.
///
/// `wait()` exists so a test body reads identically on both paths; on the
/// event path the waiting happened in the call that produced this.
#[derive(Debug)]
#[must_use]
pub(crate) struct Answered<T>(DbResult<T>);

impl<T> Answered<T> {
    /// The request's result.
    pub(crate) fn wait(self) -> DbResult<T> {
        self.0
    }
}

/// A [`DatabaseSession`] driven through one reply path or the other.
///
/// Derefs to the session, so everything that is not a request — `id()`,
/// `cancel()`, `session_state()`, `has_possibly_active_transaction()`,
/// `connect_warnings()` — is the real thing.
pub(crate) struct Session {
    inner: DatabaseSession,
    path: ReplyPath,
    queue: Option<EventQueue>,
    next_request: Cell<u64>,
    /// Events taken off the queue while looking for a particular reply.
    stashed: RefCell<Vec<SessionEvent>>,
    /// How many replies each request has produced. The shim checks ordering
    /// rule 2 for every test that uses it, so a duplicate reply fails the
    /// suite instead of being quietly stashed and ignored.
    replies_seen: RefCell<HashMap<u64, usize>>,
    terminal_seen: Cell<bool>,
}

impl Deref for Session {
    type Target = DatabaseSession;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Drop for Session {
    /// Nothing may be left over. Every reply this shim took off the queue was
    /// taken because a test asked for it, so a reply still sitting in `stashed`
    /// is one nobody asked for — a second answer to some request, or an answer
    /// to a request that was never submitted. Either breaks ordering rule 2,
    /// and without this the 40-odd shared tests would never notice.
    fn drop(&mut self) {
        if std::thread::panicking() {
            // The test is already failing; a second panic here would abort.
            return;
        }
        let stashed = self.stashed.borrow();
        let leftover: Vec<&SessionEvent> = stashed
            .iter()
            .filter(|event| event.is_reply())
            .collect::<Vec<_>>();
        assert!(
            leftover.is_empty(),
            "replies nobody asked for were left on the queue: {leftover:#?}"
        );
    }
}

/// Opens a session driven through `path`.
#[must_use]
pub(crate) fn open_on(scenario: &Arc<Scenario>, path: ReplyPath) -> Session {
    Session::new(open(scenario), path)
}

/// Opens a session driven through `path`, with non-default limits.
#[must_use]
pub(crate) fn open_on_with_limits(
    scenario: &Arc<Scenario>,
    limits: SessionLimits,
    path: ReplyPath,
) -> Session {
    Session::new(open_with_limits(scenario, limits), path)
}

impl Session {
    fn new(inner: DatabaseSession, path: ReplyPath) -> Self {
        let queue = match path {
            ReplyPath::Completion => None,
            ReplyPath::Events => {
                let (sink, queue) = event_channel(EventCaps::new());
                inner.bind_events(sink).expect("a fresh session binds once");
                Some(queue)
            }
        };
        Self {
            inner,
            path,
            queue,
            next_request: Cell::new(1),
            stashed: RefCell::new(Vec::new()),
            replies_seen: RefCell::new(HashMap::new()),
            terminal_seen: Cell::new(false),
        }
    }

    /// Which path this session is being driven through.
    pub(crate) const fn path(&self) -> ReplyPath {
        self.path
    }

    /// The queue this session's events go to, on the event path.
    pub(crate) fn queue(&self) -> &EventQueue {
        self.queue
            .as_ref()
            .expect("this session is on the completion path")
    }

    /// Events seen while waiting for replies, in delivery order.
    pub(crate) fn stashed(&self) -> std::cell::Ref<'_, Vec<SessionEvent>> {
        self.stashed.borrow()
    }

    fn next_request(&self) -> RequestId {
        let id = self.next_request.get();
        self.next_request.set(id + 1);
        RequestId(id)
    }

    /// Records one event taken off the queue, checking ordering rule 2 as it
    /// goes: a request that is answered twice fails here rather than leaving a
    /// second reply sitting unnoticed in `stashed`.
    fn stash(&self, event: SessionEvent) {
        if event.is_terminal() {
            self.terminal_seen.set(true);
        }
        if event.is_reply()
            && let Some(request) = event.request()
        {
            let mut seen = self.replies_seen.borrow_mut();
            let times = seen
                .entry(request.0)
                .and_modify(|times| *times += 1)
                .or_insert(1);
            assert_eq!(
                *times, 1,
                "{request} was answered more than once; ordering rule 2 says exactly one reply \
                 per accepted request"
            );
        }
        self.stashed.borrow_mut().push(event);
    }

    fn take_stashed_reply(&self, request: RequestId) -> Option<SessionEvent> {
        let mut stashed = self.stashed.borrow_mut();
        let found: Vec<usize> = stashed
            .iter()
            .enumerate()
            .filter(|(_, event)| event.is_reply() && event.request() == Some(request))
            .map(|(index, _)| index)
            .collect();
        assert!(
            found.len() <= 1,
            "{request} has {} replies waiting; exactly one was promised",
            found.len()
        );
        Some(stashed.remove(*found.first()?))
    }

    /// Drains until `request`'s single reply arrives, keeping everything else
    /// for the test to inspect.
    fn await_reply(&self, request: RequestId) -> SessionEvent {
        let queue = self.queue();
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            if let Some(event) = self.take_stashed_reply(request) {
                return event;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "no reply for {request} arrived within the hang guard"
            );
            if let Some(event) = queue.wait_timeout(remaining) {
                self.stash(event);
            }
        }
    }

    /// Drains until this session's `Terminal` has been seen.
    pub(crate) fn await_terminal(&self) {
        let queue = self.queue();
        let deadline = Instant::now() + HANG_GUARD;
        while !self.terminal_seen.get() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "no Terminal arrived within the hang guard"
            );
            if let Some(event) = queue.wait_timeout(remaining) {
                self.stash(event);
            }
        }
    }

    fn submitted<T>(
        &self,
        submit: impl FnOnce(RequestId) -> DbResult<()>,
        unwrap: impl FnOnce(SessionEvent) -> DbResult<T>,
    ) -> Answered<T> {
        let request = self.next_request();
        if let Err(error) = submit(request) {
            return Answered(Err(error));
        }
        Answered(unwrap(self.await_reply(request)))
    }

    fn completed(&self, event: SessionEvent, expected: CompletedOperation) -> DbResult<()> {
        match event {
            SessionEvent::Completed {
                operation, result, ..
            } => {
                assert_eq!(
                    operation, expected,
                    "a reply must name the operation it answers"
                );
                result
            }
            other => panic!("expected Completed({expected:?}), got {other:?}"),
        }
    }

    /// Executes one statement.
    pub(crate) fn execute(&self, statement: Statement) -> Answered<ExecuteOutcome> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.execute(statement).wait()),
            ReplyPath::Events => {
                let mut statement = Some(statement);
                self.submitted(
                    |request| {
                        self.inner
                            .submit_execute(request, statement.take().expect("submitted once"))
                    },
                    |event| match event {
                        SessionEvent::Executed { outcome, .. } => outcome,
                        other => panic!("expected Executed, got {other:?}"),
                    },
                )
            }
        }
    }

    /// Fetches the next batch of a result.
    pub(crate) fn fetch_batch(
        &self,
        result: ResultId,
        max_rows: NonZeroUsize,
    ) -> Answered<FetchedBatch> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.fetch_batch(result, max_rows).wait()),
            ReplyPath::Events => self.submitted(
                |request| self.inner.submit_fetch(request, result, max_rows),
                |event| match event {
                    SessionEvent::Fetched {
                        result: named,
                        batch,
                        ..
                    } => {
                        assert_eq!(named, result, "a fetch reply must name its result");
                        batch
                    }
                    other => panic!("expected Fetched, got {other:?}"),
                },
            ),
        }
    }

    /// Fetches one segment for a result store (ADR-0004).
    pub(crate) fn fetch_segment(&self, fetch: FetchRequest) -> Answered<SegmentReply> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.fetch_segment(fetch).wait()),
            ReplyPath::Events => self.submitted(
                |request| self.inner.submit_fetch_segment(request, fetch),
                |event| match event {
                    SessionEvent::FetchedSegment {
                        fetch: named,
                        segment,
                        ..
                    } => {
                        assert_eq!(named, fetch.ticket(), "a segment reply must name its fetch");
                        segment
                    }
                    other => panic!("expected FetchedSegment, got {other:?}"),
                },
            ),
        }
    }

    /// Releases a result.
    pub(crate) fn close_result(&self, result: ResultId) -> Answered<()> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.close_result(result).wait()),
            ReplyPath::Events => self.submitted(
                |request| self.inner.submit_close_result(request, result),
                |event| self.completed(event, CompletedOperation::CloseResult(result)),
            ),
        }
    }

    /// Commits.
    pub(crate) fn commit(&self) -> Answered<()> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.commit().wait()),
            ReplyPath::Events => self.submitted(
                |request| self.inner.submit_commit(request),
                |event| self.completed(event, CompletedOperation::Commit),
            ),
        }
    }

    /// Rolls back.
    pub(crate) fn rollback(&self) -> Answered<()> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.rollback().wait()),
            ReplyPath::Events => self.submitted(
                |request| self.inner.submit_rollback(request),
                |event| self.completed(event, CompletedOperation::Rollback),
            ),
        }
    }

    /// Establishes a savepoint.
    pub(crate) fn savepoint(&self, name: SavepointName) -> Answered<()> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.savepoint(name).wait()),
            ReplyPath::Events => {
                let mut name = Some(name);
                self.submitted(
                    |request| {
                        self.inner
                            .submit_savepoint(request, name.take().expect("submitted once"))
                    },
                    |event| self.completed(event, CompletedOperation::Savepoint),
                )
            }
        }
    }

    /// Rolls back to a savepoint.
    pub(crate) fn rollback_to_savepoint(&self, name: SavepointName) -> Answered<()> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.rollback_to_savepoint(name).wait()),
            ReplyPath::Events => {
                let mut name = Some(name);
                self.submitted(
                    |request| {
                        self.inner.submit_rollback_to_savepoint(
                            request,
                            name.take().expect("submitted once"),
                        )
                    },
                    |event| self.completed(event, CompletedOperation::RollbackToSavepoint),
                )
            }
        }
    }

    /// Validates the session.
    pub(crate) fn ping(&self) -> Answered<()> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.ping().wait()),
            ReplyPath::Events => self.submitted(
                |request| self.inner.submit_ping(request),
                |event| self.completed(event, CompletedOperation::Ping),
            ),
        }
    }

    /// Reads the next chunk of a large object.
    pub(crate) fn read_lob_chunk(
        &self,
        lob: LobHandle,
        max_bytes: NonZeroUsize,
    ) -> Answered<Vec<u8>> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.read_lob_chunk(lob, max_bytes).wait()),
            ReplyPath::Events => self.submitted(
                |request| self.inner.submit_read_lob_chunk(request, lob, max_bytes),
                |event| match event {
                    SessionEvent::LobChunk {
                        lob: named, bytes, ..
                    } => {
                        assert_eq!(named, lob, "a chunk reply must name its large object");
                        bytes
                    }
                    other => panic!("expected LobChunk, got {other:?}"),
                },
            ),
        }
    }

    /// Releases a large object.
    pub(crate) fn close_lob(&self, lob: LobHandle) -> Answered<()> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.close_lob(lob).wait()),
            ReplyPath::Events => self.submitted(
                |request| self.inner.submit_close_lob(request, lob),
                |event| self.completed(event, CompletedOperation::CloseLob(lob)),
            ),
        }
    }

    /// Turns server output on or off.
    pub(crate) fn set_server_output(
        &self,
        setting: ServerOutputSetting,
    ) -> Answered<ServerOutputSetting> {
        match self.path {
            ReplyPath::Completion => Answered(self.inner.set_server_output(setting).wait()),
            ReplyPath::Events => self.submitted(
                |request| self.inner.submit_set_server_output(request, setting),
                |event| match event {
                    SessionEvent::ServerOutputConfigured { result, .. } => result,
                    other => panic!("expected ServerOutputConfigured, got {other:?}"),
                },
            ),
        }
    }

    /// Everything the session delivered as server output since the last call,
    /// read the way each path delivers it: `ServerOutput` events taken off the
    /// queue while waiting for replies, or the completion-path log.
    ///
    /// On the event path it only sees what has been drained so far, which —
    /// because output precedes its statement's reply — is everything a
    /// statement printed once that statement's `wait()` has returned.
    pub(crate) fn take_output(&self) -> Output {
        match self.path {
            ReplyPath::Completion => {
                let log = self.inner.take_server_output();
                Output {
                    lines: log.lines.iter().map(|line| line.to_string()).collect(),
                    dropped: u64::from(log.dropped),
                    failures: log.failures,
                    first_failure: log.failure,
                    events: 0,
                }
            }
            ReplyPath::Events => {
                let mut output = Output::default();
                let mut stashed = self.stashed.borrow_mut();
                let mut kept = Vec::with_capacity(stashed.len());
                for event in stashed.drain(..) {
                    match event {
                        SessionEvent::ServerOutput {
                            lines,
                            dropped,
                            failure,
                            ..
                        } => {
                            output.events += 1;
                            output
                                .lines
                                .extend(lines.iter().map(|line| line.to_string()));
                            output.dropped += u64::from(dropped);
                            if let Some(failure) = failure {
                                output.failures += 1;
                                if output.first_failure.is_none() {
                                    output.first_failure = Some(failure);
                                }
                            }
                        }
                        other => kept.push(other),
                    }
                }
                *stashed = kept;
                output
            }
        }
    }

    /// Closes the session.
    ///
    /// On the event path this also waits for the session's `Terminal` when
    /// the close actually ended it, so that a test which inspects the driver
    /// afterwards has the same synchronisation `DatabaseSession::close`'s join
    /// gives it on the completion path.
    pub(crate) fn close(&self, disposition: Option<CloseDisposition>) -> Result<(), CloseError> {
        match self.path {
            ReplyPath::Completion => self.inner.close(disposition),
            ReplyPath::Events => {
                let request = self.next_request();
                if let Err(error) = self.inner.submit_close(request, disposition) {
                    return Err(CloseError::Failed(error));
                }
                let result = match self.await_reply(request) {
                    SessionEvent::SessionClosed { result, .. } => result,
                    other => panic!("expected SessionClosed, got {other:?}"),
                };
                let ended = result
                    .as_ref()
                    .err()
                    .is_none_or(|error| !error.session_is_still_open());
                if ended {
                    self.await_terminal();
                }
                result
            }
        }
    }
}

/// Server output as a test sees it, whichever path delivered it.
#[derive(Debug, Default)]
pub(crate) struct Output {
    /// Every line, in order.
    pub(crate) lines: Vec<String>,
    /// Lines reported as dropped.
    pub(crate) dropped: u64,
    /// How many reads failed.
    pub(crate) failures: u32,
    /// The first failed read.
    pub(crate) first_failure: Option<DbError>,
    /// How many `ServerOutput` events carried this (event path only).
    pub(crate) events: usize,
}

impl Output {
    /// Whether nothing at all was delivered.
    pub(crate) fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.dropped == 0 && self.failures == 0 && self.events == 0
    }
}

/// Reports the error a submit refused with, without waiting for anything.
///
/// Only the event path can refuse synchronously; on the completion path a
/// request is always accepted, so this is how a test asks for that difference
/// explicitly instead of hiding it behind [`Answered`].
pub(crate) fn refused<T>(answered: Answered<T>) -> DbError {
    answered.wait().err().expect("the submit should be refused")
}

/// Turns one `fn name(path: ReplyPath)` into the two `#[test]`s that run it on
/// both reply paths.
///
/// The generated tests are `name::on_the_completion_path` and
/// `name::on_the_event_path`, so a failure names which path broke.
#[allow(unused_macros)]
macro_rules! both_paths {
    ($($name:ident),+ $(,)?) => {
        $(
            mod $name {
                #[test]
                fn on_the_completion_path() {
                    super::$name(crate::support::ReplyPath::Completion);
                }

                #[test]
                fn on_the_event_path() {
                    super::$name(crate::support::ReplyPath::Events);
                }
            }
        )+
    };
}

#[allow(unused_imports)]
pub(crate) use both_paths;
