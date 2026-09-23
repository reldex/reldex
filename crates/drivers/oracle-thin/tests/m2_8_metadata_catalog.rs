//! M2.8 — real-database integration tests for `OracleMetadataCatalog`
//! (`SPEC.md` §16; spike S12; `reldex_db_driver_api::metadata`).
//!
//! Run against the local Oracle 19c test database:
//!
//! ```text
//! sh tools/oracle-test-db/run-it.sh m2_8_metadata_catalog
//! ```
//!
//! Gated behind the `oracle-it` feature and using the same connection helper
//! as every other spike in this directory (`AGENTS.md`, "Testing": integration
//! tests that need a real database are kept separate from unit tests).
//!
//! # What this file does not verify
//!
//! `OracleMetadataCatalog::classify_error` reclassifies `ORA-00942` (and two
//! related codes) as [`ErrorKind::Permission`] when it comes back from a
//! statement this catalog built itself. Every statement this catalog builds
//! queries an `ALL_*` dictionary view, and `SELECT` on those views is granted
//! to `PUBLIC` by Oracle out of the box, so an ordinary test user cannot be
//! made to hit that failure without a second, deliberately under-privileged
//! account this test suite does not have. That path is instead covered by
//! unit tests on the ORA code in `src/metadata.rs`
//! (`ambiguous_not_visible_codes_are_reclassified_as_permission` and
//! neighbours) — the fallback the M2.8 brief allows when the permission path
//! cannot be provoked safely.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::num::{NonZeroU32, NonZeroUsize};

use common::{connect, exec, exec_quietly, observation, scalar, unique};
use reldex_db_driver_api::{
    ColumnMetadata, DatabaseConnection, DatabaseDriver, MetadataObjectKind, MetadataRequest,
    PreparedMetadataQuery, RowBatch, ValueRef,
};
use reldex_driver_oracle_thin::OracleThinDriver;

/// The name as the dictionary stores it: an unquoted identifier is folded to
/// upper case by the server, and every dictionary predicate has to ask for
/// the stored form (mirrors `s12_dev_features.rs`'s `stored`).
fn stored(name: &str) -> String {
    name.to_uppercase()
}

fn current_owner(connection: &mut dyn DatabaseConnection) -> String {
    scalar(connection, "SELECT USER FROM dual")
}

/// Asserts the executed result's columns match the declared contract's names
/// and logical types, in order — exactly what the M2.8 brief's integration
/// tests ask for. Nullability is not compared: the declared contract is a
/// cross-vendor promise (see `reldex_db_driver_api::metadata`), not a claim
/// about this specific describe.
fn assert_contract_matches(actual: &[ColumnMetadata], declared: &[ColumnMetadata]) {
    assert_eq!(
        actual.len(),
        declared.len(),
        "column count differs from the declared contract"
    );
    for (actual, declared) in actual.iter().zip(declared) {
        assert_eq!(actual.name(), declared.name(), "column name");
        assert_eq!(
            actual.sql_type(),
            declared.sql_type(),
            "column `{}`'s logical type",
            declared.name()
        );
    }
}

/// Executes a prepared metadata statement through the ordinary path, checks
/// its result against the declared contract, and returns every row.
fn run(connection: &mut dyn DatabaseConnection, prepared: &PreparedMetadataQuery) -> RowBatch {
    let mut outcome = connection
        .execute(prepared.statement())
        .expect("a metadata statement should execute");
    let mut cursor = outcome
        .take_cursor()
        .expect("every metadata request returns a cursor");
    assert_contract_matches(cursor.columns(), prepared.columns());
    let batch = cursor
        .fetch_batch(NonZeroUsize::new(10_000).expect("non-zero"))
        .expect("fetch");
    cursor.close().expect("closing a cursor cannot fail");
    batch
}

fn text_cell(batch: &RowBatch, row: usize, column: usize) -> Option<String> {
    match batch.value(row, column) {
        Some(ValueRef::Text(text)) => Some(text.to_owned()),
        _ => None,
    }
}

fn names_column(batch: &RowBatch) -> Vec<String> {
    (0..batch.row_count())
        .map(|row| text_cell(batch, row, 0).unwrap_or_default())
        .collect()
}

fn generous_limit() -> NonZeroU32 {
    NonZeroU32::new(10_000).expect("non-zero")
}

#[test]
fn schemas_reports_the_current_user_when_filtered_to_it() {
    let mut connection = connect();
    let driver = OracleThinDriver::new();
    let owner = current_owner(connection.as_mut());

    let prepared = driver
        .metadata_catalog()
        .prepare(MetadataRequest::schemas(generous_limit()).with_name_filter(&owner))
        .expect("schemas is supported");
    let batch = run(connection.as_mut(), &prepared);

    let found = names_column(&batch);
    assert!(
        found.contains(&owner),
        "the current user should appear when the filter names it: {found:?}"
    );
    observation(format!(
        "schemas filtered to {owner}: {} row(s)",
        batch.row_count()
    ));
    connection.close().expect("close");
}

#[test]
fn every_object_group_reports_a_freshly_created_fixture_with_the_declared_contract() {
    let mut connection = connect();
    let driver = OracleThinDriver::new();
    let owner = current_owner(connection.as_mut());

    let table = unique("m28tbl");
    let view = unique("m28view");
    let package = unique("m28pkg");
    let procedure = unique("m28proc");
    let function = unique("m28func");
    let trigger = unique("m28trg");
    let sequence = unique("m28seq");
    let synonym = unique("m28syn");

    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(9) PRIMARY KEY, note VARCHAR2(80))"),
    );
    exec(
        connection.as_mut(),
        &format!("CREATE VIEW {view} AS SELECT id, note FROM {table}"),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE PACKAGE {package} AS FUNCTION doubled(n NUMBER) RETURN NUMBER; \
             END {package};"
        ),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE PACKAGE BODY {package} AS FUNCTION doubled(n NUMBER) RETURN NUMBER IS \
             BEGIN RETURN n * 2; END; END {package};"
        ),
    );
    exec(
        connection.as_mut(),
        &format!("CREATE PROCEDURE {procedure} (n NUMBER) AS BEGIN NULL; END {procedure};"),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE FUNCTION {function} (n NUMBER) RETURN NUMBER AS BEGIN RETURN n; \
             END {function};"
        ),
    );
    // No `:NEW`/`:OLD` in the body, so this reaches `oracledb` exactly as
    // written — no bind placeholder for its scan to trip over, and so no need
    // for the U-18 `EXECUTE IMMEDIATE` rewrite path
    // (`s12b_trigger_rewrite.rs`).
    exec(
        connection.as_mut(),
        &format!("CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW BEGIN NULL; END;"),
    );
    exec(connection.as_mut(), &format!("CREATE SEQUENCE {sequence}"));
    exec(
        connection.as_mut(),
        &format!("CREATE SYNONYM {synonym} FOR {table}"),
    );

    let cases: [(MetadataObjectKind, &str); 9] = [
        (MetadataObjectKind::Tables, &table),
        (MetadataObjectKind::Views, &view),
        (MetadataObjectKind::Packages, &package),
        (MetadataObjectKind::PackageBodies, &package),
        (MetadataObjectKind::Procedures, &procedure),
        (MetadataObjectKind::Functions, &function),
        (MetadataObjectKind::Triggers, &trigger),
        (MetadataObjectKind::Sequences, &sequence),
        (MetadataObjectKind::Synonyms, &synonym),
    ];

    for (kind, created_name) in cases {
        let expected = stored(created_name);
        let prepared = driver
            .metadata_catalog()
            .prepare(MetadataRequest::objects_of_kind(
                owner.clone(),
                kind,
                generous_limit(),
            ))
            .unwrap_or_else(|error| panic!("{kind:?} should be supported: {error}"));
        let batch = run(connection.as_mut(), &prepared);
        let found = names_column(&batch);
        assert!(
            found.contains(&expected),
            "{kind:?} did not report {expected} among {} row(s)",
            batch.row_count()
        );
    }

    let prepared = driver
        .metadata_catalog()
        .prepare(MetadataRequest::columns_of(owner, stored(&table)))
        .expect("columns_of is supported");
    let batch = run(connection.as_mut(), &prepared);
    assert_eq!(
        batch.row_count(),
        2,
        "the fixture table has exactly two columns"
    );
    assert_eq!(text_cell(&batch, 0, 1), Some("ID".to_owned()));
    assert_eq!(
        text_cell(&batch, 0, 3),
        Some("NO".to_owned()),
        "ID is the primary key, so it is not nullable"
    );
    assert_eq!(text_cell(&batch, 1, 1), Some("NOTE".to_owned()));
    assert_eq!(
        text_cell(&batch, 1, 3),
        Some("YES".to_owned()),
        "NOTE has no NOT NULL constraint"
    );

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    exec_quietly(connection.as_mut(), &format!("DROP SYNONYM {synonym}"));
    exec_quietly(connection.as_mut(), &format!("DROP SEQUENCE {sequence}"));
    exec_quietly(connection.as_mut(), &format!("DROP FUNCTION {function}"));
    exec_quietly(connection.as_mut(), &format!("DROP PROCEDURE {procedure}"));
    exec_quietly(connection.as_mut(), &format!("DROP PACKAGE {package}"));
    exec_quietly(connection.as_mut(), &format!("DROP VIEW {view}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn name_filter_treats_wildcard_and_escape_characters_literally() {
    let mut connection = connect();
    let driver = OracleThinDriver::new();
    let owner = current_owner(connection.as_mut());
    let sequence = unique("m28lit");
    exec(connection.as_mut(), &format!("CREATE SEQUENCE {sequence}"));

    // No Oracle identifier can contain `%`, `_`'s special meaning is a LIKE
    // wildcard rather than a character Oracle forbids, and `\` cannot appear
    // in an unquoted identifier either. If any of these were treated as a
    // wildcard instead of a literal character, at least one of them would
    // match every object of the kind instead of none.
    for literal in ["%", "\\"] {
        let prepared = driver
            .metadata_catalog()
            .prepare(
                MetadataRequest::objects_of_kind(
                    owner.clone(),
                    MetadataObjectKind::Sequences,
                    generous_limit(),
                )
                .with_name_filter(literal),
            )
            .expect("objects_of_kind is supported");
        let batch = run(connection.as_mut(), &prepared);
        assert_eq!(
            batch.row_count(),
            0,
            "a literal {literal:?} should match no Oracle identifier, got {:?}",
            names_column(&batch)
        );
    }

    // A real substring of the fixture's own name still matches normally.
    let stored_name = stored(&sequence);
    let substring = &stored_name[0..stored_name.len().min(6)];
    let prepared = driver
        .metadata_catalog()
        .prepare(
            MetadataRequest::objects_of_kind(
                owner,
                MetadataObjectKind::Sequences,
                generous_limit(),
            )
            .with_name_filter(substring),
        )
        .expect("objects_of_kind is supported");
    let batch = run(connection.as_mut(), &prepared);
    assert!(
        names_column(&batch).contains(&stored_name),
        "an ordinary substring filter should still match: {:?}",
        names_column(&batch)
    );

    exec_quietly(connection.as_mut(), &format!("DROP SEQUENCE {sequence}"));
    connection.close().expect("close");
}

#[test]
fn limit_truncation_is_signalled_by_exactly_one_extra_row() {
    let mut connection = connect();
    let driver = OracleThinDriver::new();
    let owner = current_owner(connection.as_mut());
    let prefix = stored(&unique("m28trn"));
    let created: Vec<String> = (0..3).map(|i| format!("{prefix}_{i}")).collect();
    for name in &created {
        exec(connection.as_mut(), &format!("CREATE SEQUENCE {name}"));
    }

    let small_limit = NonZeroU32::new(2).expect("non-zero");
    let prepared = driver
        .metadata_catalog()
        .prepare(
            MetadataRequest::objects_of_kind(
                owner.clone(),
                MetadataObjectKind::Sequences,
                small_limit,
            )
            .with_name_filter(&prefix),
        )
        .expect("objects_of_kind is supported");
    let batch = run(connection.as_mut(), &prepared);
    assert_eq!(
        u32::try_from(batch.row_count()).expect("row count fits in u32"),
        small_limit.get() + 1,
        "3 objects exist but limit={}: the statement must return exactly limit+1 rows to \
         signal truncation",
        small_limit.get()
    );

    let large_limit = NonZeroU32::new(1000).expect("non-zero");
    let prepared = driver
        .metadata_catalog()
        .prepare(
            MetadataRequest::objects_of_kind(owner, MetadataObjectKind::Sequences, large_limit)
                .with_name_filter(&prefix),
        )
        .expect("objects_of_kind is supported");
    let batch = run(connection.as_mut(), &prepared);
    assert_eq!(
        batch.row_count(),
        3,
        "with a limit above the true count, nothing should be truncated"
    );

    for name in &created {
        exec_quietly(connection.as_mut(), &format!("DROP SEQUENCE {name}"));
    }
    connection.close().expect("close");
}

#[test]
fn a_schema_with_no_matching_objects_reports_zero_rows_not_an_error() {
    let mut connection = connect();
    let driver = OracleThinDriver::new();
    let nonexistent_schema = stored(&unique("m28nosuchschema"));

    let prepared = driver
        .metadata_catalog()
        .prepare(MetadataRequest::objects_of_kind(
            nonexistent_schema,
            MetadataObjectKind::Tables,
            NonZeroU32::new(10).expect("non-zero"),
        ))
        .expect("objects_of_kind is supported");
    let batch = run(connection.as_mut(), &prepared);
    assert_eq!(batch.row_count(), 0);
    connection.close().expect("close");
}
