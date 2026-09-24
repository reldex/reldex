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
//! The Oracle driver's `reclassify_ambiguous_permission_error` — the
//! [`reldex_db_driver_api::MetadataErrorClassifier`] every
//! [`PreparedMetadataQuery`] here carries — reclassifies `ORA-00942` and
//! `ORA-01039` as [`ErrorKind::Permission`] when either comes back from a
//! statement this catalog built itself. Every statement this catalog builds
//! queries an `ALL_*` dictionary view, and `SELECT` on those views is granted
//! to `PUBLIC` by Oracle out of the box, so an ordinary test user cannot be
//! made to hit that failure without a second, deliberately under-privileged
//! account this test suite does not have. That path is instead covered by
//! unit tests on the ORA codes in `src/metadata.rs`
//! (`ambiguous_not_visible_codes_are_reclassified_as_permission` and
//! neighbours) — the fallback the M2.8 brief allows when the permission path
//! cannot be provoked safely.
//!
//! Also not verified: whether a materialized view's container table lists
//! under `Tables`. The test account has no `CREATE MATERIALIZED VIEW`
//! privilege, so this could not be confirmed in either direction and is left
//! as a known open item rather than guessed at (see `phase-1.md`'s M2.8 "as
//! implemented" note).

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::collections::HashMap;
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

/// `ColumnsOf` columns that are necessarily *computed* SQL expressions in
/// `OracleMetadataCatalog`'s statement (`position` hides invisible columns,
/// `type_name` composes a display type, `nullable` renders `Y`/`N` as text)
/// rather than a bare column reference. A review-round diagnostic against
/// this Oracle database found that the pinned `oracledb` crate's describe
/// reports `nullable=true` for **any** computed expression, however
/// trivially non-null — even `SELECT 1 FROM DUAL` and `NVL(COLUMN_ID, -1)`
/// describe as nullable — while a genuinely `NOT NULL` *bare* column
/// reference (`ALL_USERS.USERNAME`, `ALL_TAB_COLUMNS.COLUMN_NAME`, …)
/// correctly describes as non-nullable. There is no SQL wording that makes
/// a computed expression describe as non-nullable through this crate, so
/// these three columns' declared non-nullability — a real promise about the
/// *data* those expressions produce, not about this driver's describe of
/// them — cannot be checked this way and is exempted here rather than
/// papered over with a SQL trick that cannot work.
const NULLABLE_UNVERIFIABLE_VIA_DESCRIBE: [&str; 3] = ["position", "type_name", "nullable"];

/// Asserts the executed result's columns match the declared contract's
/// names and logical types, in order, and that nullability's one
/// *checkable* direction holds where a describe can actually answer it —
/// the M2.8 brief's integration-test ask, extended by the review round's
/// must-fix 2.
///
/// `nullable() == Some(false)` is the only firm promise the declared
/// contract makes (`reldex_db_driver_api::metadata`: "when the driver
/// knows"); `Some(true)` is either a real "can be null" fact or, for a field
/// the module documentation calls out as hedged for cross-vendor
/// portability (`schemas_columns`'s `created`), a conservative "no promise
/// either way" that a concrete driver is free to over-deliver on. A live run
/// against this Oracle database found both directions: `ColumnsOf`'s
/// `position` was declared non-nullable while the live describe reported it
/// nullable — but, per [`NULLABLE_UNVERIFIABLE_VIA_DESCRIBE`], that turned
/// out to be a describe-layer limitation for *any* computed expression, not
/// a data-level broken promise. Separately, `Schemas`' hedged `created`
/// (declared nullable, for a vendor that might not track it) described as
/// non-nullable on this concrete database — the safe, over-delivering
/// direction, not a broken promise. Only the
/// non-nullable-declared-but-nullable-in-practice direction is asserted,
/// and only for columns a bare-column describe can actually corroborate.
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
        if !NULLABLE_UNVERIFIABLE_VIA_DESCRIBE.contains(&declared.name()) {
            assert!(
                declared.nullable() != Some(false) || actual.nullable() == Some(false),
                "column `{}` is declared non-nullable but the server's own describe of this \
                 statement reports nullable={:?}",
                declared.name(),
                actual.nullable()
            );
        }
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

/// M2.8 review round, must-fix 1(a): every other `Schemas` test in this file
/// passes a name filter, so the unfiltered path's `LIKE` pattern fallback had
/// never actually executed against the server. This runs it directly.
#[test]
fn schemas_unfiltered_reports_the_current_user_among_every_visible_schema() {
    let mut connection = connect();
    let driver = OracleThinDriver::new();
    let owner = current_owner(connection.as_mut());

    let prepared = driver
        .metadata_catalog()
        .prepare(MetadataRequest::schemas(generous_limit()))
        .expect("schemas is supported");
    let batch = run(connection.as_mut(), &prepared);

    let found = names_column(&batch);
    assert!(
        found.contains(&owner),
        "the current user should appear in an unfiltered listing: {found:?}"
    );
    assert!(
        batch.row_count() > 1,
        "an unfiltered schema listing should see more than just the current user \
         (ALL_USERS lists every account, e.g. SYS/SYSTEM): got {found:?}"
    );
    observation(format!("unfiltered schemas: {} row(s)", batch.row_count()));
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

    // No unquoted Oracle identifier can contain `%` or `\`. The filter
    // combines each character with this test's own unique fixture name
    // rather than searching the whole schema for it bare: cargo runs test
    // functions in parallel by default, and another test in this same file
    // (`quoted_identifiers_with_special_characters_survive_unfiltered_and_
    // filtered_listings`) deliberately creates *quoted* sequences containing
    // these exact characters, so a bare, schema-wide "should match nothing"
    // search would be flaky against that concurrently-running fixture. A
    // search for "this fixture's own name" + the character still catches a
    // real wildcard-treated-as-wildcard bug: if the character were left
    // unescaped, the resulting pattern would accidentally match this very
    // fixture (whose name is a literal prefix of the search text), not just
    // fail to find some unrelated object.
    for literal in ["%", "\\"] {
        let filter = format!("{sequence}{literal}");
        let prepared = driver
            .metadata_catalog()
            .prepare(
                MetadataRequest::objects_of_kind(
                    owner.clone(),
                    MetadataObjectKind::Sequences,
                    generous_limit(),
                )
                .with_name_filter(&filter),
            )
            .expect("objects_of_kind is supported");
        let batch = run(connection.as_mut(), &prepared);
        assert_eq!(
            batch.row_count(),
            0,
            "this fixture's own name has no {literal:?} in it, so appending one should match \
             nothing, got {:?}",
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

/// M2.8 review round, must-fix 1(b): a **quoted** identifier can legally
/// contain `\`, `%` or `_` — unlike the unquoted names
/// `name_filter_treats_wildcard_and_escape_characters_literally` above uses,
/// which can never contain any of them. This is the exact shape that exposed
/// the bug: the earlier `NVL(UPPER(:filter_pattern), UPPER(col))` fallback
/// put a stored name containing `\` straight into the `LIKE` pattern, and
/// `ESCAPE '\'` then raised `ORA-01424` and failed the *entire* unfiltered
/// listing, not just the one row.
#[test]
fn quoted_identifiers_with_special_characters_survive_unfiltered_and_filtered_listings() {
    let mut connection = connect();
    let driver = OracleThinDriver::new();
    let owner = current_owner(connection.as_mut());

    // `unique()`'s own output embeds `_` between its prefix/pid/counter
    // parts, which would make every fixture "contain an underscore" and
    // defeat the exclusivity checks below — strip them and rely on the
    // remaining hex/counter digits for uniqueness instead.
    let base: String = unique("m28q").chars().filter(|c| *c != '_').collect();
    let backslash_name = format!("{base}A\\Z");
    let percent_name = format!("{base}B%Z");
    let underscore_name = format!("{base}C_Z");
    for name in [&backslash_name, &percent_name, &underscore_name] {
        exec(connection.as_mut(), &format!("CREATE SEQUENCE \"{name}\""));
    }

    // Unfiltered: must not raise ORA-01424 just because a stored name
    // contains `\`, and every fixture must be visible.
    let prepared = driver
        .metadata_catalog()
        .prepare(MetadataRequest::objects_of_kind(
            owner.clone(),
            MetadataObjectKind::Sequences,
            generous_limit(),
        ))
        .expect("objects_of_kind is supported");
    let batch = run(connection.as_mut(), &prepared);
    let found = names_column(&batch);
    for name in [&backslash_name, &percent_name, &underscore_name] {
        assert!(
            found.contains(name),
            "an unfiltered listing must not fail, and must still include {name:?}, among {} \
             row(s)",
            batch.row_count()
        );
    }

    // Filtered: a filter naming one special character matches only the
    // fixture that actually contains it — the character is a literal, not a
    // wildcard, even when it comes from a stored name rather than the filter
    // text itself.
    let cases: [(&str, &str, [&str; 2]); 3] = [
        ("\\", &backslash_name, [&percent_name, &underscore_name]),
        ("%", &percent_name, [&backslash_name, &underscore_name]),
        ("_", &underscore_name, [&backslash_name, &percent_name]),
    ];
    for (filter, expected, others) in cases {
        let prepared = driver
            .metadata_catalog()
            .prepare(
                MetadataRequest::objects_of_kind(
                    owner.clone(),
                    MetadataObjectKind::Sequences,
                    generous_limit(),
                )
                .with_name_filter(filter),
            )
            .expect("objects_of_kind is supported");
        let batch = run(connection.as_mut(), &prepared);
        let found = names_column(&batch);
        assert!(
            found.contains(&expected.to_owned()),
            "filtering on {filter:?} should match {expected:?}: found {found:?}"
        );
        for other in others {
            assert!(
                !found.contains(&other.to_owned()),
                "filtering on {filter:?} should not match {other:?}: found {found:?}"
            );
        }
    }

    for name in [&backslash_name, &percent_name, &underscore_name] {
        exec_quietly(connection.as_mut(), &format!("DROP SEQUENCE \"{name}\""));
    }
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

/// M2.8 review round, should-fix 3: `type_name` composes Oracle's own
/// precision/scale/length qualifiers onto bare `DATA_TYPE` for the families
/// where `DATA_TYPE` alone loses information. Every documented family gets
/// its own column here, and the assertion is the exact string, not just a
/// non-empty check.
#[test]
fn columns_of_type_name_matches_oracles_own_precision_scale_and_length_rules() {
    let mut connection = connect();
    let driver = OracleThinDriver::new();
    let owner = current_owner(connection.as_mut());
    let table = unique("m28ty");

    exec(
        connection.as_mut(),
        &format!(
            "CREATE TABLE {table} ( \
             num_ps NUMBER(10,2), \
             num_star_s NUMBER(*,2), \
             num_bare NUMBER, \
             flt FLOAT(24), \
             vc2_char VARCHAR2(10 CHAR), \
             vc2_byte VARCHAR2(10 BYTE), \
             ch CHAR(5), \
             nch NCHAR(5), \
             nvc2 NVARCHAR2(5), \
             rw RAW(8), \
             dt DATE \
             )"
        ),
    );

    let prepared = driver
        .metadata_catalog()
        .prepare(MetadataRequest::columns_of(owner, stored(&table)))
        .expect("columns_of is supported");
    let batch = run(connection.as_mut(), &prepared);

    let mut type_names: HashMap<String, String> = HashMap::new();
    for row in 0..batch.row_count() {
        let name = text_cell(&batch, row, 1).expect("column name");
        let type_name = text_cell(&batch, row, 2).expect("type_name");
        type_names.insert(name, type_name);
    }

    let expected: &[(&str, &str)] = &[
        ("NUM_PS", "NUMBER(10,2)"),
        ("NUM_STAR_S", "NUMBER(*,2)"),
        ("NUM_BARE", "NUMBER"),
        ("FLT", "FLOAT(24)"),
        ("VC2_CHAR", "VARCHAR2(10 CHAR)"),
        ("VC2_BYTE", "VARCHAR2(10 BYTE)"),
        ("CH", "CHAR(5)"),
        ("NCH", "NCHAR(5)"),
        ("NVC2", "NVARCHAR2(5)"),
        ("RW", "RAW(8)"),
        ("DT", "DATE"),
    ];
    for (column, expected_type) in expected {
        assert_eq!(
            type_names.get(*column).map(String::as_str),
            Some(*expected_type),
            "column {column}: got {:?}",
            type_names.get(*column)
        );
    }

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}
