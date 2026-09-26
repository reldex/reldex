//! Query history (`SPEC.md` §20, `phase-1.md` M4.10): what ran, when, how it
//! ended, and how long it took — kept per connection profile so a worksheet
//! can browse and re-run a profile's past statements.
//!
//! # The statement text is stored verbatim, on purpose
//!
//! A history entry's [`HistoryEntry::statement`] is exactly the SQL or
//! PL/SQL the user ran, byte for byte. That is the feature: "re-run into the
//! current worksheet" (M4.10) only works if the text is exact. This means a
//! statement that legitimately contains the word `PASSWORD` — an
//! `ALTER USER … IDENTIFIED BY …`, a `CREATE USER` — is captured as written.
//! [`crate::CredentialPattern`], the guard that refuses credential-looking
//! text in a *profile's* endpoint fields, is deliberately **not** run here: a
//! SQL statement is not an endpoint, and scanning arbitrary user SQL for the
//! word "password" would both miss real secrets (a bind variable, which is
//! never captured at all — see below) and reject legitimate DDL. This is
//! recorded as an accepted limitation in
//! `docs/decisions/0006-local-persistence-settings-profiles-sqlite.md`,
//! with a follow-up idea for M4.x: a per-statement "do not record this" flag
//! set before running it.
//!
//! What *is* still true, by construction: **bind values are never captured**.
//! [`HistoryEntry`] has no field for them — only the statement text as
//! submitted, never the parameter values bound to its placeholders — so a
//! `SELECT * FROM users WHERE password = :1` binding a real secret at `:1`
//! never puts that secret in history. The store has nothing to write it to.
//!
//! # Bounded size
//!
//! [`crate::settings::HISTORY_MAX_ENTRIES_PER_PROFILE`] (application level,
//! default 1,000) bounds how many entries a profile's history keeps;
//! `Store::record_history` trims the oldest entries past that bound in the
//! same transaction as the insert. "No limit" is accepted, honestly: the
//! store file then grows with every statement ever run.

use std::fmt;

use crate::ids::ProfileId;
use crate::time::UnixTimeMs;

/// The largest a statement's text may be, in UTF-8 bytes.
///
/// Generous for any real script, and only large enough that a paste accident
/// (an entire dump piped into the worksheet) cannot put an unbounded blob in
/// a history row while the table itself has no per-row cap of its own.
pub const MAX_STATEMENT_BYTES: usize = 1024 * 1024;

/// Identifies one history entry: the SQLite rowid it was assigned.
///
/// Opaque outside the crate beyond equality, ordering and display — a caller
/// only ever gets one back from [`crate::store::Store::record_history`] or
/// [`crate::store::Store::history`] and hands it back as
/// [`HistoryPage::before`] for the next page.
///
/// [`Self::to_ffi_value`]/[`Self::from_ffi_value`] (M2.11) are the one
/// sanctioned way to carry an id across a boundary that cannot hold a Rust
/// value directly — a C caller's pagination cursor — the same numeric-id
/// pattern `crate::settings::SettingId`'s FFI crossing already uses. Nothing
/// about that value is meaningful outside "the id this crate handed out",
/// and it is never persisted by anything but this crate's own `history`
/// table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HistoryId(i64);

impl HistoryId {
    pub(crate) const fn from_row_id(id: i64) -> Self {
        Self(id)
    }

    pub(crate) const fn as_row_id(self) -> i64 {
        self.0
    }

    /// The id's underlying value, for a caller that must carry it across a
    /// boundary this crate's own type cannot cross (M2.11's C ABI).
    #[must_use]
    pub const fn to_ffi_value(self) -> i64 {
        self.0
    }

    /// Rebuilds an id from the value [`Self::to_ffi_value`] returned.
    #[must_use]
    pub const fn from_ffi_value(value: i64) -> Self {
        Self(value)
    }
}

impl fmt::Display for HistoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// How a recorded statement ended. A small, closed set — never free text —
/// so a UI can show an icon and a re-run affordance without parsing a
/// message.
///
/// `#[non_exhaustive]`: a future outcome (for example a driver-level retry)
/// must not be a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HistoryOutcome {
    /// Ran to completion without error.
    Succeeded,
    /// The database or the driver reported an error.
    Failed {
        /// The native error code (an ORA-nnnnn number for Oracle), when the
        /// driver supplied one.
        native_code: Option<i32>,
    },
    /// Cancelled — on-demand cancellation is not available with the current
    /// driver (`SPEC.md` §10); this is here for when it is, and for a script
    /// run stopped between statements.
    Cancelled,
    /// The per-statement time limit fired (`SPEC.md` §10). Distinct from
    /// [`HistoryOutcome::Failed`]: nothing about the statement was wrong.
    TimedOut,
}

/// Why a history entry was refused and not recorded.
///
/// Never the credential-pattern guard: see the module documentation for why
/// that guard does not apply to SQL text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HistoryError {
    /// The statement is empty (or only whitespace).
    StatementEmpty,
    /// The statement is longer than [`MAX_STATEMENT_BYTES`].
    StatementTooLong {
        /// The statement's length in bytes.
        bytes: usize,
        /// The cap.
        max: usize,
    },
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StatementEmpty => f.write_str("the history entry's statement is empty"),
            Self::StatementTooLong { bytes, max } => write!(
                f,
                "the history entry's statement is {bytes} bytes, more than the {max}-byte cap"
            ),
        }
    }
}

impl std::error::Error for HistoryError {}

/// One statement to record: what ran, for which profile, and how it ended.
///
/// Never carries a bind value — only the statement text as submitted. See
/// the module documentation.
///
/// `Debug` prints [`HistoryEntry::statement`]'s length, never its text — the
/// same style as [`crate::ProfileEndpoint`]'s connect string: a statement can
/// legitimately contain `IDENTIFIED BY "…"`, and a log line is not the place
/// for it.
#[derive(Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Which profile's worksheet ran it.
    pub profile: ProfileId,
    /// When it was submitted.
    pub executed_at: UnixTimeMs,
    /// The statement text, verbatim.
    pub statement: String,
    /// How it ended.
    pub outcome: HistoryOutcome,
    /// How long it took, in milliseconds.
    pub elapsed_ms: u64,
    /// Rows affected or returned, when known.
    pub row_count: Option<u64>,
}

impl fmt::Debug for HistoryEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HistoryEntry")
            .field("profile", &self.profile)
            .field("executed_at", &self.executed_at)
            .field(
                "statement",
                &format!("<redacted, {} bytes>", self.statement.len()),
            )
            .field("outcome", &self.outcome)
            .field("elapsed_ms", &self.elapsed_ms)
            .field("row_count", &self.row_count)
            .finish()
    }
}

impl HistoryEntry {
    /// Checks the entry before it is recorded.
    ///
    /// Deliberately shallow, and deliberately does not run
    /// [`crate::CredentialPattern`] — see the module documentation.
    ///
    /// # Errors
    ///
    /// [`HistoryError`] naming the first rule the entry breaks.
    pub fn validate(&self) -> Result<(), HistoryError> {
        if self.statement.trim().is_empty() {
            return Err(HistoryError::StatementEmpty);
        }
        if self.statement.len() > MAX_STATEMENT_BYTES {
            return Err(HistoryError::StatementTooLong {
                bytes: self.statement.len(),
                max: MAX_STATEMENT_BYTES,
            });
        }
        Ok(())
    }
}

/// One stored history entry, as read back.
///
/// `Debug` redacts [`HistoryRecord::statement`] the same way
/// [`HistoryEntry`]'s does.
#[derive(Clone, PartialEq, Eq)]
pub struct HistoryRecord {
    /// Its id.
    pub id: HistoryId,
    /// Which profile ran it.
    pub profile: ProfileId,
    /// When it was submitted.
    pub executed_at: UnixTimeMs,
    /// The statement text, verbatim.
    pub statement: String,
    /// How it ended.
    pub outcome: HistoryOutcome,
    /// How long it took, in milliseconds.
    pub elapsed_ms: u64,
    /// Rows affected or returned, when known.
    pub row_count: Option<u64>,
}

impl fmt::Debug for HistoryRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HistoryRecord")
            .field("id", &self.id)
            .field("profile", &self.profile)
            .field("executed_at", &self.executed_at)
            .field(
                "statement",
                &format!("<redacted, {} bytes>", self.statement.len()),
            )
            .field("outcome", &self.outcome)
            .field("elapsed_ms", &self.elapsed_ms)
            .field("row_count", &self.row_count)
            .finish()
    }
}

/// One page of a profile's history, newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryPage {
    /// At most this many entries.
    pub limit: u32,
    /// Only entries older than this one — the last id of the previous page,
    /// for paging backward through history. `None` starts from the newest.
    pub before: Option<HistoryId>,
}

impl HistoryPage {
    /// The first page: the `limit` newest entries.
    #[must_use]
    pub const fn first(limit: u32) -> Self {
        Self {
            limit,
            before: None,
        }
    }

    /// The next page after `id` (exclusive): the `limit` newest entries
    /// older than `id`.
    #[must_use]
    pub const fn after(limit: u32, id: HistoryId) -> Self {
        Self {
            limit,
            before: Some(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_whitespace_only_statement_is_refused() {
        let entry = HistoryEntry {
            profile: ProfileId::new_random(),
            executed_at: UnixTimeMs::now(),
            statement: "   \n\t".to_owned(),
            outcome: HistoryOutcome::Succeeded,
            elapsed_ms: 0,
            row_count: None,
        };
        assert_eq!(entry.validate(), Err(HistoryError::StatementEmpty));
    }

    #[test]
    fn a_statement_over_the_cap_is_refused() {
        let entry = HistoryEntry {
            profile: ProfileId::new_random(),
            executed_at: UnixTimeMs::now(),
            statement: "x".repeat(MAX_STATEMENT_BYTES + 1),
            outcome: HistoryOutcome::Succeeded,
            elapsed_ms: 0,
            row_count: None,
        };
        assert_eq!(
            entry.validate(),
            Err(HistoryError::StatementTooLong {
                bytes: MAX_STATEMENT_BYTES + 1,
                max: MAX_STATEMENT_BYTES,
            })
        );
    }

    #[test]
    fn a_statement_at_the_cap_and_a_credential_looking_one_are_accepted() {
        let at_cap = HistoryEntry {
            profile: ProfileId::new_random(),
            executed_at: UnixTimeMs::now(),
            statement: "x".repeat(MAX_STATEMENT_BYTES),
            outcome: HistoryOutcome::Succeeded,
            elapsed_ms: 1,
            row_count: Some(0),
        };
        assert_eq!(at_cap.validate(), Ok(()));

        // The credential-pattern guard does not apply to SQL text: this
        // would be refused as a profile endpoint, but a history entry is not
        // an endpoint.
        let ddl = HistoryEntry {
            profile: ProfileId::new_random(),
            executed_at: UnixTimeMs::now(),
            statement: "ALTER USER app_owner IDENTIFIED BY \"Hunter2\"".to_owned(),
            outcome: HistoryOutcome::Succeeded,
            elapsed_ms: 4,
            row_count: None,
        };
        assert_eq!(ddl.validate(), Ok(()));
    }

    #[test]
    fn debug_redacts_the_statement_text_on_both_entry_and_record() {
        let secret_looking = "ALTER USER app_owner IDENTIFIED BY \"Hunter2\"";
        let entry = HistoryEntry {
            profile: ProfileId::new_random(),
            executed_at: UnixTimeMs::now(),
            statement: secret_looking.to_owned(),
            outcome: HistoryOutcome::Succeeded,
            elapsed_ms: 4,
            row_count: None,
        };
        let printed = format!("{entry:?}");
        assert!(!printed.contains("Hunter2"), "{printed}");
        assert!(!printed.contains(secret_looking), "{printed}");
        assert!(
            printed.contains(&format!("<redacted, {} bytes>", secret_looking.len())),
            "{printed}"
        );

        let record = HistoryRecord {
            id: HistoryId::from_row_id(1),
            profile: entry.profile,
            executed_at: entry.executed_at,
            statement: secret_looking.to_owned(),
            outcome: entry.outcome,
            elapsed_ms: entry.elapsed_ms,
            row_count: entry.row_count,
        };
        let printed = format!("{record:?}");
        assert!(!printed.contains("Hunter2"), "{printed}");
        assert!(!printed.contains(secret_looking), "{printed}");
        assert!(
            printed.contains(&format!("<redacted, {} bytes>", secret_looking.len())),
            "{printed}"
        );
    }
}
