//! Core-owned identifiers.
//!
//! `reldex-db-driver-api` deliberately does not define a session identifier
//! (ADR-0002, amendment C1): a session is a `db-core` concept, so its id
//! belongs here.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

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

#[cfg(test)]
mod tests {
    use super::SessionId;

    #[test]
    fn ids_are_unique_and_increasing() {
        let a = SessionId::allocate();
        let b = SessionId::allocate();
        assert_ne!(a, b);
        assert!(b.get() > a.get());
        assert_eq!(a.to_string(), format!("SessionId#{}", a.get()));
    }
}
