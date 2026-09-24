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
//! [`reclassify_ambiguous_permission_error`].
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
//! filter uses `UPPER(col) LIKE NVL(UPPER(:filter_pattern), '%') ESCAPE '\'`:
//! `:filter_pattern` appears exactly once, and when it is NULL — no filter —
//! the pattern degenerates to the constant wildcard `'%'`, which matches
//! every non-NULL `col` without depending on what `col` itself contains.
//!
//! An earlier draft used `UPPER(col)` as this fallback instead of `'%'`: when
//! there was no filter, the pattern became `UPPER(col) LIKE UPPER(col)`,
//! which is true for any non-NULL `col` — correct as a truth table, but wrong
//! as a `LIKE` pattern. `ESCAPE '\'` makes every `\` in *the pattern*
//! significant, and the pattern was `col`'s own value: a name legally
//! containing a literal `\` (a quoted identifier such as
//! `"OPS$DOMAIN\user"`, which Oracle happily creates) turned into a pattern
//! ending in an unpartnered escape character, raising `ORA-01424` and failing
//! the *entire* unfiltered listing rather than just mishandling that one row.
//! `'%'` never contains a `\`, so it carries no such risk regardless of what
//! any stored name contains. Every column filtered this way (`OBJECT_NAME`,
//! `USERNAME`) is `NOT NULL` in the dictionary, so the `NVL` only ever
//! substitutes when `:filter_pattern` itself is NULL — i.e. no filter — never
//! because of the column.

use reldex_db_driver_api::{
    Bind, DbError, DbResult, ErrorKind, MetadataCatalog, MetadataObjectKind, MetadataRequest,
    NamedBind, NativeError, PreparedMetadataQuery, Statement, columns_of_columns,
    name_filter_pattern, objects_of_kind_columns, schemas_columns,
};

/// `ALL_USERS` scoped and shaped for [`MetadataRequest::Schemas`].
const SCHEMAS_SQL: &str = "SELECT USERNAME AS \"name\", CREATED AS \"created\" FROM ALL_USERS \
WHERE UPPER(USERNAME) LIKE NVL(UPPER(:filter_pattern), '%') ESCAPE '\\' \
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
AND UPPER(o.OBJECT_NAME) LIKE NVL(UPPER(:filter_pattern), '%') ESCAPE '\\' \
ORDER BY o.OBJECT_NAME FETCH FIRST :limit_plus_one ROWS ONLY";

/// `ALL_TAB_COLUMNS` scoped and shaped for [`MetadataRequest::ColumnsOf`].
///
/// `DATA_DEFAULT` (a `LONG`) is deliberately not selected; see the
/// `reldex_db_driver_api::metadata` module documentation. `nullable` is
/// rendered as the ANSI `information_schema` convention `"YES"`/`"NO"` rather
/// than Oracle's own `Y`/`N`, so the value is meaningful without knowing which
/// vendor answered.
///
/// `AND COLUMN_ID IS NOT NULL` excludes `INVISIBLE` columns: Oracle gives
/// them no ordinal position (`COLUMN_ID` is NULL), which would otherwise
/// falsify the declared contract's `position` column (always non-nullable)
/// in the *data*. This is the same default SQL Developer's column list uses;
/// a later phase can add an explicit "show invisible columns" request shape
/// if that turns out to matter.
///
/// This is a promise about the *data*, not about what a describe of the
/// statement reports. Oracle's own describe protocol reports the server's
/// `nulls_allowed` flag for a projected column, and the pinned `oracledb`
/// crate copies it verbatim with no client-side computation
/// (`nullable: (nulls_allowed != 0)`,
/// `oracledb-26.0.0-beta.3/src/metadata.rs:120,157`) — see ADR-0002, "Notes
/// for driver implementers". That flag narrows to non-null only for a
/// **bare reference to a `NOT NULL` column** (e.g. `ALL_USERS.USERNAME`);
/// every computed expression describes as nullable regardless of how
/// provably non-null it is, and a `WHERE` predicate never narrows a bare
/// column's own declared nullability either. `type_name` and `nullable`
/// below are computed (`CASE`/`DECODE`) and so always describe nullable.
/// `position` is a bare `COLUMN_ID` reference, not computed — but
/// `COLUMN_ID` is itself declared nullable in `ALL_TAB_COLUMNS`'s own
/// definition (it holds NULL for an `INVISIBLE` column), and the `AND
/// COLUMN_ID IS NOT NULL` filter above narrows the *data*, not what Oracle
/// reports for the source column's own declared nullability, so `position`
/// also describes nullable. None of the three can ever describe as
/// non-nullable through this driver regardless of SQL wording;
/// `m2_8_metadata_catalog.rs`'s `assert_contract_matches` documents this and
/// checks the promise against the data directly instead.
///
/// `type_name` composes Oracle's own precision/scale/length qualifiers onto
/// `DATA_TYPE` for the families where `DATA_TYPE` alone loses information —
/// `NUMBER`, `FLOAT`, `VARCHAR2`, `CHAR`, `NCHAR`, `NVARCHAR2`, `RAW` — using
/// the same rules Oracle's own DDL-generation tools use (`NUMBER(*,s)` is
/// Oracle's own notation for "any precision, scale `s`"; `VARCHAR2` and
/// `CHAR` both show a `CHAR|BYTE` unit, since `CHAR_USED` distinguishes char
/// from byte semantics for either family — `NCHAR`/`NVARCHAR2` never need
/// the unit, since a national-charset column is always char-length
/// semantics). Every other family (`DATE`, `CLOB`, `RAW` handled above, and
/// notably `TIMESTAMP(6) WITH TIME ZONE`/`INTERVAL DAY(2) TO SECOND(6)`-
/// shaped types) already carries its precision inside Oracle's own
/// `DATA_TYPE` text, so the `ELSE` branch
/// passes it through unchanged.
const COLUMNS_OF_SQL: &str = "SELECT COLUMN_ID AS \"position\", COLUMN_NAME AS \"name\", \
CASE DATA_TYPE \
WHEN 'NUMBER' THEN CASE \
  WHEN DATA_PRECISION IS NOT NULL AND DATA_SCALE IS NOT NULL \
    THEN 'NUMBER(' || DATA_PRECISION || ',' || DATA_SCALE || ')' \
  WHEN DATA_PRECISION IS NULL AND DATA_SCALE IS NOT NULL \
    THEN 'NUMBER(*,' || DATA_SCALE || ')' \
  ELSE 'NUMBER' \
END \
WHEN 'FLOAT' THEN 'FLOAT(' || DATA_PRECISION || ')' \
WHEN 'VARCHAR2' THEN 'VARCHAR2(' || CHAR_LENGTH || ' ' \
  || DECODE(CHAR_USED, 'C', 'CHAR', 'B', 'BYTE', 'BYTE') || ')' \
WHEN 'CHAR' THEN 'CHAR(' || CHAR_LENGTH || ' ' \
  || DECODE(CHAR_USED, 'C', 'CHAR', 'B', 'BYTE', 'BYTE') || ')' \
WHEN 'NCHAR' THEN 'NCHAR(' || CHAR_LENGTH || ')' \
WHEN 'NVARCHAR2' THEN 'NVARCHAR2(' || CHAR_LENGTH || ')' \
WHEN 'RAW' THEN 'RAW(' || DATA_LENGTH || ')' \
ELSE DATA_TYPE \
END AS \"type_name\", \
DECODE(NULLABLE, 'Y', 'YES', 'NO') AS \"nullable\" \
FROM ALL_TAB_COLUMNS WHERE OWNER = :schema AND TABLE_NAME = :table_name \
AND COLUMN_ID IS NOT NULL \
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
         AND UPPER(OBJECT_NAME) LIKE NVL(UPPER(:filter_pattern), '%') ESCAPE '\\' \
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
/// - `1039` ("insufficient privileges on underlying objects of the view") is
///   the same "not visible" shape one level down: it names the view's own
///   underlying objects rather than the view.
///
/// Two codes the M2.8 review round considered and rejected: `1031`
/// ("insufficient privileges") is not here because `classify_code` already
/// reports it as [`ErrorKind::Permission`] unconditionally — there is no
/// ambiguity for this list to resolve. `4043` ("object does not exist") was
/// in an earlier draft of this list; it is a match by name against a
/// bare `4043`, but every statement this catalog builds is a plain `SELECT`,
/// and `ORA-04043` is Oracle's answer for a *DDL or PL/SQL* reference to a
/// missing object (`ALTER`/`DROP`/a PL/SQL call naming an object that is not
/// there), not something a `SELECT` on an existing, `PUBLIC`-granted `ALL_*`
/// view can raise — there is no statement here that could ever produce it,
/// so keeping it would be an untested, unreachable branch.
const AMBIGUOUS_NOT_VISIBLE_CODES: [i32; 2] = [942, 1039];

/// The [`reldex_db_driver_api::MetadataErrorClassifier`]
/// [`OracleMetadataCatalog::prepare`] embeds in every [`PreparedMetadataQuery`]
/// it returns: reclassifies one of [`AMBIGUOUS_NOT_VISIBLE_CODES`] as
/// [`ErrorKind::Permission`], preserving everything else about the error, or
/// declines (`None`) for anything else.
fn reclassify_ambiguous_permission_error(error: &DbError) -> Option<DbError> {
    if error.kind() == ErrorKind::Permission {
        return None;
    }
    let code = error.native().map(NativeError::code)?;
    if !AMBIGUOUS_NOT_VISIBLE_CODES.contains(&code) {
        return None;
    }
    // `DbError` has no setter for its own kind and no way to take `source`
    // back out once lent through `Error::source`; this driver never attaches
    // one to a server-message error (see `crate::error`'s module
    // documentation), so nothing is lost by rebuilding — but say so out loud
    // rather than leave it to be rediscovered if that ever changes.
    debug_assert!(
        std::error::Error::source(error).is_none(),
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
    Some(rebuilt)
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
                Ok(PreparedMetadataQuery::new(
                    statement,
                    schemas_columns(),
                    reclassify_ambiguous_permission_error,
                ))
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
                    reclassify_ambiguous_permission_error,
                ))
            }
            MetadataRequest::ColumnsOf { schema, table } => {
                let binds = vec![
                    NamedBind::new("schema", Bind::input(schema)),
                    NamedBind::new("table_name", Bind::input(table)),
                ];
                let statement = Statement::new(COLUMNS_OF_SQL).with_named_binds(binds);
                Ok(PreparedMetadataQuery::new(
                    statement,
                    columns_of_columns(),
                    reclassify_ambiguous_permission_error,
                ))
            }
            _ => Err(DbError::unsupported(
                "this metadata request shape (a future shape this driver predates)",
            )),
        }
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

    /// M2.8 review round, must-fix 1: an unfiltered listing's `LIKE` pattern
    /// must fall back to the constant `'%'`, never to the column's own
    /// value — a fallback of `UPPER(col)` put whatever a stored name
    /// happened to contain into the pattern, and a `\` in that name (legal
    /// in a quoted identifier) raised `ORA-01424` under `ESCAPE '\'` and
    /// failed the entire listing. This checks the SQL text directly, at the
    /// level a unit test can; the live behaviour is covered by
    /// `m2_8_metadata_catalog.rs`'s unfiltered-`Schemas` and
    /// quoted-identifier tests.
    #[test]
    fn the_unfiltered_like_pattern_falls_back_to_a_constant_wildcard() {
        for sql in [
            SCHEMAS_SQL,
            TABLES_SQL,
            &simple_objects_of_kind_sql("VIEW"),
            &simple_objects_of_kind_sql("SEQUENCE"),
        ] {
            assert!(
                sql.contains("NVL(UPPER(:filter_pattern), '%')"),
                "expected the constant '%' fallback in: {sql}"
            );
            assert!(
                !sql.contains("NVL(UPPER(:filter_pattern), UPPER("),
                "must not fall back to the filtered column's own value: {sql}"
            );
        }
    }

    /// M2.8 review round, must-fix 2: `INVISIBLE` columns (`COLUMN_ID IS
    /// NULL` in `ALL_TAB_COLUMNS`) must be excluded, so the declared
    /// `position` contract (always non-nullable) is never falsified.
    #[test]
    fn columns_of_sql_excludes_invisible_columns() {
        assert!(COLUMNS_OF_SQL.contains("AND COLUMN_ID IS NOT NULL"));
    }

    /// M2.8 review round, should-fix 3: `type_name` composes Oracle's own
    /// precision/scale/length qualifiers for the families where bare
    /// `DATA_TYPE` loses information. Exact-string behaviour against a real
    /// table is `m2_8_metadata_catalog.rs`'s job; this only checks the SQL
    /// text builds a case for each documented family.
    #[test]
    fn columns_of_sql_composes_type_name_for_every_documented_family() {
        for fragment in [
            "WHEN 'NUMBER'",
            "WHEN 'FLOAT'",
            "WHEN 'VARCHAR2'",
            "WHEN 'CHAR'",
            "WHEN 'NCHAR'",
            "WHEN 'NVARCHAR2'",
            "WHEN 'RAW'",
            "ELSE DATA_TYPE",
        ] {
            assert!(
                COLUMNS_OF_SQL.contains(fragment),
                "expected {fragment} in COLUMNS_OF_SQL"
            );
        }
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
            let reclassified = reclassify_ambiguous_permission_error(&error)
                .unwrap_or_else(|| panic!("ORA-{code:05} should be reclassified"));
            assert_eq!(reclassified.kind(), ErrorKind::Permission, "ORA-{code:05}");
            assert_eq!(
                reclassified.native().map(NativeError::code),
                Some(code),
                "the native code must survive the reclassification"
            );
        }
    }

    #[test]
    fn dropped_code_4043_is_no_longer_reclassified() {
        // The M2.8 review round dropped ORA-04043 from the ambiguous list:
        // every statement this catalog builds is a plain `SELECT` against a
        // `PUBLIC`-granted `ALL_*` view, and `ORA-04043` is Oracle's answer
        // for a DDL/PL-SQL reference to a missing object, not something a
        // `SELECT` here can raise. Kept as a regression guard, not a claim
        // that 4043 is reachable through this catalog.
        let error = DbError::new(ErrorKind::Syntax, "object does not exist")
            .with_native(NativeError::new(4043, "ORA-04043: object does not exist"));
        assert!(reclassify_ambiguous_permission_error(&error).is_none());
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
        let reclassified =
            reclassify_ambiguous_permission_error(&error).expect("942 should be reclassified");
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
        assert!(reclassify_ambiguous_permission_error(&error).is_none());

        // Already-`Permission` errors (e.g. ORA-01031, which the general
        // classifier already gets right) decline too: there is nothing left
        // for this catalog-specific correction to do.
        let permission = DbError::new(ErrorKind::Permission, "insufficient privileges")
            .with_native(NativeError::new(1031, "ORA-01031: insufficient privileges"));
        assert!(reclassify_ambiguous_permission_error(&permission).is_none());
    }

    #[test]
    fn an_error_with_no_native_code_is_left_alone() {
        let error = DbError::new(ErrorKind::Other, "no native code here");
        assert!(reclassify_ambiguous_permission_error(&error).is_none());
    }

    #[test]
    fn every_prepared_query_carries_the_reclassifying_classifier() {
        // The correction travels with the returned value (M2.8 review
        // round): a caller holding only the `PreparedMetadataQuery`, not a
        // separate `&dyn MetadataCatalog`, can still get it corrected.
        let prepared_queries = [
            OracleMetadataCatalog
                .prepare(MetadataRequest::schemas(limit(1)))
                .expect("schemas is supported"),
            OracleMetadataCatalog
                .prepare(MetadataRequest::objects_of_kind(
                    "HR",
                    MetadataObjectKind::Tables,
                    limit(1),
                ))
                .expect("objects_of_kind is supported"),
            OracleMetadataCatalog
                .prepare(MetadataRequest::columns_of("HR", "EMPLOYEES"))
                .expect("columns_of is supported"),
        ];
        for prepared in prepared_queries {
            let error =
                DbError::new(ErrorKind::Syntax, "table or view does not exist").with_native(
                    NativeError::new(942, "ORA-00942: table or view does not exist"),
                );
            assert_eq!(
                prepared.reclassify_error(error).kind(),
                ErrorKind::Permission
            );
        }
    }
}
