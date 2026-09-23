//! Oracle dictionary SQL for the vendor-neutral metadata catalog (M2.8;
//! `SPEC.md` §16; `reldex_db_driver_api::metadata` for the whole design).
//!
//! Every query here reads `ALL_*` dictionary views: `ALL_USERS` for schemas,
//! `ALL_OBJECTS` (joined to `ALL_TABLES` for the `Tables` group, to exclude
//! nested-table storage, index-organized-table overflow segments and
//! secondary objects) for the other eight object groups, and
//! `ALL_TAB_COLUMNS` for a table's columns. `ALL_*` views already scope
//! themselves to what the current session can see, which is why a permission
//! failure against one of them is unusual rather than routine — see
//! [`OracleMetadataCatalog::classify_error`].
//!
//! # Why the SQL text never changes per call
//!
//! Every statement built here is either a `const` string or built once from a
//! closed, driver-chosen set of literals (the nine `OBJECT_TYPE` values);
//! `schema`, `table`, the name filter and the limit are **always** bind
//! values, never interpolated. `tests::the_sql_text_is_a_constant_per_kind`
//! checks this directly by preparing the same request shape with different
//! schema/filter/limit values and asserting the SQL text does not change.
//!
//! # Avoiding a second occurrence of the name-filter bind
//!
//! The obvious `(:filter_pattern IS NULL OR UPPER(col) LIKE UPPER(:filter_pattern) ESCAPE '\')`
//! shape binds `:filter_pattern` twice in one statement. Oracle's named-bind
//! API in the pinned `oracledb` version takes name→value pairs built from
//! *this driver's* declared named binds (see `crate::conn::prepare_binds`),
//! and nothing in this codebase had yet exercised a name used more than once
//! in one statement's text. Rather than depend on unverified behaviour, the
//! filter uses `UPPER(col) LIKE NVL(UPPER(:filter_pattern), UPPER(col))
//! ESCAPE '\'`: `:filter_pattern` appears exactly once, and when it is NULL —
//! no filter — the pattern degenerates to `UPPER(col) LIKE UPPER(col)`, which
//! is always true for any non-NULL `col`, exactly the "no filter" behaviour
//! wanted. Every column filtered this way (`OBJECT_NAME`, `USERNAME`) is
//! `NOT NULL` in the dictionary.

use reldex_db_driver_api::{
    Bind, DbError, DbResult, ErrorKind, MetadataCatalog, MetadataObjectKind, MetadataRequest,
    NamedBind, NativeError, PreparedMetadataQuery, Statement, columns_of_columns,
    name_filter_pattern, objects_of_kind_columns, schemas_columns,
};

/// `ALL_USERS` scoped and shaped for [`MetadataRequest::Schemas`].
const SCHEMAS_SQL: &str = "SELECT USERNAME AS \"name\", CREATED AS \"created\" FROM ALL_USERS \
WHERE UPPER(USERNAME) LIKE NVL(UPPER(:filter_pattern), UPPER(USERNAME)) ESCAPE '\\' \
ORDER BY USERNAME FETCH FIRST :limit_plus_one ROWS ONLY";

/// `ALL_OBJECTS` joined to `ALL_TABLES` for the `Tables` group: the join is
/// what lets nested-table storage, IOT overflow segments and secondary
/// objects be excluded, none of which `ALL_OBJECTS` alone can tell apart from
/// an ordinary table. The recycle bin is excluded by name (`BIN$…`), which
/// Oracle reserves and no ordinary `CREATE TABLE` can produce.
const TABLES_SQL: &str = "SELECT o.OBJECT_NAME AS \"name\", o.STATUS AS \"status\", \
o.CREATED AS \"created\", o.LAST_DDL_TIME AS \"last_modified\" FROM ALL_OBJECTS o \
JOIN ALL_TABLES t ON t.OWNER = o.OWNER AND t.TABLE_NAME = o.OBJECT_NAME \
WHERE o.OWNER = :schema AND o.OBJECT_TYPE = 'TABLE' AND t.NESTED = 'NO' \
AND t.SECONDARY = 'N' AND (t.IOT_TYPE IS NULL OR t.IOT_TYPE != 'IOT_OVERFLOW') \
AND o.OBJECT_NAME NOT LIKE 'BIN$%' \
AND UPPER(o.OBJECT_NAME) LIKE NVL(UPPER(:filter_pattern), UPPER(o.OBJECT_NAME)) ESCAPE '\\' \
ORDER BY o.OBJECT_NAME FETCH FIRST :limit_plus_one ROWS ONLY";

/// `ALL_TAB_COLUMNS` scoped and shaped for [`MetadataRequest::ColumnsOf`].
///
/// `DATA_DEFAULT` (a `LONG`) is deliberately not selected; see the
/// `reldex_db_driver_api::metadata` module documentation. `nullable` is
/// rendered as the ANSI `information_schema` convention `"YES"`/`"NO"` rather
/// than Oracle's own `Y`/`N`, so the value is meaningful without knowing which
/// vendor answered.
const COLUMNS_OF_SQL: &str = "SELECT COLUMN_ID AS \"position\", COLUMN_NAME AS \"name\", \
DATA_TYPE AS \"type_name\", DECODE(NULLABLE, 'Y', 'YES', 'NO') AS \"nullable\" \
FROM ALL_TAB_COLUMNS WHERE OWNER = :schema AND TABLE_NAME = :table_name \
ORDER BY COLUMN_ID";

/// Every other object group: `ALL_OBJECTS` filtered by `OBJECT_TYPE`, which
/// (unlike `Tables`) needs no extra exclusion — none of these groups has a
/// nested/overflow/secondary shadow object, and none of their names collides
/// with the recycle bin's `BIN$…` convention.
fn simple_objects_of_kind_sql(object_type: &str) -> String {
    format!(
        "SELECT OBJECT_NAME AS \"name\", STATUS AS \"status\", CREATED AS \"created\", \
         LAST_DDL_TIME AS \"last_modified\" FROM ALL_OBJECTS WHERE OWNER = :schema \
         AND OBJECT_TYPE = '{object_type}' \
         AND UPPER(OBJECT_NAME) LIKE NVL(UPPER(:filter_pattern), UPPER(OBJECT_NAME)) ESCAPE '\\' \
         ORDER BY OBJECT_NAME FETCH FIRST :limit_plus_one ROWS ONLY"
    )
}

/// The SQL text for one [`MetadataObjectKind`], or [`ErrorKind::Unsupported`]
/// for a kind this driver predates.
fn objects_of_kind_sql(kind: MetadataObjectKind) -> DbResult<String> {
    Ok(match kind {
        MetadataObjectKind::Tables => TABLES_SQL.to_owned(),
        MetadataObjectKind::Views => simple_objects_of_kind_sql("VIEW"),
        MetadataObjectKind::Packages => simple_objects_of_kind_sql("PACKAGE"),
        MetadataObjectKind::PackageBodies => simple_objects_of_kind_sql("PACKAGE BODY"),
        MetadataObjectKind::Procedures => simple_objects_of_kind_sql("PROCEDURE"),
        MetadataObjectKind::Functions => simple_objects_of_kind_sql("FUNCTION"),
        MetadataObjectKind::Triggers => simple_objects_of_kind_sql("TRIGGER"),
        MetadataObjectKind::Sequences => simple_objects_of_kind_sql("SEQUENCE"),
        MetadataObjectKind::Synonyms => simple_objects_of_kind_sql("SYNONYM"),
        _ => {
            return Err(DbError::unsupported(
                "this metadata object kind (a future kind this driver predates)",
            ));
        }
    })
}

/// The `filter_pattern`/`limit_plus_one` binds every list request shares.
fn filter_and_limit_binds(
    name_filter: Option<String>,
    limit: std::num::NonZeroU32,
) -> Vec<NamedBind> {
    let pattern: Option<String> = name_filter.as_deref().map(name_filter_pattern);
    // `limit + 1`: the documented truncation signal
    // (`reldex_db_driver_api::metadata`, "Row cap and truncation"). `limit` is
    // a `u32`, so this never overflows `i64`.
    let limit_plus_one = i64::from(limit.get()) + 1;
    vec![
        NamedBind::new("filter_pattern", Bind::input(pattern)),
        NamedBind::new("limit_plus_one", Bind::input(limit_plus_one)),
    ]
}

/// Native `ORA-` codes an [`OracleMetadataCatalog`] statement can produce that
/// mean "not visible to this session" even though the general classifier —
/// which must stay correct for arbitrary user SQL — reports something else
/// for them.
///
/// - `942` ("table or view does not exist") is Oracle's answer for both
///   "no such object" and "no privilege to see it"
///   (`crate::error`'s `classify_code` reports `Syntax`, the right answer for
///   ordinary user SQL where the object genuinely may not exist). Every
///   statement this catalog builds names a dictionary view chosen by this
///   driver, which always exists, so here the ambiguity resolves the other
///   way.
/// - `1039` and `4043` are the same "does not exist / not visible" shape for
///   a describe-style lookup and a stored-object reference respectively.
const AMBIGUOUS_NOT_VISIBLE_CODES: [i32; 3] = [942, 1039, 4043];

/// Reclassifies one of [`AMBIGUOUS_NOT_VISIBLE_CODES`] as
/// [`ErrorKind::Permission`], preserving everything else about the error.
fn reclassify_ambiguous_permission_error(error: DbError) -> DbError {
    if error.kind() == ErrorKind::Permission {
        return error;
    }
    let Some(code) = error.native().map(NativeError::code) else {
        return error;
    };
    if !AMBIGUOUS_NOT_VISIBLE_CODES.contains(&code) {
        return error;
    }
    // `DbError` has no setter for its own kind and no way to take `source`
    // back out once lent through `Error::source`; this driver never attaches
    // one to a server-message error (see `crate::error`'s module
    // documentation), so nothing is lost by rebuilding — but say so out loud
    // rather than leave it to be rediscovered if that ever changes.
    debug_assert!(
        std::error::Error::source(&error).is_none(),
        "this rebuild would drop a source that this driver does not currently attach"
    );
    let mut rebuilt = DbError::new(ErrorKind::Permission, error.message().to_owned())
        .with_session_state(error.session_state())
        .with_retryable(error.is_retryable());
    if let Some(native) = error.native() {
        rebuilt = rebuilt.with_native(NativeError::new(native.code(), native.message()));
    }
    if let Some(position) = error.position() {
        rebuilt = rebuilt.with_position(*position);
    }
    rebuilt
}

/// The Oracle Database implementation of `MetadataCatalog` (`SPEC.md` §16).
///
/// Stateless: every method builds a [`Statement`] from its request and
/// returns it without touching the network. Dictionary SQL for all nine
/// object groups lives only here (`AGENTS.md`: vendor-specific database
/// behavior stays in drivers).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct OracleMetadataCatalog;

impl MetadataCatalog for OracleMetadataCatalog {
    fn prepare(&self, request: MetadataRequest) -> DbResult<PreparedMetadataQuery> {
        match request {
            MetadataRequest::Schemas { name_filter, limit } => {
                let statement = Statement::new(SCHEMAS_SQL)
                    .with_named_binds(filter_and_limit_binds(name_filter, limit));
                Ok(PreparedMetadataQuery::new(statement, schemas_columns()))
            }
            MetadataRequest::ObjectsOfKind {
                schema,
                kind,
                name_filter,
                limit,
            } => {
                let sql = objects_of_kind_sql(kind)?;
                let mut binds = vec![NamedBind::new("schema", Bind::input(schema))];
                binds.extend(filter_and_limit_binds(name_filter, limit));
                let statement = Statement::new(sql).with_named_binds(binds);
                Ok(PreparedMetadataQuery::new(
                    statement,
                    objects_of_kind_columns(),
                ))
            }
            MetadataRequest::ColumnsOf { schema, table } => {
                let binds = vec![
                    NamedBind::new("schema", Bind::input(schema)),
                    NamedBind::new("table_name", Bind::input(table)),
                ];
                let statement = Statement::new(COLUMNS_OF_SQL).with_named_binds(binds);
                Ok(PreparedMetadataQuery::new(statement, columns_of_columns()))
            }
            _ => Err(DbError::unsupported(
                "this metadata request shape (a future shape this driver predates)",
            )),
        }
    }

    fn classify_error(&self, error: DbError) -> DbError {
        reclassify_ambiguous_permission_error(error)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use reldex_db_driver_api::{Binds, SessionState, SqlPosition};

    use super::*;

    fn limit(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    fn named_bind_value<'a>(
        statement: &'a Statement,
        name: &str,
    ) -> Option<&'a reldex_db_driver_api::BindValue> {
        let Binds::Named(binds) = statement.binds() else {
            panic!("expected named binds");
        };
        binds
            .iter()
            .find(|bind| bind.name() == name)
            .and_then(|bind| bind.bind().value())
    }

    #[test]
    fn schemas_yields_the_declared_contract_and_expected_sql() {
        let prepared = OracleMetadataCatalog
            .prepare(MetadataRequest::schemas(limit(50)))
            .expect("schemas is supported");
        assert_eq!(prepared.statement().sql(), SCHEMAS_SQL);
        assert_eq!(prepared.columns(), schemas_columns());
        assert_eq!(
            named_bind_value(prepared.statement(), "limit_plus_one"),
            Some(&reldex_db_driver_api::BindValue::from(51_i64))
        );
        assert_eq!(
            named_bind_value(prepared.statement(), "filter_pattern"),
            Some(&reldex_db_driver_api::BindValue::Null)
        );
    }

    #[test]
    fn every_object_kind_yields_its_declared_contract() {
        for kind in [
            MetadataObjectKind::Tables,
            MetadataObjectKind::Views,
            MetadataObjectKind::Packages,
            MetadataObjectKind::PackageBodies,
            MetadataObjectKind::Procedures,
            MetadataObjectKind::Functions,
            MetadataObjectKind::Triggers,
            MetadataObjectKind::Sequences,
            MetadataObjectKind::Synonyms,
        ] {
            let prepared = OracleMetadataCatalog
                .prepare(MetadataRequest::objects_of_kind("HR", kind, limit(10)))
                .unwrap_or_else(|error| panic!("{kind:?} should be supported: {error}"));
            assert_eq!(prepared.columns(), objects_of_kind_columns(), "{kind:?}");
            assert_eq!(
                named_bind_value(prepared.statement(), "schema"),
                Some(&reldex_db_driver_api::BindValue::from("HR")),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn the_sql_text_is_a_constant_per_kind_regardless_of_schema_filter_or_limit() {
        // A statement whose text depended on caller-controlled data would be a
        // SQL-injection surface; identifiers must travel as binds only.
        let a = OracleMetadataCatalog
            .prepare(MetadataRequest::objects_of_kind(
                "HR",
                MetadataObjectKind::Tables,
                limit(10),
            ))
            .expect("supported");
        let b = OracleMetadataCatalog
            .prepare(
                MetadataRequest::objects_of_kind(
                    "OTHER_SCHEMA'; DROP TABLE t; --",
                    MetadataObjectKind::Tables,
                    limit(999_999),
                )
                .with_name_filter("%_anything\\"),
            )
            .expect("supported");
        assert_eq!(a.statement().sql(), b.statement().sql());
        assert_eq!(a.statement().sql(), TABLES_SQL);

        let schemas_a = OracleMetadataCatalog
            .prepare(MetadataRequest::schemas(limit(1)))
            .expect("supported");
        let schemas_b = OracleMetadataCatalog
            .prepare(MetadataRequest::schemas(limit(2)).with_name_filter("x"))
            .expect("supported");
        assert_eq!(schemas_a.statement().sql(), schemas_b.statement().sql());

        let columns_a = OracleMetadataCatalog
            .prepare(MetadataRequest::columns_of("HR", "EMPLOYEES"))
            .expect("supported");
        let columns_b = OracleMetadataCatalog
            .prepare(MetadataRequest::columns_of("X'; --", "Y\" OR 1=1 --"))
            .expect("supported");
        assert_eq!(columns_a.statement().sql(), columns_b.statement().sql());
        assert_eq!(columns_a.statement().sql(), COLUMNS_OF_SQL);
    }

    #[test]
    fn schema_and_table_identifiers_travel_as_bind_values_verbatim() {
        let tricky_schema = "HR'; DROP TABLE t; --";
        let prepared = OracleMetadataCatalog
            .prepare(MetadataRequest::objects_of_kind(
                tricky_schema,
                MetadataObjectKind::Views,
                limit(5),
            ))
            .expect("supported");
        assert_eq!(
            named_bind_value(prepared.statement(), "schema"),
            Some(&reldex_db_driver_api::BindValue::from(tricky_schema))
        );
        assert!(!prepared.statement().sql().contains(tricky_schema));

        let prepared = OracleMetadataCatalog
            .prepare(MetadataRequest::columns_of(
                "HR",
                "T\" UNION SELECT password FROM users --",
            ))
            .expect("supported");
        assert_eq!(
            named_bind_value(prepared.statement(), "table_name"),
            Some(&reldex_db_driver_api::BindValue::from(
                "T\" UNION SELECT password FROM users --"
            ))
        );
    }

    #[test]
    fn columns_of_has_no_limit_or_filter_binds() {
        let prepared = OracleMetadataCatalog
            .prepare(MetadataRequest::columns_of("HR", "EMPLOYEES"))
            .expect("supported");
        let Binds::Named(binds) = prepared.statement().binds() else {
            panic!("expected named binds");
        };
        let names: Vec<&str> = binds.iter().map(NamedBind::name).collect();
        assert_eq!(names, ["schema", "table_name"]);
    }

    #[test]
    fn a_name_filter_is_bound_as_the_escaped_like_pattern() {
        let prepared = OracleMetadataCatalog
            .prepare(MetadataRequest::schemas(limit(10)).with_name_filter("50%_off"))
            .expect("supported");
        assert_eq!(
            named_bind_value(prepared.statement(), "filter_pattern"),
            Some(&reldex_db_driver_api::BindValue::from(name_filter_pattern(
                "50%_off"
            )))
        );
    }

    #[test]
    fn limit_is_bound_as_limit_plus_one_for_truncation_detection() {
        for requested in [1_u32, 10, 1000] {
            let prepared = OracleMetadataCatalog
                .prepare(MetadataRequest::schemas(limit(requested)))
                .expect("supported");
            assert_eq!(
                named_bind_value(prepared.statement(), "limit_plus_one"),
                Some(&reldex_db_driver_api::BindValue::from(
                    i64::from(requested) + 1
                ))
            );
        }
    }

    #[test]
    fn ambiguous_not_visible_codes_are_reclassified_as_permission() {
        for code in AMBIGUOUS_NOT_VISIBLE_CODES {
            let native = NativeError::new(code, format!("ORA-{code:05}: does not exist"));
            let error =
                DbError::new(ErrorKind::Syntax, "table or view does not exist").with_native(native);
            let reclassified = OracleMetadataCatalog.classify_error(error);
            assert_eq!(reclassified.kind(), ErrorKind::Permission, "ORA-{code:05}");
            assert_eq!(
                reclassified.native().map(NativeError::code),
                Some(code),
                "the native code must survive the reclassification"
            );
        }
    }

    #[test]
    fn classify_error_preserves_position_and_session_state() {
        let error = DbError::new(ErrorKind::Syntax, "table or view does not exist")
            .with_native(NativeError::new(
                942,
                "ORA-00942: table or view does not exist",
            ))
            .with_position(SqlPosition::at_char_offset(7))
            .with_session_state(SessionState::Usable)
            .with_retryable(false);
        let reclassified = OracleMetadataCatalog.classify_error(error);
        assert_eq!(reclassified.kind(), ErrorKind::Permission);
        assert_eq!(reclassified.session_state(), SessionState::Usable);
        assert_eq!(
            reclassified
                .position()
                .copied()
                .and_then(SqlPosition::char_offset),
            Some(7)
        );
    }

    #[test]
    fn classify_error_leaves_unrelated_errors_alone() {
        let error = DbError::new(ErrorKind::Syntax, "ORA-00904: invalid identifier")
            .with_native(NativeError::new(904, "ORA-00904: invalid identifier"));
        let reclassified = OracleMetadataCatalog.classify_error(error);
        assert_eq!(reclassified.kind(), ErrorKind::Syntax);

        // Already-`Permission` errors (e.g. ORA-01031, which the general
        // classifier already gets right) pass through unchanged.
        let permission = DbError::new(ErrorKind::Permission, "insufficient privileges")
            .with_native(NativeError::new(1031, "ORA-01031: insufficient privileges"));
        let unchanged = OracleMetadataCatalog.classify_error(permission);
        assert_eq!(unchanged.kind(), ErrorKind::Permission);
        assert_eq!(unchanged.native().map(NativeError::code), Some(1031));
    }

    #[test]
    fn an_error_with_no_native_code_is_left_alone() {
        let error = DbError::new(ErrorKind::Other, "no native code here");
        let reclassified = OracleMetadataCatalog.classify_error(error);
        assert_eq!(reclassified.kind(), ErrorKind::Other);
    }
}
