//! Core-owned identifiers.
//!
//! `reldex-db-driver-api` deliberately does not define a session identifier
//! (ADR-0002, amendment C1): a session is a `db-core` concept, so its id
//! belongs here. So do the two *handles* `db-core` hands to callers in place of
//! driver-owned objects — [`ResultId`] for a cursor and [`LobHandle`] for a
//! large-object locator. Both carry the [`SessionId`] that owns them, so a
//! handle used on the wrong session is rejected as *foreign* rather than
//! silently confused with one that was closed.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use reldex_db_driver_api::ResultSetId;

/// Identifies one [`crate::DatabaseSession`], stable for that session's whole
/// lifetime.
///
/// Process-local and monotonically increasing; not stable across restarts and
/// not meaningful to persist. A reconnect opens a session with a *new* id
/// (`SPEC.md` §18) — it never reuses the id of the session it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(u64);

impl SessionId {
    pub(crate) fn allocate() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// The raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId#{}", self.0)
    }
}

/// An open result set, owned by the session that produced it.
///
/// The driver's `Box<dyn Cursor>` never leaves the session's worker thread
/// (ADR-0002 D1/D2); this is what the caller gets instead. It names the owning
/// session as well as the result, so passing a handle to a *different* session
/// is reported as "belongs to another session" rather than being mistaken for a
/// result that was closed — two very different bugs that used to produce the
/// same message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResultId {
    session: SessionId,
    result: ResultSetId,
}

impl ResultId {
    pub(crate) const fn new(session: SessionId, result: ResultSetId) -> Self {
        Self { session, result }
    }

    pub(crate) const fn result(self) -> ResultSetId {
        self.result
    }

    /// The session that owns this result. Only that session can fetch it.
    #[must_use]
    pub const fn owner(self) -> SessionId {
        self.session
    }
}

impl fmt::Display for ResultId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ResultId#{}.{}", self.session.get(), self.result.get())
    }
}

/// A large object parked on its session's worker thread, waiting to be read.
///
/// A driver's `LobLocator` is a handle derived from the connection, so it must
/// never cross a thread boundary — not even to be *dropped* (ADR-0002 D1/D2).
/// `db-core` therefore takes every locator out of a fetched batch before the
/// batch is sent to the caller and parks it beside the cursors; the caller gets
/// this opaque handle in its place and reads through
/// [`crate::DatabaseSession::read_lob_chunk`].
///
/// A handle dies with the result it came from and with its session: closing
/// either releases it, and using it afterwards is reported, never silently
/// ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LobHandle {
    session: SessionId,
    serial: u64,
}

impl LobHandle {
    pub(crate) fn allocate(session: SessionId) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            session,
            serial: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// The session that owns this large object. Only that session can read it.
    #[must_use]
    pub const fn owner(self) -> SessionId {
        self.session
    }
}

impl fmt::Display for LobHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LobHandle#{}.{}", self.session.get(), self.serial)
    }
}

#[cfg(test)]
mod tests {
    use super::{LobHandle, ResultId, SessionId};
    use reldex_db_driver_api::ResultSetId;

    #[test]
    fn ids_are_unique_and_increasing() {
        let a = SessionId::allocate();
        let b = SessionId::allocate();
        assert_ne!(a, b);
        assert!(b.get() > a.get());
        assert_eq!(a.to_string(), format!("SessionId#{}", a.get()));
    }

    #[test]
    fn handles_name_the_session_that_owns_them() {
        // The property that makes "belongs to another session" distinguishable
        // from "already closed": two sessions can never mint equal handles,
        // even for the same underlying driver id.
        let one = SessionId::allocate();
        let two = SessionId::allocate();
        let result = ResultSetId::from_raw(7);

        assert_eq!(ResultId::new(one, result).owner(), one);
        assert_ne!(ResultId::new(one, result), ResultId::new(two, result));
        assert!(ResultId::new(one, result).to_string().contains("ResultId#"));

        let lob = LobHandle::allocate(one);
        assert_eq!(lob.owner(), one);
        assert_ne!(lob, LobHandle::allocate(one));
        assert!(lob.to_string().contains("LobHandle#"));
    }
}
