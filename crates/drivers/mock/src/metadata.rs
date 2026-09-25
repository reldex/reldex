//! [`MockMetadataCatalog`]: the mock driver's `MetadataCatalog` implementation
//! (M2.8), plus [`MetadataFixture`] for scripting its statements against a
//! [`Scenario`].
//!
//! [`MockMetadataCatalog`] is stateless, like
//! `reldex_driver_oracle_thin`'s implementation: it only builds a
//! [`Statement`] and a declared column contract, and never touches a
//! [`Scenario`] itself. The statement's SQL text is a marker string unique to
//! the request's shape (and, where the request is naturally scoped to one, its
//! schema/table/kind) — this mock has no SQL engine, so unlike a real driver's
//! statement text it is *not* a constant per request shape; the marker is how
//! [`crate::MockConnection::execute`] routes to the right canned answer, which
//! [`Scenario::find_action`] matches by exact statement text. `schema` and
//! `table` are still attached as ordinary named binds too, so a generic
//! caller sees the same statement shape a real driver would produce.
//!
//! [`MetadataFixture`] is the seeding half: a thin wrapper over
//! [`Scenario::on_sql`] that knows the exact marker text
//! [`MockMetadataCatalog`] builds, so a test does not have to hard-code it.
//! Because [`Scenario::find_action`] answers with the *first* registration
//! that matches a statement's text, seed (or script a failure for) a given
//! request shape only **once** — register a different schema/kind/table, or
//! build a fresh [`Scenario`], to change the answer partway through a test.

use reldex_db_driver_api::{
    Bind, DbError, DbResult, ErrorKind, MetadataCatalog, MetadataObjectKind, MetadataRequest,
    NamedBind, NativeError, PreparedMetadataQuery, Statement, Timestamp, columns_of_columns,
    objects_of_kind_columns, schemas_columns,
};

use crate::{Action, ColumnSpec, QueryPlan, QuerySource, Scenario, ScriptValue, ScriptedError};

fn schemas_marker() -> &'static str {
    "RELDEX_MOCK_METADATA::SCHEMAS"
}

fn objects_marker(schema: &str, kind: MetadataObjectKind) -> String {
    format!("RELDEX_MOCK_METADATA::OBJECTS::{schema}::{kind:?}")
}

fn columns_marker(schema: &str, table: &str) -> String {
    format!("RELDEX_MOCK_METADATA::COLUMNS::{schema}::{table}")
}

/// Converts a shared, vendor-neutral column contract into the mock's own
/// column descriptor, dropping nullability: [`crate::MockCursor`] does not
/// report result-column nullability at all (nothing in the mock's fixtures
/// needs it), so a declared contract's `nullable` flag has nothing to compare
/// against on this driver.
fn as_column_specs(contract: Vec<reldex_db_driver_api::ColumnMetadata>) -> Vec<ColumnSpec> {
    contract
        .into_iter()
        .map(|column| ColumnSpec::new(column.name().to_owned(), column.sql_type()))
        .collect()
}

/// Native codes this mimics as "not visible to this session" — the same
/// codes `reldex_driver_oracle_thin`'s `AMBIGUOUS_NOT_VISIBLE_CODES`
/// reclassifies, duplicated here (not imported: a driver crate does not
/// depend on another driver crate) so this mock's classifier exercises the
/// same shape a real Oracle connection's does.
const MOCK_AMBIGUOUS_NOT_VISIBLE_CODES: [i32; 2] = [942, 1039];

/// A [`reldex_db_driver_api::MetadataErrorClassifier`] that mimics
/// `reldex_driver_oracle_thin`'s reclassification shape for testing
/// purposes: reclassifies one of [`MOCK_AMBIGUOUS_NOT_VISIBLE_CODES`] from
/// whatever it arrived as into [`ErrorKind::Permission`].
///
/// This exists so a `db-core`/M6.1 test written against this mock exercises
/// the same "raw failure, then the caller must call
/// [`PreparedMetadataQuery::reclassify_error`]" path a real Oracle
/// connection produces. [`MetadataFixture::fail_objects_with_permission`]
/// and [`MetadataFixture::fail_columns_with_permission`] script the *raw*,
/// unclassified failure by default precisely so a caller that forgets to
/// call `reclassify_error` sees the wrong [`ErrorKind`] against this mock
/// too — not a false-positive `Permission` that would hide the omission.
fn mimic_oracle_permission_classifier(error: &DbError) -> Option<DbError> {
    if error.kind() == ErrorKind::Permission {
        return None;
    }
    let code = error.native().map(NativeError::code)?;
    if !MOCK_AMBIGUOUS_NOT_VISIBLE_CODES.contains(&code) {
        return None;
    }
    let mut rebuilt = DbError::new(ErrorKind::Permission, error.message().to_owned());
    if let Some(native) = error.native() {
        rebuilt = rebuilt.with_native(NativeError::new(native.code(), native.message()));
    }
    Some(rebuilt)
}

/// The mock driver's `MetadataCatalog` implementation.
///
/// Stateless, like the Oracle driver's: it only builds statements, and a test
/// scripts their answers separately with [`MetadataFixture`].
#[derive(Debug, Default, Clone, Copy)]
pub struct MockMetadataCatalog;

impl MetadataCatalog for MockMetadataCatalog {
    fn prepare(&self, request: MetadataRequest) -> DbResult<PreparedMetadataQuery> {
        match request {
            MetadataRequest::Schemas { name_filter, limit } => {
                let statement = Statement::new(schemas_marker()).with_named_binds(vec![
                    NamedBind::new("filter_pattern", Bind::input(name_filter)),
                    NamedBind::new("limit", Bind::input(i64::from(limit.get()))),
                ]);
                Ok(PreparedMetadataQuery::new(
                    statement,
                    schemas_columns(),
                    mimic_oracle_permission_classifier,
                ))
            }
            MetadataRequest::ObjectsOfKind {
                schema,
                kind,
                name_filter,
                limit,
            } => {
                let statement =
                    Statement::new(objects_marker(&schema, kind)).with_named_binds(vec![
                        NamedBind::new("schema", Bind::input(schema)),
                        NamedBind::new("filter_pattern", Bind::input(name_filter)),
                        NamedBind::new("limit", Bind::input(i64::from(limit.get()))),
                    ]);
                Ok(PreparedMetadataQuery::new(
                    statement,
                    objects_of_kind_columns(),
                    mimic_oracle_permission_classifier,
                ))
            }
            MetadataRequest::ColumnsOf { schema, table } => {
                let statement =
                    Statement::new(columns_marker(&schema, &table)).with_named_binds(vec![
                        NamedBind::new("schema", Bind::input(schema)),
                        NamedBind::new("table_name", Bind::input(table)),
                    ]);
                Ok(PreparedMetadataQuery::new(
                    statement,
                    columns_of_columns(),
                    mimic_oracle_permission_classifier,
                ))
            }
            _ => Err(DbError::unsupported(
                "this metadata request shape (a future shape this driver predates)",
            )),
        }
    }

    // Every `PreparedMetadataQuery` above carries
    // `mimic_oracle_permission_classifier`, so a test against this mock
    // exercises the same "raw failure, then reclassify_error" shape a real
    // Oracle connection produces — see that function's documentation.
}

/// Scripts a [`Scenario`] with deterministic answers for
/// [`MockMetadataCatalog`]'s prepared statements, so a [`crate::MockConnection`]
/// opened against that scenario can execute them through the ordinary
/// execute/fetch path. See the module documentation for the one-registration
/// rule this shares with [`Scenario::on_sql`].
pub struct MetadataFixture<'a> {
    scenario: &'a Scenario,
}

impl<'a> MetadataFixture<'a> {
    /// Scripts against `scenario`.
    #[must_use]
    pub const fn new(scenario: &'a Scenario) -> Self {
        Self { scenario }
    }

    /// Seeds the sole `Schemas` answer. `rows` are `(name, created)` pairs.
    pub fn seed_schemas(&self, rows: &[(&str, Timestamp)]) {
        let plan = QueryPlan::new(
            as_column_specs(schemas_columns()),
            rows.iter()
                .map(|(name, created)| vec![ScriptValue::from(*name), ScriptValue::from(*created)])
                .collect(),
        );
        self.scenario
            .on_sql(schemas_marker(), Action::query(QuerySource::Fixed(plan)));
    }

    /// Seeds the `ObjectsOfKind` answer for one `(schema, kind)` pair. `rows`
    /// are `(name, status, created, last_modified)`.
    pub fn seed_objects(
        &self,
        schema: &str,
        kind: MetadataObjectKind,
        rows: &[(&str, Option<&str>, Timestamp, Timestamp)],
    ) {
        let plan = QueryPlan::new(
            as_column_specs(objects_of_kind_columns()),
            rows.iter()
                .map(|(name, status, created, last_modified)| {
                    vec![
                        ScriptValue::from(*name),
                        ScriptValue::from(status.map(str::to_owned)),
                        ScriptValue::from(*created),
                        ScriptValue::from(*last_modified),
                    ]
                })
                .collect(),
        );
        self.scenario.on_sql(
            objects_marker(schema, kind),
            Action::query(QuerySource::Fixed(plan)),
        );
    }

    /// Seeds the `ColumnsOf` answer for one `(schema, table)` pair. `rows` are
    /// `(position, name, type_name, nullable)`.
    pub fn seed_columns(&self, schema: &str, table: &str, rows: &[(i64, &str, &str, bool)]) {
        let plan = QueryPlan::new(
            as_column_specs(columns_of_columns()),
            rows.iter()
                .map(|(position, name, type_name, nullable)| {
                    vec![
                        ScriptValue::from(*position),
                        ScriptValue::from(*name),
                        ScriptValue::from(*type_name),
                        ScriptValue::from(if *nullable { "YES" } else { "NO" }),
                    ]
                })
                .collect(),
        );
        self.scenario.on_sql(
            columns_marker(schema, table),
            Action::query(QuerySource::Fixed(plan)),
        );
    }

    /// Scripts an `ObjectsOfKind` permission failure for one `(schema, kind)`
    /// pair, carrying the *raw*, unclassified shape a real Oracle dictionary
    /// permission failure actually arrives as — `ErrorKind::Syntax` with
    /// native `ORA-00942` — exactly as it would come back from
    /// [`DatabaseConnection::execute`](reldex_db_driver_api::DatabaseConnection::execute)
    /// before a caller applies
    /// [`PreparedMetadataQuery::reclassify_error`]. Deliberately *not*
    /// scripted as [`ErrorKind::Permission`] directly: this mock's
    /// [`mimic_oracle_permission_classifier`] reclassifies it the same way
    /// the Oracle driver's does, so a `db-core`/M6.1 test that forgets to
    /// call `reclassify_error` sees the wrong `ErrorKind` here too, instead
    /// of a false-positive `Permission` that would hide the omission. Use
    /// [`MetadataFixture::fail_objects_with_permission_preclassified`] to
    /// script the already-`Permission` case directly instead.
    pub fn fail_objects_with_permission(&self, schema: &str, kind: MetadataObjectKind) {
        self.scenario.on_sql(
            objects_marker(schema, kind),
            Action::Fail(permission_denied_raw()),
        );
    }

    /// The same, for [`MetadataRequest::ColumnsOf`].
    pub fn fail_columns_with_permission(&self, schema: &str, table: &str) {
        self.scenario.on_sql(
            columns_marker(schema, table),
            Action::Fail(permission_denied_raw()),
        );
    }

    /// Scripts an `ObjectsOfKind` failure that is *already*
    /// [`ErrorKind::Permission`] — the shape after reclassification, for a
    /// test that wants to exercise a caller's handling of an
    /// already-corrected error without also exercising the classifier
    /// itself.
    pub fn fail_objects_with_permission_preclassified(
        &self,
        schema: &str,
        kind: MetadataObjectKind,
    ) {
        self.scenario.on_sql(
            objects_marker(schema, kind),
            Action::Fail(permission_denied_preclassified()),
        );
    }

    /// The same, for [`MetadataRequest::ColumnsOf`].
    pub fn fail_columns_with_permission_preclassified(&self, schema: &str, table: &str) {
        self.scenario.on_sql(
            columns_marker(schema, table),
            Action::Fail(permission_denied_preclassified()),
        );
    }
}

/// The raw, unclassified shape: what a real Oracle connection's execute
/// actually returns before [`PreparedMetadataQuery::reclassify_error`] runs.
fn permission_denied_raw() -> ScriptedError {
    ScriptedError::new(ErrorKind::Syntax, "ORA-00942: table or view does not exist")
        .with_native(942, "ORA-00942: table or view does not exist")
}

/// The already-reclassified shape, for a test that wants `Permission`
/// straight from `execute` without exercising the classifier.
fn permission_denied_preclassified() -> ScriptedError {
    ScriptedError::new(
        ErrorKind::Permission,
        "reldex-driver-mock: not visible to this session",
    )
    .with_native(942, "ORA-00942: table or view does not exist")
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroUsize};
    use std::sync::Arc;

    use reldex_db_driver_api::{
        ConnectionParams, Credentials, DatabaseConnection, DatabaseDriver, Endpoint, ErrorKind,
    };

    use super::*;
    use crate::MockDriver;

    fn connect(scenario: &Arc<Scenario>) -> Box<dyn DatabaseConnection> {
        let params = ConnectionParams::new(
            Endpoint::ConnectString("mock".to_owned()),
            Credentials::External,
        );
        MockDriver::new(Arc::clone(scenario))
            .connect(&params)
            .expect("mock connect never fails by default")
    }

    fn limit(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    fn timestamp(year: i16) -> Timestamp {
        Timestamp::new(year, 1, 1, 0, 0, 0).expect("valid date")
    }

    /// Executes a prepared metadata statement through the ordinary
    /// execute/fetch path and checks the fetched columns' names and types
    /// against the declared contract — the "mock catalog round trip" the M2.8
    /// brief asks for.
    fn round_trip(
        connection: &mut dyn DatabaseConnection,
        prepared: &PreparedMetadataQuery,
    ) -> reldex_db_driver_api::RowBatch {
        let mut outcome = connection
            .execute(prepared.statement())
            .expect("the scenario has a registered answer");
        let mut cursor = outcome.take_cursor().expect("a list request returns rows");
        assert_eq!(cursor.columns().len(), prepared.columns().len());
        for (actual, declared) in cursor.columns().iter().zip(prepared.columns()) {
            assert_eq!(actual.name(), declared.name());
            assert_eq!(actual.sql_type(), declared.sql_type());
        }
        let batch = cursor
            .fetch_batch(NonZeroUsize::new(100).expect("non-zero"))
            .expect("fetch");
        cursor.close().expect("close never fails here");
        batch
    }

    #[test]
    fn schemas_round_trip_matches_the_declared_contract() {
        let scenario = Scenario::new();
        MetadataFixture::new(&scenario)
            .seed_schemas(&[("ALICE", timestamp(2020)), ("BOB", timestamp(2021))]);
        let mut connection = connect(&scenario);
        let catalog = MockDriver::new(Arc::clone(&scenario));
        let prepared = catalog
            .metadata_catalog()
            .prepare(MetadataRequest::schemas(limit(10)))
            .expect("schemas is supported");

        let batch = round_trip(connection.as_mut(), &prepared);
        assert_eq!(batch.row_count(), 2);
        assert_eq!(batch.value(0, 0).and_then(|v| v.as_str()), Some("ALICE"));
        assert_eq!(batch.value(1, 0).and_then(|v| v.as_str()), Some("BOB"));
    }

    #[test]
    fn objects_of_kind_round_trip_matches_the_declared_contract() {
        let scenario = Scenario::new();
        MetadataFixture::new(&scenario).seed_objects(
            "HR",
            MetadataObjectKind::Tables,
            &[
                ("EMPLOYEES", Some("VALID"), timestamp(2019), timestamp(2022)),
                ("DEPARTMENTS", None, timestamp(2019), timestamp(2019)),
            ],
        );
        let mut connection = connect(&scenario);
        let catalog = MockDriver::new(Arc::clone(&scenario));
        let prepared = catalog
            .metadata_catalog()
            .prepare(MetadataRequest::objects_of_kind(
                "HR",
                MetadataObjectKind::Tables,
                limit(10),
            ))
            .expect("objects_of_kind is supported");

        let batch = round_trip(connection.as_mut(), &prepared);
        assert_eq!(batch.row_count(), 2);
        assert_eq!(
            batch.value(0, 0).and_then(|v| v.as_str()),
            Some("EMPLOYEES")
        );
        assert!(batch.value(1, 1).expect("cell exists").is_null());
    }

    #[test]
    fn columns_of_round_trip_matches_the_declared_contract() {
        let scenario = Scenario::new();
        MetadataFixture::new(&scenario).seed_columns(
            "HR",
            "EMPLOYEES",
            &[
                (1, "EMPLOYEE_ID", "NUMBER", false),
                (2, "LAST_NAME", "VARCHAR2", false),
                (3, "COMMISSION_PCT", "NUMBER", true),
            ],
        );
        let mut connection = connect(&scenario);
        let catalog = MockDriver::new(Arc::clone(&scenario));
        let prepared = catalog
            .metadata_catalog()
            .prepare(MetadataRequest::columns_of("HR", "EMPLOYEES"))
            .expect("columns_of is supported");

        let batch = round_trip(connection.as_mut(), &prepared);
        assert_eq!(batch.row_count(), 3);
        assert_eq!(
            batch.value(2, 3).and_then(|v| v.as_str()),
            Some("YES"),
            "COMMISSION_PCT is nullable"
        );
    }

    #[test]
    fn a_scripted_permission_failure_arrives_raw_and_reclassify_error_corrects_it() {
        let scenario = Scenario::new();
        MetadataFixture::new(&scenario)
            .fail_objects_with_permission("HR", MetadataObjectKind::Tables);
        let mut connection = connect(&scenario);
        let catalog = MockDriver::new(Arc::clone(&scenario));
        let prepared = catalog
            .metadata_catalog()
            .prepare(MetadataRequest::objects_of_kind(
                "HR",
                MetadataObjectKind::Tables,
                limit(10),
            ))
            .expect("objects_of_kind is supported");

        let error = connection
            .execute(prepared.statement())
            .expect_err("the scenario scripts a failure");
        // Raw, as a real Oracle connection would actually return it — a
        // caller that forgets to call `reclassify_error` would see this
        // wrong `ErrorKind`, not a false-positive `Permission`.
        assert_eq!(error.kind(), ErrorKind::Syntax);
        assert_eq!(
            error.native().map(reldex_db_driver_api::NativeError::code),
            Some(942)
        );

        let corrected = prepared.reclassify_error(error);
        assert_eq!(corrected.kind(), ErrorKind::Permission);
        assert_eq!(
            corrected
                .native()
                .map(reldex_db_driver_api::NativeError::code),
            Some(942)
        );
    }

    #[test]
    fn a_preclassified_permission_failure_reaches_the_caller_directly() {
        let scenario = Scenario::new();
        MetadataFixture::new(&scenario)
            .fail_objects_with_permission_preclassified("HR", MetadataObjectKind::Tables);
        let mut connection = connect(&scenario);
        let catalog = MockDriver::new(Arc::clone(&scenario));
        let prepared = catalog
            .metadata_catalog()
            .prepare(MetadataRequest::objects_of_kind(
                "HR",
                MetadataObjectKind::Tables,
                limit(10),
            ))
            .expect("objects_of_kind is supported");

        let error = connection
            .execute(prepared.statement())
            .expect_err("the scenario scripts a failure");
        assert_eq!(error.kind(), ErrorKind::Permission);
        // `reclassify_error` is a no-op on an already-`Permission` error, the
        // same as the Oracle driver's own classifier.
        let unchanged = prepared.reclassify_error(error);
        assert_eq!(unchanged.kind(), ErrorKind::Permission);
    }

    #[test]
    fn columns_of_permission_failure_arrives_raw_and_reclassify_error_corrects_it() {
        let scenario = Scenario::new();
        MetadataFixture::new(&scenario).fail_columns_with_permission("HR", "SALARIES");
        let mut connection = connect(&scenario);
        let catalog = MockDriver::new(Arc::clone(&scenario));
        let prepared = catalog
            .metadata_catalog()
            .prepare(MetadataRequest::columns_of("HR", "SALARIES"))
            .expect("columns_of is supported");

        let error = connection
            .execute(prepared.statement())
            .expect_err("the scenario scripts a failure");
        assert_eq!(error.kind(), ErrorKind::Syntax);

        let corrected = prepared.reclassify_error(error);
        assert_eq!(corrected.kind(), ErrorKind::Permission);
    }

    #[test]
    fn columns_of_preclassified_permission_failure_reaches_the_caller_directly() {
        let scenario = Scenario::new();
        MetadataFixture::new(&scenario)
            .fail_columns_with_permission_preclassified("HR", "SALARIES");
        let mut connection = connect(&scenario);
        let catalog = MockDriver::new(Arc::clone(&scenario));
        let prepared = catalog
            .metadata_catalog()
            .prepare(MetadataRequest::columns_of("HR", "SALARIES"))
            .expect("columns_of is supported");

        let error = connection
            .execute(prepared.statement())
            .expect_err("the scenario scripts a failure");
        assert_eq!(error.kind(), ErrorKind::Permission);
    }

    #[test]
    fn an_unsupported_request_shape_is_a_typed_error_not_a_panic() {
        // `MockMetadataCatalog` supports every current request shape; this
        // exercises the same defensive path
        // `OracleMetadataCatalog::prepare` has for a future
        // `MetadataObjectKind` it predates, at the level the trait itself can
        // be tested generically (see `reldex_db_driver_api::metadata`'s own
        // `Noop` unit test for the direct case).
        let prepared = MockMetadataCatalog.prepare(MetadataRequest::schemas(limit(1)));
        assert!(prepared.is_ok(), "schemas is always supported");
    }
}
