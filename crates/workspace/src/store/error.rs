//! The store's error type. SQLite's own error type never leaves this module:
//! a caller sees what went wrong in Reldex's terms, with SQLite's extended
//! result code kept for diagnostics.

use std::fmt;
use std::path::PathBuf;

use rusqlite::ErrorCode;

use crate::history::HistoryError;
use crate::ids::{ProfileId, WorksheetId};
use crate::profile::ProfileError;
use crate::settings::SettingError;
use crate::worksheet::WorksheetError;

/// Why a store operation failed. Every failure is one of these; none panics.
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    /// The file was written by a newer Reldex. It is refused, never
    /// downgraded: an older build cannot know what the newer schema means,
    /// and "migrating" it backwards would lose data silently.
    NewerSchema {
        /// The file's schema version.
        found: u32,
        /// The newest version this build understands.
        supported: u32,
    },
    /// The file is an SQLite database, but not a Reldex store. Nothing was
    /// written to it.
    NotAReldexStore,
    /// The file carries Reldex's identity but schema version 0, and already
    /// holds tables: a header Reldex never writes, since the identity and the
    /// version are set in the transaction that creates the tables. Refused
    /// before anything is written.
    IncompleteHeader,
    /// SQLite refused one of this build's statements against the file's
    /// tables: a table or column differs from what the file's schema version
    /// promises — a file altered outside Reldex.
    SchemaMismatch {
        /// SQLite's message. It names the construct in this build's SQL
        /// (`no such column: …`), never a row's values.
        detail: String,
    },
    /// The file, its directory or its media is read-only (`SQLITE_READONLY`),
    /// so nothing can be saved. Nothing was written.
    ReadOnly,
    /// The file is not a database, or its contents are damaged.
    Corrupt {
        /// SQLite's description.
        detail: String,
    },
    /// Another connection held the file's lock for longer than the busy
    /// timeout. Nothing was written; the operation can be retried.
    Busy,
    /// The file could not be opened or created: a missing or unwritable
    /// directory, a permission.
    CannotOpen {
        /// The file.
        path: PathBuf,
        /// SQLite's or the operating system's description.
        detail: String,
    },
    /// No platform data directory could be found. Platforms without one
    /// (Android, iOS) pass an explicit path to [`crate::Store::open`].
    NoDataDirectory,
    /// Creating the data directory failed.
    CreateDirectory {
        /// The directory.
        path: PathBuf,
        /// The operating system's error.
        source: std::io::Error,
    },
    /// A profile failed validation and was not written.
    InvalidProfile(ProfileError),
    /// A setting value was refused and not written.
    InvalidSetting(SettingError),
    /// No profile has this id.
    ProfileNotFound(ProfileId),
    /// A profile with this id already exists.
    ProfileExists(ProfileId),
    /// A history entry failed validation and was not written.
    InvalidHistory(HistoryError),
    /// A worksheet failed validation and was not written.
    InvalidWorksheet(WorksheetError),
    /// No worksheet has this id.
    WorksheetNotFound(WorksheetId),
    /// A stored row could not be read back as a model value.
    InvalidRow {
        /// What was wrong with it.
        detail: String,
    },
    /// Any other SQLite failure.
    Sqlite {
        /// SQLite's extended result code, or -1 when the failure did not come
        /// from SQLite itself.
        code: i32,
        /// SQLite's description.
        detail: String,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewerSchema { found, supported } => write!(
                f,
                "this settings file was written by a newer Reldex (schema {found}; this \
                 build understands up to {supported}) and was not opened"
            ),
            Self::NotAReldexStore => f.write_str("this file is not a Reldex settings store"),
            Self::IncompleteHeader => f.write_str(
                "this settings file has Reldex's identity but no schema version, and was not \
                 opened",
            ),
            Self::SchemaMismatch { detail } => write!(
                f,
                "the settings store's tables differ from what its schema version promises: \
                 {detail}"
            ),
            Self::ReadOnly => f.write_str("the settings store is read-only; nothing was saved"),
            Self::Corrupt { detail } => write!(f, "the settings store is damaged: {detail}"),
            Self::Busy => f.write_str("the settings store is locked by another connection"),
            Self::CannotOpen { path, detail } => {
                write!(f, "cannot open {}: {detail}", path.display())
            }
            Self::NoDataDirectory => f.write_str("no platform data directory was found"),
            Self::CreateDirectory { path, source } => {
                write!(f, "cannot create {}: {source}", path.display())
            }
            Self::InvalidProfile(error) => write!(f, "{error}"),
            Self::InvalidSetting(error) => write!(f, "{error}"),
            Self::ProfileNotFound(id) => write!(f, "no profile {id}"),
            Self::ProfileExists(id) => write!(f, "profile {id} already exists"),
            Self::InvalidHistory(error) => write!(f, "{error}"),
            Self::InvalidWorksheet(error) => write!(f, "{error}"),
            Self::WorksheetNotFound(id) => write!(f, "no worksheet {id}"),
            Self::InvalidRow { detail } => write!(f, "a stored row is invalid: {detail}"),
            Self::Sqlite { code, detail } => write!(f, "SQLite error {code}: {detail}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CreateDirectory { source, .. } => Some(source),
            Self::InvalidProfile(error) => Some(error),
            Self::InvalidSetting(error) => Some(error),
            Self::InvalidHistory(error) => Some(error),
            Self::InvalidWorksheet(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProfileError> for StoreError {
    fn from(error: ProfileError) -> Self {
        Self::InvalidProfile(error)
    }
}

impl From<SettingError> for StoreError {
    fn from(error: SettingError) -> Self {
        Self::InvalidSetting(error)
    }
}

impl From<HistoryError> for StoreError {
    fn from(error: HistoryError) -> Self {
        Self::InvalidHistory(error)
    }
}

impl From<WorksheetError> for StoreError {
    fn from(error: WorksheetError) -> Self {
        Self::InvalidWorksheet(error)
    }
}

/// Whether SQLite's own message proves a schema difference — the file's
/// tables do not match what its header's version promises — rather than some
/// other bug behind the same generic `SQLITE_ERROR`/prepare-failure shape.
///
/// Deliberately conservative: only the message shapes this build's own SQL
/// would produce against a `history`/`worksheet`/`setting`/… table someone
/// altered outside Reldex. A message this does not recognise stays
/// [`StoreError::Sqlite`] — silence, not a guess, because mislabelling a
/// Reldex-side SQL bug as "schema mismatch" would send a user chasing the
/// wrong cause.
fn is_schema_mismatch_message(detail: &str) -> bool {
    detail.contains("no such table")
        || detail.contains("no such column")
        || detail.contains("has no column named")
        || (detail.contains("columns but") && detail.contains("values"))
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::SqliteFailure(failure, message) => {
                let detail = message.clone().unwrap_or_else(|| failure.to_string());
                match failure.code {
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Self::Busy,
                    ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt => {
                        Self::Corrupt { detail }
                    }
                    ErrorCode::ReadOnly => Self::ReadOnly,
                    // A bare `SQLITE_ERROR` from a failed `prepare` — SQLite's
                    // own generic bucket for a *lot* of unrelated failures,
                    // not only "no such table"/"no such column" (a malformed
                    // expression, a bad type mismatch, a bug in this crate's
                    // own SQL text all land here too). Only narrow this to
                    // `SchemaMismatch` when the message itself names a
                    // missing/mismatched table or column — never on the bare
                    // error code alone, which would mislabel a future
                    // Reldex-side SQL bug as "file altered outside Reldex".
                    ErrorCode::Unknown if is_schema_mismatch_message(&detail) => {
                        Self::SchemaMismatch { detail }
                    }
                    _ => Self::Sqlite {
                        code: failure.extended_code,
                        detail,
                    },
                }
            }
            // A statement SQLite could not prepare, with SQLite's own
            // diagnostic pinpointing the offending token. Same narrowing as
            // `ErrorCode::Unknown` above: this shape also carries ordinary
            // syntax errors, not only "no such column".
            rusqlite::Error::SqlInputError { msg, .. } if is_schema_mismatch_message(msg) => {
                Self::SchemaMismatch {
                    detail: msg.clone(),
                }
            }
            _ => Self::Sqlite {
                code: -1,
                detail: error.to_string(),
            },
        }
    }
}

impl StoreError {
    /// Maps a failure to open `path`, keeping the path for the message.
    pub(crate) fn opening(path: &std::path::Path, error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::SqliteFailure(failure, message)
                if matches!(
                    failure.code,
                    ErrorCode::CannotOpen | ErrorCode::PermissionDenied
                ) =>
            {
                Self::CannotOpen {
                    path: path.to_path_buf(),
                    detail: message.clone().unwrap_or_else(|| failure.to_string()),
                }
            }
            _ => Self::from(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    #[test]
    fn schema_mismatch_message_recognises_the_documented_shapes() {
        assert!(is_schema_mismatch_message("no such table: history"));
        assert!(is_schema_mismatch_message("no such column: bogus"));
        assert!(is_schema_mismatch_message(
            "table history has no column named bogus"
        ));
        assert!(is_schema_mismatch_message(
            "table history has 7 columns but 8 values were supplied"
        ));
    }

    #[test]
    fn schema_mismatch_message_rejects_an_unrelated_sqlite_error() {
        assert!(!is_schema_mismatch_message("near \"FRM\": syntax error"));
        assert!(!is_schema_mismatch_message(
            "database disk image is malformed"
        ));
        assert!(!is_schema_mismatch_message("UNIQUE constraint failed: t.x"));
    }

    /// A file missing a table its schema version promises — the case this
    /// mapping exists for — still maps to `SchemaMismatch`, not a bare
    /// `Sqlite`, regardless of which rusqlite error shape carries it.
    #[test]
    fn a_missing_table_maps_to_schema_mismatch_not_a_bare_sqlite_error() {
        let connection = Connection::open_in_memory().expect("open");
        let error = connection
            .execute("SELECT * FROM this_table_does_not_exist", [])
            .expect_err("missing table is an error");
        let mapped = StoreError::from(error);
        assert!(
            matches!(&mapped, StoreError::SchemaMismatch { detail } if detail.contains("no such table")),
            "expected SchemaMismatch naming the missing table, got {mapped:?}"
        );
    }

    /// A syntax error in this build's own SQL is a Reldex bug, not a file
    /// altered outside Reldex — the must-fix this narrowing exists for: it
    /// must never be mislabelled `SchemaMismatch`.
    #[test]
    fn an_unrelated_sql_bug_stays_a_bare_sqlite_error_not_schema_mismatch() {
        let connection = Connection::open_in_memory().expect("open");
        connection
            .execute("CREATE TABLE t (x INTEGER)", [])
            .expect("create");
        let error = connection
            .execute("SELECT * FRM t", [])
            .expect_err("malformed SQL is an error");
        let mapped = StoreError::from(error);
        assert!(
            matches!(&mapped, StoreError::Sqlite { .. }),
            "expected a bare Sqlite error for a syntax bug, got {mapped:?}"
        );
    }
}
