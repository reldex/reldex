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
                    // own generic bucket for "no such table"/"no such
                    // column", which is how a *missing table* surfaces
                    // (rusqlite reports "no such column" as `SqlInputError`
                    // below, but "no such table" as a plain `SqliteFailure`
                    // with this code). This build's SQL is fixed and tested,
                    // so against a file whose header this build already
                    // accepted, either message means the same thing: a table
                    // or column the file's schema version promises is not
                    // there — a file altered outside Reldex.
                    ErrorCode::Unknown => Self::SchemaMismatch { detail },
                    _ => Self::Sqlite {
                        code: failure.extended_code,
                        detail,
                    },
                }
            }
            // A statement SQLite could not prepare, with SQLite's own
            // diagnostic pinpointing the offending token — how "no such
            // column" surfaces. Same reasoning as `ErrorCode::Unknown` above.
            rusqlite::Error::SqlInputError { msg, .. } => Self::SchemaMismatch {
                detail: msg.clone(),
            },
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
