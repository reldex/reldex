//! The store's error type. SQLite's own error type never leaves this module:
//! a caller sees what went wrong in Reldex's terms, with SQLite's extended
//! result code kept for diagnostics.

use std::fmt;
use std::path::PathBuf;

use rusqlite::ErrorCode;

use crate::ids::ProfileId;
use crate::profile::ProfileError;
use crate::settings::SettingError;

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
    /// The file is not a database, or its contents are damaged.
    Corrupt {
        /// SQLite's description.
        detail: String,
    },
    /// Another connection held the file's lock for longer than the busy
    /// timeout. Nothing was written; the operation can be retried.
    Busy,
    /// The file could not be opened or created: a missing or unwritable
    /// directory, read-only media, a permission.
    CannotOpen {
        /// The file.
        path: PathBuf,
        /// SQLite's description.
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
                    _ => Self::Sqlite {
                        code: failure.extended_code,
                        detail,
                    },
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
                    ErrorCode::CannotOpen | ErrorCode::PermissionDenied | ErrorCode::ReadOnly
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
