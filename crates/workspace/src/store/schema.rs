//! The file format: how a file is recognised as a Reldex store, its schema,
//! and the forward-only migration path.
//!
//! # Version and identity live in the SQLite header
//!
//! `PRAGMA application_id` marks the file as Reldex's ([`APPLICATION_ID`],
//! the bytes `RLDX`) and `PRAGMA user_version` is its schema version. Both
//! are fields of the database header, written in the same transaction as the
//! schema change they describe, so the version and the schema cannot
//! disagree after a crash — which a separate version table could only
//! promise by convention. And because they can be read before any table
//! exists, the three cases an open must tell apart need no guessing:
//!
//! | `application_id` | `user_version` | Tables | Meaning |
//! | --- | --- | --- | --- |
//! | 0 | 0 | none | a new, empty file: migrate from version 0 |
//! | `RLDX` | 1..=[`SCHEMA_VERSION`] | ours | a Reldex store: migrate forward if older |
//! | `RLDX` | > [`SCHEMA_VERSION`] | — | written by a newer Reldex: **refused** |
//! | `RLDX` | 0 | some | a header Reldex never writes: **refused** |
//! | anything else | — | — | not a Reldex store: **refused** |
//!
//! A refusal happens before anything is written, including the switch to
//! WAL mode, so a file this build does not understand is left byte-for-byte
//! as it was.
//!
//! # Migration policy
//!
//! Forward only, one step per version, each step a function over an open
//! `IMMEDIATE` transaction; all pending steps and the new header values
//! commit together or not at all. There is no downgrade path: an older build
//! refuses a newer file ([`super::StoreError::NewerSchema`]) rather than
//! guess. Adding a **setting** never needs a migration — settings are rows,
//! and a key this build does not know is reported and kept, not deleted.
//! Adding a table (query history M4.10, workspace M6.2) or a column is a new
//! step appended to [`MIGRATIONS`].

use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::error::StoreError;

/// `PRAGMA application_id` of every Reldex store: `RLDX` in ASCII.
pub const APPLICATION_ID: i32 = 0x524C_4458;

/// The schema version this build writes and understands.
pub const SCHEMA_VERSION: u32 = 1;

/// One forward step: the version it produces and the DDL that produces it.
pub(crate) struct Migration {
    pub(crate) to: u32,
    pub(crate) apply: fn(&Transaction<'_>) -> rusqlite::Result<()>,
}

/// Every step, in order. `steps_are_contiguous` checks `to` runs 1, 2, 3, ….
pub(crate) const MIGRATIONS: &[Migration] = &[Migration { to: 1, apply: v1 }];

/// Version 1: profiles and settings.
///
/// Both tables are `STRICT`, so SQLite itself refuses a value of the wrong
/// storage class. Enumerations are stored as text and validated by the code
/// that reads them rather than by `CHECK` constraints: SQLite cannot alter a
/// `CHECK`, and a database type or environment added later must not need a
/// table rebuild. `CHECK`s are kept for invariants that will never change.
///
/// There is no column for a password, a key or a token, and none may be
/// added: `the_schema_has_no_column_that_could_hold_a_secret` checks every
/// column name of every table.
///
/// Ids compare `COLLATE NOCASE`: this build writes them in lowercase, but a
/// row edited by hand in uppercase still names the same profile, so an update
/// or a delete finds it rather than silently doing nothing.
const V1: &str = "
CREATE TABLE profile (
    id                           TEXT    NOT NULL COLLATE NOCASE PRIMARY KEY
        CHECK (length(id) = 36),
    name                         TEXT    NOT NULL CHECK (length(name) > 0),
    database_type                TEXT    NOT NULL,
    environment                  TEXT    NOT NULL,
    environment_label            TEXT,
    treat_as_production          INTEGER NOT NULL CHECK (treat_as_production IN (0, 1)),
    endpoint_kind                TEXT    NOT NULL,
    host                         TEXT,
    port                         INTEGER CHECK (port BETWEEN 1 AND 65535),
    service_name                 TEXT,
    sid                          TEXT,
    connect_string               TEXT,
    auth_kind                    TEXT    NOT NULL,
    username                     TEXT,
    password_in_credential_store INTEGER CHECK (password_in_credential_store IN (0, 1)),
    role                         TEXT    NOT NULL,
    transport                    TEXT    NOT NULL,
    ca_directory                 TEXT,
    allow_unenforced_certificate_pin INTEGER NOT NULL
        CHECK (allow_unenforced_certificate_pin IN (0, 1)),
    created_at                   INTEGER NOT NULL,
    modified_at                  INTEGER NOT NULL
) STRICT;

CREATE TABLE setting (
    scope       TEXT    NOT NULL CHECK (scope IN ('application', 'profile', 'worksheet')),
    scope_id    TEXT    NOT NULL COLLATE NOCASE,
    setting_key TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    int_value   INTEGER,
    text_value  TEXT,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (scope, scope_id, setting_key),
    CHECK ((scope = 'application') = (scope_id = '')),
    CHECK (int_value IS NULL OR text_value IS NULL)
) STRICT, WITHOUT ROWID;
";

fn v1(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(V1)
}

/// What an open found in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Header {
    /// A new file: no identity, no version, no tables.
    Empty,
    /// A Reldex store at this version (never newer than [`SCHEMA_VERSION`]).
    Reldex { version: u32 },
}

/// Reads the header and decides what the file is. Writes nothing.
///
/// One statement, so one snapshot: read as three statements, the identity,
/// the version and the table count could come from either side of another
/// connection's migration commit — "no identity, but tables" — and a store
/// being created next door would be refused as foreign (found by running
/// `two_opens_racing_on_a_new_file_both_succeed_and_migrate_once` in a loop).
pub(crate) fn inspect(connection: &Connection) -> Result<Header, StoreError> {
    let (application_id, user_version, objects): (i64, i64, i64) = connection.query_row(
        "SELECT (SELECT application_id FROM pragma_application_id), \
                (SELECT user_version FROM pragma_user_version), \
                (SELECT count(*) FROM sqlite_schema)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if application_id == i64::from(APPLICATION_ID) {
        let version = u32::try_from(user_version).map_err(|_| StoreError::NotAReldexStore)?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::NewerSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        // The identity and the version are set in the transaction that
        // creates the tables, so "ours, version 0, with tables" is a header
        // this build never writes. Migrating from 0 would fail half-way on
        // the existing tables at best; refuse before anything is written.
        if version == 0 && objects > 0 {
            return Err(StoreError::IncompleteHeader);
        }
        return Ok(Header::Reldex { version });
    }
    if application_id == 0 && user_version == 0 && objects == 0 {
        return Ok(Header::Empty);
    }
    Err(StoreError::NotAReldexStore)
}

/// The versions a migration went between; equal when there was nothing to
/// do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migrated {
    /// The version the file had.
    pub from: u32,
    /// The version it has now.
    pub to: u32,
}

/// Brings the file to [`SCHEMA_VERSION`].
pub(crate) fn migrate(connection: &mut Connection) -> Result<Migrated, StoreError> {
    migrate_with(connection, MIGRATIONS, SCHEMA_VERSION)
}

/// Brings the file to `target` using `steps`, in one transaction.
///
/// A file already at `target` is only read: opening a current store must not
/// need the write lock, or a second window would wait on (or fail against)
/// whatever the first one is writing. When there is work, the header is read
/// again **inside** the write transaction: two processes that open the same
/// new file at once both see version 0 before either migrates, and the
/// second must find the first one's work rather than repeat it.
pub(crate) fn migrate_with(
    connection: &mut Connection,
    steps: &[Migration],
    target: u32,
) -> Result<Migrated, StoreError> {
    if inspect(connection)? == (Header::Reldex { version: target }) {
        return Ok(Migrated {
            from: target,
            to: target,
        });
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let from = match inspect(&transaction)? {
        Header::Empty => 0,
        Header::Reldex { version } => version,
    };
    if from > target {
        return Err(StoreError::NewerSchema {
            found: from,
            supported: target,
        });
    }
    for step in steps
        .iter()
        .filter(|step| step.to > from && step.to <= target)
    {
        (step.apply)(&transaction)?;
    }
    if from < target {
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.pragma_update(None, "user_version", target)?;
    }
    transaction.commit()?;
    Ok(Migrated { from, to: target })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_contiguous_and_end_at_the_current_version() {
        for (index, step) in MIGRATIONS.iter().enumerate() {
            assert_eq!(step.to as usize, index + 1);
        }
        assert_eq!(MIGRATIONS.last().map(|step| step.to), Some(SCHEMA_VERSION));
    }

    #[test]
    fn the_application_id_spells_rldx() {
        assert_eq!(&APPLICATION_ID.to_be_bytes(), b"RLDX");
    }

    #[test]
    fn a_new_file_migrates_from_zero_and_a_second_run_does_nothing() {
        let mut connection = Connection::open_in_memory().expect("memory");
        assert_eq!(inspect(&connection).expect("inspect"), Header::Empty);
        assert_eq!(
            migrate(&mut connection).expect("migrate"),
            Migrated { from: 0, to: 1 }
        );
        assert_eq!(
            inspect(&connection).expect("inspect"),
            Header::Reldex { version: 1 }
        );
        assert_eq!(
            migrate(&mut connection).expect("migrate"),
            Migrated { from: 1, to: 1 }
        );
    }

    #[test]
    fn a_failing_step_leaves_the_file_exactly_as_it_was() {
        fn broken(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
            transaction.execute_batch("CREATE TABLE half_done (x INTEGER); SELECT * FROM nope;")
        }
        let steps = [
            Migration { to: 1, apply: v1 },
            Migration {
                to: 2,
                apply: broken,
            },
        ];
        let mut connection = Connection::open_in_memory().expect("memory");
        assert!(migrate_with(&mut connection, &steps, 2).is_err());
        // Neither step 1's tables, nor step 2's half, nor the header moved.
        assert_eq!(inspect(&connection).expect("inspect"), Header::Empty);
    }

    #[test]
    fn a_foreign_database_is_not_a_reldex_store() {
        let connection = Connection::open_in_memory().expect("memory");
        connection
            .execute_batch("CREATE TABLE theirs (x INTEGER)")
            .expect("create");
        assert!(matches!(
            inspect(&connection),
            Err(StoreError::NotAReldexStore)
        ));

        let other_app = Connection::open_in_memory().expect("memory");
        other_app
            .pragma_update(None, "application_id", 0x1234_5678)
            .expect("pragma");
        assert!(matches!(
            inspect(&other_app),
            Err(StoreError::NotAReldexStore)
        ));

        let versioned = Connection::open_in_memory().expect("memory");
        versioned
            .pragma_update(None, "user_version", 7)
            .expect("pragma");
        assert!(matches!(
            inspect(&versioned),
            Err(StoreError::NotAReldexStore)
        ));
    }

    #[test]
    fn a_newer_version_is_refused_by_inspect_and_by_migrate() {
        let mut connection = Connection::open_in_memory().expect("memory");
        migrate(&mut connection).expect("migrate");
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("pragma");
        assert!(matches!(
            inspect(&connection),
            Err(StoreError::NewerSchema { found, supported })
                if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION
        ));
        assert!(matches!(
            migrate(&mut connection),
            Err(StoreError::NewerSchema { .. })
        ));
    }
}
