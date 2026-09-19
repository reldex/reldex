//! Typed identifiers used across the driver boundary (ADR-0002 D7).
//!
//! Nothing crosses a layer as a bare integer or a bare string: a connection id
//! cannot be passed where a result-set id is expected, and a savepoint name is
//! validated before a driver can interpolate it into SQL.
//!
//! Only the two identifiers the *contract itself* uses live here. A session id
//! belongs to `db-core`, which owns sessions, and a statement id would be a
//! `db-core` concept too — neither appears in any signature below the driver
//! boundary, so neither is defined here (ADR-0002, amendment C1).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Declares a process-local, monotonically increasing `u64` identifier newtype.
macro_rules! sequential_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Allocates the next identifier for this process.
            ///
            /// Identifiers are unique within one process run; they are not
            /// stable across restarts and must not be persisted.
            #[must_use]
            pub fn allocate() -> Self {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                Self(NEXT.fetch_add(1, Ordering::Relaxed))
            }

            /// Rebuilds an identifier from a raw value.
            ///
            /// Intended for tests and for the FFI boundary, where the value has
            /// already been allocated on the Rust side.
            #[must_use]
            pub const fn from_raw(value: u64) -> Self {
                Self(value)
            }

            /// The raw value.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}#{}", stringify!($name), self.0)
            }
        }
    };
}

sequential_id! {
    /// Identifies one physical/logical connection produced by a driver.
    ///
    /// Every object derived from a connection — a [`crate::Cursor`], a
    /// [`crate::LobLocator`] — reports the id of the connection that owns it, so
    /// `db-core` can assert that a handle is only ever used on that connection's
    /// worker thread (ADR-0002 D2).
    ConnectionId
}

sequential_id! {
    /// Identifies one result set (cursor) produced by a statement execution.
    ResultSetId
}

/// Longest savepoint name this contract accepts.
///
/// Oracle Database limits identifiers to 128 bytes from 12.2 onwards; the
/// contract uses the smaller, universally safe bound so a name accepted here
/// cannot be rejected by an older server.
pub const MAX_SAVEPOINT_NAME_LEN: usize = 30;

/// Why a savepoint name was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SavepointNameError {
    /// The name was empty.
    Empty,
    /// The name exceeded [`MAX_SAVEPOINT_NAME_LEN`] bytes.
    TooLong,
    /// The name did not start with an ASCII letter.
    BadFirstCharacter,
    /// The name contained a character other than an ASCII letter, digit, `_`,
    /// `$` or `#`.
    BadCharacter,
}

impl fmt::Display for SavepointNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "savepoint name is empty",
            Self::TooLong => "savepoint name is too long",
            Self::BadFirstCharacter => "savepoint name must start with an ASCII letter",
            Self::BadCharacter => {
                "savepoint name may contain only ASCII letters, digits, '_', '$' and '#'"
            }
        };
        f.write_str(text)
    }
}

impl std::error::Error for SavepointNameError {}

/// A validated savepoint identifier.
///
/// Savepoints are named in SQL text the driver builds itself, so the contract
/// refuses anything that is not a plain unquoted identifier rather than trusting
/// the caller.
///
/// ```
/// use reldex_db_driver_api::SavepointName;
///
/// assert!(SavepointName::new("before_update").is_ok());
/// assert!(SavepointName::new("drop table t --").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SavepointName(Box<str>);

impl SavepointName {
    /// Validates and wraps a savepoint name.
    ///
    /// # Errors
    ///
    /// Returns [`SavepointNameError`] if the name is empty, too long, does not
    /// start with an ASCII letter, or contains anything other than ASCII
    /// letters, digits, `_`, `$` or `#`.
    pub fn new(name: impl Into<String>) -> Result<Self, SavepointNameError> {
        let name = name.into();
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return Err(SavepointNameError::Empty);
        };
        if name.len() > MAX_SAVEPOINT_NAME_LEN {
            return Err(SavepointNameError::TooLong);
        }
        if !first.is_ascii_alphabetic() {
            return Err(SavepointNameError::BadFirstCharacter);
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#')) {
            return Err(SavepointNameError::BadCharacter);
        }
        Ok(Self(name.into_boxed_str()))
    }

    /// The validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SavepointName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocated_ids_are_unique_and_typed() {
        let a = ConnectionId::allocate();
        let b = ConnectionId::allocate();
        assert_ne!(a, b);
        assert!(b.get() > a.get());

        // Distinct sequences: a ResultSetId is not comparable with a
        // ConnectionId, which is the point of the newtypes.
        let r = ResultSetId::allocate();
        assert_eq!(ResultSetId::from_raw(r.get()), r);
    }

    #[test]
    fn ids_display_with_their_type_name() {
        assert_eq!(ResultSetId::from_raw(7).to_string(), "ResultSetId#7");
        assert_eq!(ConnectionId::from_raw(1).to_string(), "ConnectionId#1");
    }

    #[test]
    fn savepoint_names_accept_plain_identifiers() {
        for name in ["a", "SP1", "before_update", "sp$1", "sp#1", "A0_$#"] {
            assert!(
                SavepointName::new(name).is_ok(),
                "{name} should be accepted"
            );
        }
        assert_eq!(
            SavepointName::new("before_update").expect("valid").as_str(),
            "before_update"
        );
    }

    #[test]
    fn savepoint_names_reject_injection_and_junk() {
        assert_eq!(SavepointName::new(""), Err(SavepointNameError::Empty));
        assert_eq!(
            SavepointName::new("1sp"),
            Err(SavepointNameError::BadFirstCharacter)
        );
        assert_eq!(
            SavepointName::new("_sp"),
            Err(SavepointNameError::BadFirstCharacter)
        );
        assert_eq!(
            SavepointName::new("sp; drop table t"),
            Err(SavepointNameError::BadCharacter)
        );
        assert_eq!(
            SavepointName::new("sp name"),
            Err(SavepointNameError::BadCharacter)
        );
        assert_eq!(
            SavepointName::new("ชื่อ"),
            Err(SavepointNameError::BadFirstCharacter)
        );
        assert_eq!(
            SavepointName::new("s".repeat(MAX_SAVEPOINT_NAME_LEN + 1)),
            Err(SavepointNameError::TooLong)
        );
    }
}
