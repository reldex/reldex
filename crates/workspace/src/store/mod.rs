//! The SQLite store: one file holding profiles and settings (owner decision
//! 2026-09-20, `phase-1.md` §C.3 item 8), query history (M4.10, `store::history`)
//! and workspace state (M6.2, `store::worksheet`).
//!
//! # Threading contract
//!
//! [`Store`] is `Send` and **not** `Sync`. It is owned by the UI-side
//! workspace service's own thread — the thread that answers the UI's
//! requests for profiles and settings — and is **never opened or used on the
//! UI thread**: every method does disk I/O, and a write can wait up to the
//! busy timeout for another connection's lock. The UI asks that thread and
//! is answered asynchronously, exactly as it is for database work
//! (`ARCHITECTURE.md` §6). It never touches a database session's worker
//! thread either: nothing here is on the query path.
//!
//! Two handles on one file — two processes, or a second window — are safe:
//! the file is in WAL mode, so readers never block the writer or each other,
//! and a writer that finds the lock held waits for the busy timeout and then
//! fails with [`StoreError::Busy`], never a panic and never a partial write.
//!
//! The busy timeout bounds each wait, not a whole open: one open can wait up
//! to four times in a row — reading the header, converting the file to WAL,
//! re-reading the header before migrating, and taking the migration's
//! `IMMEDIATE` lock — and on Windows each wait was measured at up to about
//! 1.5× the timeout (SQLite's busy handler sleeps in coarse steps there).
//! There is no overall deadline; the service thread must not assume one.
//!
//! # Permissions
//!
//! [`Store::open_default`] creates the data directory owner-only (`0700`) and
//! a new store file owner-only (`0600`) on Unix, before SQLite touches it;
//! SQLite gives the `-wal` and `-shm` files the store file's permissions.
//! Existing directories and files are left as they are. On Windows the
//! per-user `%LOCALAPPDATA%` already restricts access to the user.
//!
//! # Durability
//!
//! Every write is one `IMMEDIATE` transaction: it takes the write lock
//! before reading anything, so the check-then-write inside it (does this
//! profile exist?) cannot race another writer. `synchronous = FULL`: a
//! settings change the user saw succeed is on disk.

mod codec;
mod error;
mod history;
mod paths;
mod schema;
mod worksheet;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

pub use error::StoreError;
pub use paths::{APP_ID, STORE_FILE_NAME, default_data_dir, default_store_path};
pub use schema::{APPLICATION_ID, Migrated, SCHEMA_VERSION};

use crate::ids::{ProfileId, WorksheetId};
use crate::profile::Profile;
use crate::settings::{
    ApplicationScope, Level, ProfileScope, ScopeLevel, Setting, SettingError, SettingId,
    SettingType, SettingValue, SettingsLayer, WorksheetScope,
};
use crate::time::UnixTimeMs;

use codec::{DecodeValueError, PROFILE_COLUMNS, ProfileRow};

/// Where a setting value is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Scope {
    /// The application defaults.
    Application,
    /// One profile's overrides.
    Profile(ProfileId),
    /// One worksheet's overrides.
    Worksheet(WorksheetId),
}

impl Scope {
    /// The level this scope's values resolve at.
    #[must_use]
    pub const fn level(self) -> Level {
        match self {
            Self::Application => Level::Application,
            Self::Profile(_) => Level::Profile,
            Self::Worksheet(_) => Level::Worksheet,
        }
    }

    fn id_text(self) -> String {
        match self {
            Self::Application => String::new(),
            Self::Profile(id) => id.to_string(),
            Self::Worksheet(id) => id.to_string(),
        }
    }
}

/// Which table a rejected row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StoreTable {
    /// Connection profiles.
    Profile,
    /// Setting values.
    Setting,
    /// Open worksheets.
    Worksheet,
}

/// Why a stored row was left out of what was loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RejectReason {
    /// A setting this build does not know — perhaps a newer Reldex's. The row
    /// is kept.
    UnknownSetting,
    /// A value kind this build does not know.
    UnknownKind(String),
    /// A value that does not decode as its kind.
    Malformed,
    /// A value this build's rules refuse (level, kind or bounds). The level
    /// below is used instead.
    Refused(SettingError),
    /// A profile row that does not decode; the text names the column.
    Undecodable(String),
}

/// A row that was not loaded, and why. Reported, not dropped silently and
/// not deleted: a UI can say "one setting could not be read and its default
/// is used instead".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedRow {
    /// The table.
    pub table: StoreTable,
    /// The row's key: the setting's storage key, or the profile's id, as
    /// stored.
    pub key: String,
    /// Why.
    pub reason: RejectReason,
}

/// What was loaded, plus the rows that could not be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded<T> {
    /// The usable part.
    pub value: T,
    /// Rows left out, in the order they were read.
    pub rejected: Vec<RejectedRow>,
}

/// Tunables for opening a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct StoreOptions {
    /// How long a write waits for another connection's lock before
    /// [`StoreError::Busy`]. Default 5 s.
    pub busy_timeout: Duration,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            busy_timeout: Duration::from_secs(5),
        }
    }
}

impl StoreOptions {
    /// Sets the busy timeout.
    #[must_use]
    pub const fn with_busy_timeout(mut self, busy_timeout: Duration) -> Self {
        self.busy_timeout = busy_timeout;
        self
    }
}

/// The local store: profiles and settings in one SQLite file.
///
/// See the module documentation for the threading contract: `Send`, not
/// `Sync`, owned by the workspace service thread, never the UI thread.
///
/// ```
/// fn assert_send<T: Send>() {}
/// assert_send::<reldex_workspace::Store>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<reldex_workspace::Store>();
/// ```
#[derive(Debug)]
pub struct Store {
    connection: Connection,
    path: Option<PathBuf>,
    journal_mode: String,
    migrated: Migrated,
}

// The contract above, checked: moving a store to its service thread must
// compile.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Store>();
};

impl Store {
    /// Opens (creating if needed) the store at `path`, migrating it forward.
    ///
    /// Does disk I/O and can wait for another connection's lock up to four
    /// times, each bounded by the busy timeout but with no overall deadline
    /// (see the module documentation): call it on the service thread.
    ///
    /// # Errors
    ///
    /// [`StoreError::NewerSchema`], [`StoreError::NotAReldexStore`] and
    /// [`StoreError::IncompleteHeader`] before anything is written to the
    /// file; [`StoreError::Corrupt`] for a file that is not a database;
    /// [`StoreError::CannotOpen`] for a path that cannot be opened;
    /// [`StoreError::ReadOnly`] for a file that cannot be written;
    /// [`StoreError::Busy`] when another connection holds the lock
    /// throughout a wait.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with(path, StoreOptions::default())
    }

    /// [`Store::open`] with explicit options.
    ///
    /// # Errors
    ///
    /// As [`Store::open`].
    pub fn open_with(path: impl AsRef<Path>, options: StoreOptions) -> Result<Self, StoreError> {
        let path = path.as_ref();
        // No `SQLITE_OPEN_URI`: a path is a path, never a `file:` URI with
        // query parameters that could change how the file is opened.
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection =
            Connection::open_with_flags(path, flags).map_err(|e| StoreError::opening(path, e))?;
        Self::initialise(connection, Some(path.to_path_buf()), options)
    }

    /// Opens the store at [`default_store_path`], creating its directory.
    ///
    /// On Unix a directory it creates is owner-only (`0700`), and so is a
    /// store file it creates (`0600`); see the module documentation.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoDataDirectory`] where the platform has none Reldex can
    /// derive (pass an explicit path there), [`StoreError::CreateDirectory`],
    /// [`StoreError::CannotOpen`] if the file cannot be created, or anything
    /// [`Store::open`] returns.
    pub fn open_default() -> Result<Self, StoreError> {
        let directory = default_data_dir().ok_or(StoreError::NoDataDirectory)?;
        Self::open_in_directory(&directory, StoreOptions::default())
    }

    /// Creates `directory` and the store file in it, owner-only on Unix, then
    /// opens the file. What [`Store::open_default`] does with its directory.
    pub(crate) fn open_in_directory(
        directory: &Path,
        options: StoreOptions,
    ) -> Result<Self, StoreError> {
        private::create_directory(directory).map_err(|source| StoreError::CreateDirectory {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = directory.join(STORE_FILE_NAME);
        private::create_file(&path).map_err(|error| StoreError::CannotOpen {
            path: path.clone(),
            detail: error.to_string(),
        })?;
        Self::open_with(path, options)
    }

    /// As [`Store::open`], but first creates `path`'s parent directory and
    /// the store file itself if either is missing -- owner-only on Unix,
    /// exactly like [`Store::open_default`] does for its own directory
    /// (module documentation). This is the caller-supplied-path counterpart
    /// to [`Store::open_default`]/`Store::open_in_directory` (crate-private,
    /// so not a doc link here), for a caller that (unlike them) does not fix
    /// the file name: `reldex-ffi`'s
    /// `reldex_workspace_open` takes a full path from across the ABI (an
    /// explicit override, or the C++ adapter's own copy of the ADR-0006 P5
    /// platform-default-path rule) and used to call [`Store::open`] directly,
    /// which does not create anything -- silently falling back to
    /// [`Store::open_in_memory`] whenever the directory happened not to
    /// exist yet, and otherwise leaving directory/file creation (with its
    /// permissions) to be reimplemented on the C++ side of the boundary, on
    /// whichever thread called it. Doing it here instead means every ABI
    /// caller gets ADR-0006 P5 for free, and on the right thread: this runs
    /// wherever the caller calls it, which for `reldex_workspace_open` is
    /// the workspace's own service thread (`service_main`), never a UI
    /// thread.
    ///
    /// # Errors
    ///
    /// [`StoreError::CreateDirectory`], [`StoreError::CannotOpen`] if the
    /// file cannot be created, or anything [`Store::open`] returns.
    pub fn open_creating(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(directory) = path
            .parent()
            .filter(|directory| !directory.as_os_str().is_empty())
        {
            private::create_directory(directory).map_err(|source| StoreError::CreateDirectory {
                path: directory.to_path_buf(),
                source,
            })?;
        }
        private::create_file(path).map_err(|error| StoreError::CannotOpen {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
        Self::open_with(path, StoreOptions::default())
    }

    /// A private, in-memory store, gone when dropped. For tests and for a
    /// session that must not touch disk.
    ///
    /// # Errors
    ///
    /// Only if SQLite itself fails.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        Self::initialise(connection, None, StoreOptions::default())
    }

    fn initialise(
        mut connection: Connection,
        path: Option<PathBuf>,
        options: StoreOptions,
    ) -> Result<Self, StoreError> {
        connection.busy_timeout(options.busy_timeout)?;
        // Refuse a file this build does not understand before writing to it
        // at all — switching to WAL is itself a write.
        schema::inspect(&connection)?;
        let journal_mode = if path.as_deref().is_some_and(|p| !paths::is_memory(p)) {
            enable_wal(&connection, options.busy_timeout)?
        } else {
            connection.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?
        };
        connection.pragma_update(None, "synchronous", "FULL")?;
        // No foreign key exists in schema 1; on from the start so the first
        // one a later migration adds (history rows naming a profile, M4.10)
        // is enforced rather than decorative.
        connection.pragma_update(None, "foreign_keys", true)?;
        let migrated = schema::migrate(&mut connection)?;
        Ok(Self {
            connection,
            path,
            journal_mode: journal_mode.to_ascii_lowercase(),
            migrated,
        })
    }

    /// The file, or `None` for an in-memory store.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether the file is in WAL mode. Always the case for a file on a local
    /// disk; SQLite keeps a rollback journal where WAL is impossible (some
    /// network file systems), which is still atomic but serialises readers
    /// with the writer.
    #[must_use]
    pub fn is_wal(&self) -> bool {
        self.journal_mode == "wal"
    }

    /// What opening this store did to its schema.
    #[must_use]
    pub const fn migration(&self) -> Migrated {
        self.migrated
    }

    /// The file's schema version, read from its header.
    ///
    /// # Errors
    ///
    /// Only if SQLite fails.
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        u32::try_from(version).map_err(|_| StoreError::NotAReldexStore)
    }

    /// Runs SQLite's `quick_check` over the whole file. Costs a read of the
    /// file; for diagnostics, not for every open.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] with SQLite's first finding.
    pub fn check_integrity(&self) -> Result<(), StoreError> {
        let finding: String = self
            .connection
            .pragma_query_value(None, "quick_check", |row| row.get(0))?;
        if finding == "ok" {
            Ok(())
        } else {
            Err(StoreError::Corrupt { detail: finding })
        }
    }

    // ---- profiles --------------------------------------------------------

    /// Adds a profile.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidProfile`], [`StoreError::ProfileExists`], or an
    /// SQLite failure. Nothing is written on error.
    pub fn insert_profile(&mut self, profile: &Profile) -> Result<(), StoreError> {
        let row = encoded(profile)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = transaction.execute(
            &format!(
                "INSERT INTO profile ({PROFILE_COLUMNS}) VALUES \
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
                 ?18, ?19, ?20, ?21) ON CONFLICT (id) DO NOTHING"
            ),
            params![
                row.id,
                row.name,
                row.database_type,
                row.environment,
                row.environment_label,
                row.treat_as_production,
                row.endpoint_kind,
                row.host,
                row.port,
                row.service_name,
                row.sid,
                row.connect_string,
                row.auth_kind,
                row.username,
                row.password_in_credential_store,
                row.role,
                row.transport,
                row.ca_directory,
                row.allow_unenforced_certificate_pin,
                row.created_at,
                row.modified_at,
            ],
        )?;
        if inserted == 0 {
            return Err(StoreError::ProfileExists(profile.id()));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Replaces a stored profile's details and modification time. Its
    /// creation time is kept as stored.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidProfile`], [`StoreError::ProfileNotFound`], or an
    /// SQLite failure. Nothing is written on error.
    pub fn update_profile(&mut self, profile: &Profile) -> Result<(), StoreError> {
        let row = encoded(profile)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = transaction.execute(
            "UPDATE profile SET name = ?2, database_type = ?3, environment = ?4, \
             environment_label = ?5, treat_as_production = ?6, endpoint_kind = ?7, host = ?8, \
             port = ?9, service_name = ?10, sid = ?11, connect_string = ?12, auth_kind = ?13, \
             username = ?14, password_in_credential_store = ?15, role = ?16, transport = ?17, \
             ca_directory = ?18, allow_unenforced_certificate_pin = ?19, modified_at = ?20 \
             WHERE id = ?1",
            params![
                row.id,
                row.name,
                row.database_type,
                row.environment,
                row.environment_label,
                row.treat_as_production,
                row.endpoint_kind,
                row.host,
                row.port,
                row.service_name,
                row.sid,
                row.connect_string,
                row.auth_kind,
                row.username,
                row.password_in_credential_store,
                row.role,
                row.transport,
                row.ca_directory,
                row.allow_unenforced_certificate_pin,
                row.modified_at,
            ],
        )?;
        if updated == 0 {
            return Err(StoreError::ProfileNotFound(profile.id()));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Deletes a profile and, in the same transaction, every setting it
    /// overrides. Returns whether it existed.
    ///
    /// The profile's password is not here to delete: the caller removes it
    /// from the credential store under [`Profile::credential_key`] (M2.10).
    ///
    /// # Errors
    ///
    /// An SQLite failure; nothing is deleted on error.
    pub fn delete_profile(&mut self, id: ProfileId) -> Result<bool, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = id.to_string();
        transaction.execute(
            "DELETE FROM setting WHERE scope = 'profile' AND scope_id = ?1",
            params![key],
        )?;
        let deleted = transaction.execute("DELETE FROM profile WHERE id = ?1", params![key])?;
        transaction.commit()?;
        Ok(deleted > 0)
    }

    /// One profile.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidRow`] if its row does not decode, or an SQLite
    /// failure.
    pub fn profile(&self, id: ProfileId) -> Result<Option<Profile>, StoreError> {
        let row = self
            .connection
            .query_row(
                &format!("SELECT {PROFILE_COLUMNS} FROM profile WHERE id = ?1"),
                params![id.to_string()],
                ProfileRow::from_sql,
            )
            .optional()?;
        row.map(|row| {
            codec::decode_profile(row).map_err(|detail| StoreError::InvalidRow { detail })
        })
        .transpose()
    }

    /// Every profile, by name. A row that does not decode is reported in
    /// [`Loaded::rejected`] rather than hiding every other profile.
    ///
    /// # Errors
    ///
    /// An SQLite failure.
    pub fn profiles(&self) -> Result<Loaded<Vec<Profile>>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {PROFILE_COLUMNS} FROM profile ORDER BY name, id"
        ))?;
        let rows = statement.query_map([], ProfileRow::from_sql)?;
        let mut loaded = Loaded {
            value: Vec::new(),
            rejected: Vec::new(),
        };
        for row in rows {
            let row = row?;
            let key = row.id.clone();
            match codec::decode_profile(row) {
                Ok(profile) => loaded.value.push(profile),
                Err(detail) => loaded.rejected.push(RejectedRow {
                    table: StoreTable::Profile,
                    key,
                    reason: RejectReason::Undecodable(detail),
                }),
            }
        }
        Ok(loaded)
    }

    // ---- settings --------------------------------------------------------

    /// The application-level values.
    ///
    /// # Errors
    ///
    /// An SQLite failure. Rows that cannot be used are reported in
    /// [`Loaded::rejected`].
    pub fn application_settings(
        &self,
    ) -> Result<Loaded<SettingsLayer<ApplicationScope>>, StoreError> {
        self.load_layer(Scope::Application)
    }

    /// One profile's overrides.
    ///
    /// # Errors
    ///
    /// As [`Store::application_settings`].
    pub fn profile_settings(
        &self,
        id: ProfileId,
    ) -> Result<Loaded<SettingsLayer<ProfileScope>>, StoreError> {
        self.load_layer(Scope::Profile(id))
    }

    /// One worksheet's overrides.
    ///
    /// # Errors
    ///
    /// As [`Store::application_settings`].
    pub fn worksheet_settings(
        &self,
        id: WorksheetId,
    ) -> Result<Loaded<SettingsLayer<WorksheetScope>>, StoreError> {
        self.load_layer(Scope::Worksheet(id))
    }

    /// Both scope tables read the same four columns; `Scope::Worksheet` reads
    /// [`worksheet_setting`] — the FK-backed table ADR-0006 moved worksheet
    /// overrides into — everything else still reads `setting`. Collected
    /// into a `Vec` first because the two branches prepare different
    /// statements (different concrete `Rows<'_>` types).
    fn load_layer<S: ScopeLevel>(
        &self,
        scope: Scope,
    ) -> Result<Loaded<SettingsLayer<S>>, StoreError> {
        debug_assert_eq!(scope.level(), S::LEVEL);
        type Row = (String, String, Option<i64>, Option<String>);
        let to_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Row> {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        };
        let rows: Vec<Row> = match scope {
            Scope::Worksheet(id) => {
                let mut statement = self.connection.prepare(
                    "SELECT setting_key, kind, int_value, text_value FROM worksheet_setting \
                     WHERE worksheet_id = ?1 ORDER BY setting_key",
                )?;
                statement
                    .query_map(params![id.to_string()], to_row)?
                    .collect::<rusqlite::Result<_>>()?
            }
            Scope::Application | Scope::Profile(_) => {
                let mut statement = self.connection.prepare(
                    "SELECT setting_key, kind, int_value, text_value FROM setting \
                     WHERE scope = ?1 AND scope_id = ?2 ORDER BY setting_key",
                )?;
                statement
                    .query_map(
                        params![codec::level_word(scope.level()), scope.id_text()],
                        to_row,
                    )?
                    .collect::<rusqlite::Result<_>>()?
            }
        };
        let mut loaded = Loaded {
            value: SettingsLayer::<S>::new(),
            rejected: Vec::new(),
        };
        for row in rows {
            let (key, kind, int, text) = row;
            let reject = |reason| RejectedRow {
                table: StoreTable::Setting,
                key: key.clone(),
                reason,
            };
            let Some(id) = SettingId::from_storage_key(&key) else {
                loaded.rejected.push(reject(RejectReason::UnknownSetting));
                continue;
            };
            let value = match codec::decode_value(&kind, int, text.as_deref()) {
                Ok(value) => value,
                Err(DecodeValueError::UnknownKind(kind)) => {
                    loaded
                        .rejected
                        .push(reject(RejectReason::UnknownKind(kind)));
                    continue;
                }
                Err(DecodeValueError::Malformed) => {
                    loaded.rejected.push(reject(RejectReason::Malformed));
                    continue;
                }
            };
            if let Err(error) = loaded.value.set_value(id, value) {
                loaded.rejected.push(reject(RejectReason::Refused(error)));
            }
        }
        Ok(loaded)
    }

    /// Writes one setting value at `scope`, replacing any value there.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidSetting`] if the setting may not be set at that
    /// level or the value is out of bounds; [`StoreError::ProfileNotFound`]
    /// for a profile scope whose profile does not exist; an SQLite failure.
    /// Nothing is written on error.
    pub fn put_setting<T: SettingType>(
        &mut self,
        scope: Scope,
        setting: Setting<T>,
        value: T,
    ) -> Result<(), StoreError> {
        self.put_setting_value(scope, setting.id(), value.into_value())
    }

    /// Writes a setting chosen at run time.
    ///
    /// # Errors
    ///
    /// As [`Store::put_setting`], plus a kind mismatch.
    /// [`StoreError::WorksheetNotFound`] for a worksheet scope whose
    /// worksheet does not exist — the same rule [`StoreError::ProfileNotFound`]
    /// already enforced for a profile scope, now possible for a worksheet
    /// scope too because `worksheet_setting` has a real foreign key to
    /// `worksheet(id)` (ADR-0006, resolved by M6.2).
    pub fn put_setting_value(
        &mut self,
        scope: Scope,
        id: SettingId,
        value: SettingValue,
    ) -> Result<(), StoreError> {
        id.descriptor().check(scope.level(), value)?;
        let (kind, int) = codec::encode_value(value);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Scope::Profile(profile) = scope {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM profile WHERE id = ?1",
                    params![profile.to_string()],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(StoreError::ProfileNotFound(profile));
            }
        }
        match scope {
            Scope::Worksheet(worksheet) => {
                let exists = transaction
                    .query_row(
                        "SELECT 1 FROM worksheet WHERE id = ?1",
                        params![worksheet.to_string()],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !exists {
                    return Err(StoreError::WorksheetNotFound(worksheet));
                }
                transaction.execute(
                    "INSERT INTO worksheet_setting (worksheet_id, setting_key, kind, \
                     int_value, text_value, updated_at) VALUES (?1, ?2, ?3, ?4, NULL, ?5) \
                     ON CONFLICT (worksheet_id, setting_key) DO UPDATE SET \
                     kind = excluded.kind, int_value = excluded.int_value, text_value = NULL, \
                     updated_at = excluded.updated_at",
                    params![
                        worksheet.to_string(),
                        id.storage_key(),
                        kind,
                        int,
                        UnixTimeMs::now().as_millis(),
                    ],
                )?;
            }
            Scope::Application | Scope::Profile(_) => {
                transaction.execute(
                    "INSERT INTO setting (scope, scope_id, setting_key, kind, int_value, \
                     text_value, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6) \
                     ON CONFLICT (scope, scope_id, setting_key) DO UPDATE SET \
                     kind = excluded.kind, int_value = excluded.int_value, text_value = NULL, \
                     updated_at = excluded.updated_at",
                    params![
                        codec::level_word(scope.level()),
                        scope.id_text(),
                        id.storage_key(),
                        kind,
                        int,
                        UnixTimeMs::now().as_millis(),
                    ],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Removes the value at `scope`, so the setting is inherited again.
    /// Returns whether there was one.
    ///
    /// # Errors
    ///
    /// An SQLite failure.
    pub fn clear_setting(&mut self, scope: Scope, id: SettingId) -> Result<bool, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = match scope {
            Scope::Worksheet(worksheet) => transaction.execute(
                "DELETE FROM worksheet_setting WHERE worksheet_id = ?1 AND setting_key = ?2",
                params![worksheet.to_string(), id.storage_key()],
            )?,
            Scope::Application | Scope::Profile(_) => transaction.execute(
                "DELETE FROM setting WHERE scope = ?1 AND scope_id = ?2 AND setting_key = ?3",
                params![
                    codec::level_word(scope.level()),
                    scope.id_text(),
                    id.storage_key()
                ],
            )?,
        };
        transaction.commit()?;
        Ok(deleted > 0)
    }

    /// Removes every value a worksheet overrides, without deleting the
    /// worksheet itself. Returns how many were removed.
    ///
    /// [`Store::delete_worksheet`] removes them too, via
    /// `worksheet_setting`'s `ON DELETE CASCADE` — this method is for
    /// resetting a worksheet's overrides to inherited defaults while keeping
    /// the worksheet open.
    ///
    /// # Errors
    ///
    /// An SQLite failure.
    pub fn clear_worksheet_settings(&mut self, id: WorksheetId) -> Result<usize, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = transaction.execute(
            "DELETE FROM worksheet_setting WHERE worksheet_id = ?1",
            params![id.to_string()],
        )?;
        transaction.commit()?;
        Ok(deleted)
    }
}

/// Puts the file in WAL mode, returning the journal mode in force.
///
/// A file already in WAL mode is left alone. Converting one is the only step
/// of an open for which SQLite does **not** consult the busy handler: a
/// second connection converting the same new file at the same moment gets
/// `SQLITE_BUSY` at once (found by
/// `two_opens_racing_on_a_new_file_both_succeed_and_migrate_once`). So this
/// retries, with a short backoff, for as long as the busy timeout allows —
/// the same patience every other step of an open gets from SQLite itself.
fn enable_wal(connection: &Connection, patience: Duration) -> Result<String, StoreError> {
    let started = Instant::now();
    let mut pause = Duration::from_millis(1);
    loop {
        let current: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if current.eq_ignore_ascii_case("wal") {
            return Ok(current);
        }
        match connection
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
        {
            Ok(mode) => return Ok(mode),
            Err(error) => match StoreError::from(error) {
                StoreError::Busy if started.elapsed() < patience => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(50));
                }
                other => return Err(other),
            },
        }
    }
}

/// Owner-only creation on Unix; plain creation elsewhere.
mod private {
    use std::io;
    use std::path::Path;

    /// Creates `directory` and any missing parents; on Unix each one it
    /// creates is `0700`. An existing directory is left as it is.
    pub(super) fn create_directory(directory: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(directory)
        }
    }

    /// Creates `path` empty if it does not exist — `0600` on Unix — so SQLite
    /// opens a file that was private from its first byte. An existing file is
    /// left as it is.
    pub(super) fn create_file(path: &Path) -> io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn encoded(profile: &Profile) -> Result<ProfileRow, StoreError> {
    profile.details().validate()?;
    codec::encode_profile(profile).ok_or(StoreError::InvalidProfile(
        crate::profile::ProfileError::PathNotUnicode,
    ))
}

#[cfg(test)]
mod tests;
