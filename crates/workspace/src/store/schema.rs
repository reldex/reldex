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
//! Adding a table or a column is a new step appended to [`MIGRATIONS`]: step
//! 2 (query history, M4.10), step 3 (workspace state, M6.2) and step 4
//! (`history_meta`'s per-profile counter, same task, review follow-up) are
//! exactly that.

use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::error::StoreError;

/// `PRAGMA application_id` of every Reldex store: `RLDX` in ASCII.
pub const APPLICATION_ID: i32 = 0x524C_4458;

/// The schema version this build writes and understands.
pub const SCHEMA_VERSION: u32 = 4;

/// One forward step: the version it produces and the DDL that produces it.
pub(crate) struct Migration {
    pub(crate) to: u32,
    pub(crate) apply: fn(&Transaction<'_>) -> rusqlite::Result<()>,
}

/// Every step, in order. `steps_are_contiguous` checks `to` runs 1, 2, 3, ….
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration { to: 1, apply: v1 },
    Migration { to: 2, apply: v2 },
    Migration { to: 3, apply: v3 },
    Migration { to: 4, apply: v4 },
];

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

/// Version 2: query history (M4.10), one row per statement run.
///
/// `id` is the SQLite rowid (`INTEGER PRIMARY KEY`), `AUTOINCREMENT` so an id
/// handed out as a paging cursor ([`crate::HistoryPage::before`]) is never
/// reused, even after the row it named is trimmed. `profile_id` cascades: a
/// deleted profile's history goes with it. `statement` has no length
/// constraint in the schema — [`crate::history::MAX_STATEMENT_BYTES`] is
/// enforced in code, where the message can say why, not by a `CHECK` SQLite
/// would reject with no context.
///
/// `outcome` is one of a fixed set of words, like every other enumeration in
/// this schema; `native_code` is meaningful only for `'failed'`, which the
/// `CHECK` enforces so a decoded row can never disagree with itself.
const V2: &str = "
CREATE TABLE history (
    id          INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    profile_id  TEXT    NOT NULL COLLATE NOCASE REFERENCES profile (id) ON DELETE CASCADE,
    executed_at INTEGER NOT NULL,
    statement   TEXT    NOT NULL,
    outcome     TEXT    NOT NULL CHECK (outcome IN ('succeeded', 'failed', 'cancelled', 'timed_out')),
    native_code INTEGER,
    elapsed_ms  INTEGER NOT NULL,
    row_count   INTEGER,
    CHECK ((outcome = 'failed') OR (native_code IS NULL))
) STRICT;

CREATE INDEX history_profile_id_idx ON history (profile_id, id);
";

fn v2(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(V2)
}

/// Version 3: workspace state (M6.2) — open worksheets, worksheet-scoped
/// settings, and the workspace's own layout. Non-transactional state only:
/// no column here names a session or a transaction (`crate::worksheet`'s
/// module documentation).
///
/// **`setting`'s `scope = 'worksheet'` rows move to a table of their own**,
/// `worksheet_setting`, with a real foreign key to `worksheet(id)` and
/// `ON DELETE CASCADE` — the "real FK target" ADR-0006 deferred to this
/// migration. A single shared `scope_id` column cannot carry that FK: SQLite
/// enforces a foreign key over every row of the column it is declared on,
/// and `setting.scope_id` also holds profile ids and the empty string for
/// the application scope, neither of which names a row in `worksheet`. The
/// existing `setting` table keeps `application` and `profile` rows exactly
/// as schema 1 and 2 left them (its own `CHECK` still names `'worksheet'` as
/// a value the column *type* allows — `CHECK`s cannot be altered — but the
/// application code never writes that scope there again).
///
/// Schema 1/2 never shipped a `worksheet` table, so any pre-existing
/// `scope = 'worksheet'` row in `setting` cannot name a real worksheet — it
/// is already an orphan by construction — and is dropped rather than carried
/// forward into a table whose foreign key it cannot satisfy.
const V3: &str = "
DELETE FROM setting WHERE scope = 'worksheet';

CREATE TABLE worksheet (
    id         TEXT    NOT NULL COLLATE NOCASE PRIMARY KEY CHECK (length(id) = 36),
    profile_id TEXT    COLLATE NOCASE REFERENCES profile (id) ON DELETE SET NULL,
    title      TEXT    NOT NULL,
    text       TEXT    NOT NULL,
    caret      INTEGER NOT NULL,
    scroll     INTEGER NOT NULL,
    tab_order  INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE worksheet_setting (
    worksheet_id TEXT    NOT NULL COLLATE NOCASE REFERENCES worksheet (id) ON DELETE CASCADE,
    setting_key  TEXT    NOT NULL,
    kind         TEXT    NOT NULL,
    int_value    INTEGER,
    text_value   TEXT,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (worksheet_id, setting_key),
    CHECK (int_value IS NULL OR text_value IS NULL)
) STRICT, WITHOUT ROWID;

CREATE TABLE layout (
    id                    INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    active_worksheet_id   TEXT    COLLATE NOCASE REFERENCES worksheet (id) ON DELETE SET NULL,
    active_profile_id     TEXT    COLLATE NOCASE REFERENCES profile (id) ON DELETE SET NULL,
    object_browser_width  INTEGER,
    result_pane_height    INTEGER,
    window_x              INTEGER,
    window_y              INTEGER,
    window_width          INTEGER,
    window_height         INTEGER,
    window_maximized      INTEGER NOT NULL CHECK (window_maximized IN (0, 1)),
    updated_at            INTEGER NOT NULL
) STRICT;
";

fn v3(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(V3)
}

/// Version 4: `history_meta`, a per-profile running count that makes
/// [`super::history`]'s FIFO trim O(1) amortized instead of O(min(rows,
/// limit)) per insert.
///
/// A prior release trimmed by re-deriving "keep the newest `limit`" with
/// `DELETE … WHERE id NOT IN (SELECT id … ORDER BY id DESC LIMIT ?)` on every
/// insert — correct, but its cost scales with the limit (and, once the table
/// has grown past it, with the table): measured at 400 µs/insert flat at
/// limit 1,000, but 430 µs → 11.4 ms/insert as the table grows 2k→20k rows at
/// limit 100,000 (release, in-memory; see the ADR-0006 amendment's
/// measurement table). `history_meta` replaces that with a maintained counter
/// so trimming a *steady-state* insert deletes exactly one row: insert, then
/// `count += 1`; if over the limit, delete the oldest `count − limit` rows
/// (1, in steady state) by an index-bound `ORDER BY id ASC LIMIT k`, and
/// `count -= k`. Lowering the limit is caught up in a single O(k) pass on the
/// *next* insert, not proactively — documented in [`super::history`].
///
/// `count` has no `CHECK (count >= 0)`: two connections racing under
/// `IMMEDIATE` cannot under/overcount (SQLite serialises writers), so a
/// negative count would only mean a bug in this crate's own SQL — a `CHECK`
/// would turn that into an opaque constraint-violation error instead of a
/// wrong-but-diagnosable number; either way it is caught by
/// `store::tests::history_meta_count_always_matches_the_real_row_count`
/// reconciling it against `COUNT(*)` after every kind of write.
///
/// Backfilled from `COUNT(*)` once, for whatever `history` already holds —
/// empty on a fresh v1/v2 file (schema 1/2 never had a `history` table with
/// rows in it at this point in their own history), real counts on a
/// genuine v3 file with history in it.
const V4: &str = "
CREATE TABLE history_meta (
    profile_id TEXT    NOT NULL COLLATE NOCASE PRIMARY KEY
        REFERENCES profile (id) ON DELETE CASCADE,
    count      INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

INSERT INTO history_meta (profile_id, count)
SELECT profile_id, count(*) FROM history GROUP BY profile_id;
";

fn v4(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute_batch(V4)
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
            Migrated {
                from: 0,
                to: SCHEMA_VERSION
            }
        );
        assert_eq!(
            inspect(&connection).expect("inspect"),
            Header::Reldex {
                version: SCHEMA_VERSION
            }
        );
        assert_eq!(
            migrate(&mut connection).expect("migrate"),
            Migrated {
                from: SCHEMA_VERSION,
                to: SCHEMA_VERSION
            }
        );
    }

    #[test]
    fn migrating_from_v1_applies_every_step_in_order_and_creates_the_new_tables() {
        // A genuine schema-1 file (the literal historical DDL), migrated by
        // today's code straight through steps 2 and 3.
        let mut connection = Connection::open_in_memory().expect("memory");
        {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("begin");
            v1(&transaction).expect("v1 DDL");
            transaction
                .pragma_update(None, "application_id", APPLICATION_ID)
                .expect("pragma");
            transaction
                .pragma_update(None, "user_version", 1u32)
                .expect("pragma");
            transaction.commit().expect("commit");
        }
        assert_eq!(
            inspect(&connection).expect("inspect"),
            Header::Reldex { version: 1 }
        );
        assert_eq!(
            migrate(&mut connection).expect("migrate"),
            Migrated {
                from: 1,
                to: SCHEMA_VERSION
            }
        );
        for table in [
            "profile",
            "setting",
            "history",
            "worksheet",
            "worksheet_setting",
            "layout",
            "history_meta",
        ] {
            let exists: i64 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("query");
            assert_eq!(exists, 1, "table {table} missing after migration");
        }
    }

    #[test]
    fn migrating_from_v3_backfills_the_history_counter_from_the_real_row_count() {
        // A genuine schema-3 file (v1, v2, v3's literal historical DDL) with
        // real history rows already in it — the case `V4`'s backfill exists
        // for, as opposed to the v1 case above, where `history` is empty.
        let mut connection = Connection::open_in_memory().expect("memory");
        {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("begin");
            v1(&transaction).expect("v1 DDL");
            v2(&transaction).expect("v2 DDL");
            v3(&transaction).expect("v3 DDL");
            transaction
                .execute(
                    "INSERT INTO profile (id, name, database_type, environment, \
                     treat_as_production, endpoint_kind, auth_kind, role, transport, \
                     allow_unenforced_certificate_pin, created_at, modified_at) \
                     VALUES ('11111111-1111-1111-1111-111111111111', 'p', 'oracle', \
                     'development', 0, 'host_port', 'password', 'ordinary', 'plain', 0, 0, 0)",
                    [],
                )
                .expect("insert profile");
            for n in 0..3 {
                transaction
                    .execute(
                        "INSERT INTO history (profile_id, executed_at, statement, outcome, \
                         elapsed_ms) VALUES ('11111111-1111-1111-1111-111111111111', 0, ?1, \
                         'succeeded', 0)",
                        [format!("select {n}")],
                    )
                    .expect("insert history");
            }
            transaction
                .pragma_update(None, "application_id", APPLICATION_ID)
                .expect("pragma");
            transaction
                .pragma_update(None, "user_version", 3u32)
                .expect("pragma");
            transaction.commit().expect("commit");
        }
        assert_eq!(
            migrate(&mut connection).expect("migrate"),
            Migrated {
                from: 3,
                to: SCHEMA_VERSION
            }
        );
        let count: i64 = connection
            .query_row(
                "SELECT count FROM history_meta WHERE profile_id = \
                 '11111111-1111-1111-1111-111111111111'",
                [],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(count, 3, "backfilled from the real row count, not zero");
    }

    #[test]
    fn a_v3_only_builds_migrate_refuses_a_file_already_at_v4() {
        // The same shape as `an_older_builds_migrate_refuses_a_file_newer_than_it_supports`,
        // one version later: "v3 code" (its own `MIGRATIONS` stops at 3)
        // opening a file a later build already brought to version 4 must
        // refuse it, not silently reinterpret it as version 3.
        let mut connection = Connection::open_in_memory().expect("memory");
        let up_to_v3 = [
            Migration { to: 1, apply: v1 },
            Migration { to: 2, apply: v2 },
            Migration { to: 3, apply: v3 },
        ];
        assert_eq!(
            migrate_with(&mut connection, &up_to_v3, 3).expect("migrate to 3"),
            Migrated { from: 0, to: 3 }
        );
        let up_to_v4 = [
            Migration { to: 1, apply: v1 },
            Migration { to: 2, apply: v2 },
            Migration { to: 3, apply: v3 },
            Migration { to: 4, apply: v4 },
        ];
        assert_eq!(
            migrate_with(&mut connection, &up_to_v4, 4).expect("migrate to 4"),
            Migrated { from: 3, to: 4 }
        );
        // "v3 code" reopening that same file now refuses it.
        assert!(matches!(
            migrate_with(&mut connection, &up_to_v3, 3),
            Err(StoreError::NewerSchema {
                found: 4,
                supported: 3
            })
        ));
    }

    #[test]
    fn an_older_builds_migrate_refuses_a_file_newer_than_it_supports() {
        // Simulates "v1 code": a build whose own `MIGRATIONS` stops at 1,
        // opening a file a later build already brought to version 2. The
        // mechanism is exactly `a_newer_version_is_refused_by_inspect_and_by_migrate`,
        // checked here against an intermediate version rather than only
        // against `SCHEMA_VERSION + 1`, since it is the same rule at every
        // version, not a special case of the current one.
        let mut connection = Connection::open_in_memory().expect("memory");
        let v1_only = [Migration { to: 1, apply: v1 }];
        assert_eq!(
            migrate_with(&mut connection, &v1_only, 1).expect("migrate to 1"),
            Migrated { from: 0, to: 1 }
        );
        let v1_and_v2 = [
            Migration { to: 1, apply: v1 },
            Migration { to: 2, apply: v2 },
        ];
        assert_eq!(
            migrate_with(&mut connection, &v1_and_v2, 2).expect("migrate to 2"),
            Migrated { from: 1, to: 2 }
        );
        // "v1 code" reopening that same file now refuses it.
        assert!(matches!(
            migrate_with(&mut connection, &v1_only, 1),
            Err(StoreError::NewerSchema {
                found: 2,
                supported: 1
            })
        ));
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
