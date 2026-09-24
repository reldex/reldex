//! Vendor-neutral metadata catalog descriptor ("the light shape";
//! `docs/decisions/0002-driver-api-and-concurrency-model.md` amendment B4.3;
//! `SPEC.md` §16; `docs/exec-plans/active/phase-1.md` §B4 item 3, M2.8).
//!
//! [`DatabaseDriver::metadata_catalog`](crate::DatabaseDriver::metadata_catalog)
//! returns a [`MetadataCatalog`], which does **not** execute anything and does
//! **not** return rows itself. For each vendor-neutral [`MetadataRequest`] it
//! returns a [`PreparedMetadataQuery`]: an ordinary [`Statement`] — SQL text
//! plus typed binds, exactly like any other statement this contract knows how
//! to run — together with a declared column contract. The caller (`db-core`,
//! or a test standing in for it) executes that statement through the ordinary
//! [`DatabaseConnection::execute`](crate::DatabaseConnection::execute) /
//! [`Cursor::fetch_batch`](crate::Cursor::fetch_batch) path and gets an
//! ordinary [`RowBatch`](crate::RowBatch) back. There is no new result type, no
//! paging and no cache: a `MetadataProvider` trait with its own result shape
//! was considered and rejected for Phase 1 — duplicating the fetch path bought
//! nothing an object browser needs yet, and the SQLite metadata cache
//! (`ARCHITECTURE.md` §13 item 8) is a P2 concern regardless.
//!
//! # The column contract
//!
//! Every list request's declared columns ([`schemas_columns`],
//! [`objects_of_kind_columns`], [`columns_of_columns`]) are fixed, ordered,
//! lower-case identifiers that are the **same for every driver** — a UI can
//! bind to `"name"` or `"status"` without knowing which vendor answered. A
//! driver's prepared statement must alias its SQL so the executed result's
//! columns come back with exactly these names, in exactly this order; that is
//! what lets a generic caller compare
//! [`Cursor::columns`](crate::Cursor::columns) against the declared contract
//! as a correctness check.
//!
//! The set is deliberately small: only facts that are cheap for Oracle to
//! supply today and that another relational vendor could plausibly supply too.
//! Notably absent is a column default expression: Oracle stores it in a `LONG`
//! column (`ALL_TAB_COLUMNS.DATA_DEFAULT`), and the pinned version of this
//! project's Oracle driver dependency can abort the whole process decoding
//! certain `LONG` shapes
//! (`crates/drivers/oracle-thin/tests/s12_dev_features.rs`, upstream gap U-4).
//! The Phase 1 contract leaves it out rather than risk a process abort from an
//! object browser; a later phase may add it once that upstream risk is
//! resolved or worked around.
//!
//! # Server-side name filtering
//!
//! `name_filter` on [`MetadataRequest::Schemas`] and
//! [`MetadataRequest::ObjectsOfKind`] is a case-insensitive **contains** match
//! against the object name, applied by the server — never by fetching
//! everything and filtering in `db-core`. `None` means no filter. The
//! characters `%`, `_` and the escape character itself are matched
//! **literally**, never as wildcards: [`name_filter_pattern`] builds the
//! `LIKE` pattern a driver binds as a parameter, with those three characters
//! escaped by [`NAME_FILTER_ESCAPE_CHAR`]. The filter text itself is always a
//! bind value; it is never concatenated into SQL text.
//!
//! # Row cap and truncation
//!
//! `limit` on a list request is mandatory. A driver applies it **server-side**
//! and its prepared statement returns **at most `limit.get() + 1` rows**.
//! Getting back exactly `limit.get() + 1` rows means the true result was
//! larger: the caller discards the last row (or otherwise notes truncation)
//! rather than treating it as data. This one extra row is the whole
//! signalling mechanism — there is no separate "was this truncated" flag — so
//! a caller must always compare the fetched row count against `limit` before
//! deciding the list is complete. Results are ordered deterministically (by
//! the object name) so that which rows are kept and which are the truncated
//! tail is stable across repeated calls with the same parameters.
//!
//! [`MetadataRequest::ColumnsOf`] has no `limit`: a table's column count is
//! already bounded by the server (1000 columns is Oracle's own hard limit), so
//! no server-side cap is needed.
//!
//! # Identifiers are never spliced into SQL
//!
//! `schema` and `table` are passed as bind values, compared against the
//! dictionary's own stored (case-sensitive, exact) spelling of the name. A
//! driver's SQL text for a given request shape is a **constant**: it does not
//! change when the schema, table, filter or limit change, only the bound
//! values do. This is what keeps a request built from user-controlled text
//! from becoming a SQL-injection surface; see the unit tests in each driver.
//!
//! # Permission failures
//!
//! A dictionary query can fail because the session lacks a privilege, and on
//! Oracle a missing privilege on a view and a genuinely absent object report
//! the identical `ORA-00942` — a driver cannot always tell those apart for
//! *ordinary user SQL*, so the general error classifier must not be changed to
//! favour one reading over the other. But a statement this catalog built
//! itself always names a dictionary object the driver chose, which always
//! exists, so a failure carrying one of the same "object not found" codes back
//! from *this* statement can only mean "not visible to this session" — a
//! permission fact, not a syntax one.
//!
//! That correction travels with the [`PreparedMetadataQuery`] itself rather
//! than living on a separate method a caller has to remember to invoke on the
//! `&dyn MetadataCatalog`: every value [`MetadataCatalog::prepare`] returns
//! embeds a [`MetadataErrorClassifier`] function pointer, applied through
//! [`PreparedMetadataQuery::reclassify_error`] to an error the statement's
//! execution produced. Because the classifier rides along on the value the
//! caller already holds (the thing it must have to execute the statement in
//! the first place), there is no separate step to forget. A driver whose
//! dictionary errors are already unambiguous passes
//! [`no_error_reclassification`], the identity classifier — no allocation, no
//! vendor-specific type, just a plain function pointer.

use std::num::NonZeroU32;

use crate::error::DbError;
use crate::statement::Statement;
use crate::types::{ColumnMetadata, SqlType};

/// The escape character [`name_filter_pattern`] uses for a literal `%`, `_` or
/// itself inside a name filter.
pub const NAME_FILTER_ESCAPE_CHAR: char = '\\';

/// Builds the `LIKE` pattern for a case-insensitive "contains" name filter.
///
/// Wraps `filter` in `%…%` and escapes any `%`, `_` or
/// [`NAME_FILTER_ESCAPE_CHAR`] it contains with [`NAME_FILTER_ESCAPE_CHAR`],
/// so a filter a user typed is matched **literally** rather than as a wildcard
/// pattern. The result is a plain string meant to be bound as a parameter and
/// compared with a case-folded `LIKE …  ESCAPE '\'` (or the vendor's
/// equivalent) — this function only builds the pattern; case-folding is a
/// SQL-text concern left to the driver.
///
/// ```
/// use reldex_db_driver_api::name_filter_pattern;
///
/// assert_eq!(name_filter_pattern("abc"), "%abc%");
/// assert_eq!(name_filter_pattern("50%"), "%50\\%%");
/// assert_eq!(name_filter_pattern("a_b"), "%a\\_b%");
/// assert_eq!(name_filter_pattern("a\\b"), "%a\\\\b%");
/// ```
#[must_use]
pub fn name_filter_pattern(filter: &str) -> String {
    let mut pattern = String::with_capacity(filter.len() + 2);
    pattern.push('%');
    for ch in filter.chars() {
        if ch == NAME_FILTER_ESCAPE_CHAR || ch == '%' || ch == '_' {
            pattern.push(NAME_FILTER_ESCAPE_CHAR);
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// One of the object groups `SPEC.md` §16 lists, other than `Schemas` (which
/// [`MetadataRequest::Schemas`] covers on its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MetadataObjectKind {
    /// Tables, excluding nested-table storage tables, index-organized-table
    /// overflow segments, secondary objects and the recycle bin.
    Tables,
    /// Views.
    Views,
    /// PL/SQL package specifications.
    Packages,
    /// PL/SQL package bodies.
    PackageBodies,
    /// Stored procedures.
    Procedures,
    /// Stored functions.
    Functions,
    /// Triggers.
    Triggers,
    /// Sequences.
    Sequences,
    /// Synonyms.
    Synonyms,
}

/// One vendor-neutral metadata request a [`MetadataCatalog`] can prepare.
///
/// Built through [`MetadataRequest::schemas`], [`MetadataRequest::objects_of_kind`]
/// or [`MetadataRequest::columns_of`], and optionally
/// [`MetadataRequest::with_name_filter`]. `#[non_exhaustive]` so a future
/// request shape does not break existing callers; a driver that does not yet
/// support a shape it does recognize (a new [`MetadataObjectKind`] it
/// predates) returns [`crate::ErrorKind::Unsupported`] from
/// [`MetadataCatalog::prepare`] instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MetadataRequest {
    /// Every schema (user/owner) the session can see.
    Schemas {
        /// Server-side, case-insensitive "contains" filter; see the module
        /// documentation. `None` means no filter.
        name_filter: Option<String>,
        /// The maximum number of schemas to report; see the module
        /// documentation on truncation.
        limit: NonZeroU32,
    },
    /// Every object of one kind in one schema.
    ObjectsOfKind {
        /// The schema to list, matched exactly (case-sensitive) against the
        /// dictionary's stored spelling.
        schema: String,
        /// Which of the nine object groups to list.
        kind: MetadataObjectKind,
        /// Server-side, case-insensitive "contains" filter; see the module
        /// documentation. `None` means no filter.
        name_filter: Option<String>,
        /// The maximum number of objects to report; see the module
        /// documentation on truncation.
        limit: NonZeroU32,
    },
    /// Every column of one table (or view), in ordinal position.
    ColumnsOf {
        /// The schema owning the table, matched exactly (case-sensitive).
        schema: String,
        /// The table (or view) name, matched exactly (case-sensitive).
        table: String,
    },
}

impl MetadataRequest {
    /// A [`MetadataRequest::Schemas`] request with no name filter.
    #[must_use]
    pub const fn schemas(limit: NonZeroU32) -> Self {
        Self::Schemas {
            name_filter: None,
            limit,
        }
    }

    /// A [`MetadataRequest::ObjectsOfKind`] request with no name filter.
    #[must_use]
    pub fn objects_of_kind(
        schema: impl Into<String>,
        kind: MetadataObjectKind,
        limit: NonZeroU32,
    ) -> Self {
        Self::ObjectsOfKind {
            schema: schema.into(),
            kind,
            name_filter: None,
            limit,
        }
    }

    /// A [`MetadataRequest::ColumnsOf`] request.
    #[must_use]
    pub fn columns_of(schema: impl Into<String>, table: impl Into<String>) -> Self {
        Self::ColumnsOf {
            schema: schema.into(),
            table: table.into(),
        }
    }

    /// Attaches a server-side name filter to a `Schemas` or `ObjectsOfKind`
    /// request.
    ///
    /// # Panics
    ///
    /// Panics if `self` is [`MetadataRequest::ColumnsOf`]. `ColumnsOf` has no
    /// name filter to set and never will (`SPEC.md` §16: a table's columns
    /// are always shown in full) — silently ignoring the call would let a
    /// caller believe a filter is in effect when it is not, so this is a loud
    /// programmer error rather than a quiet no-op.
    #[must_use]
    pub fn with_name_filter(self, filter: impl Into<String>) -> Self {
        match self {
            Self::Schemas { limit, .. } => Self::Schemas {
                name_filter: Some(filter.into()),
                limit,
            },
            Self::ObjectsOfKind {
                schema,
                kind,
                limit,
                ..
            } => Self::ObjectsOfKind {
                schema,
                kind,
                name_filter: Some(filter.into()),
                limit,
            },
            Self::ColumnsOf { .. } => panic!(
                "MetadataRequest::with_name_filter called on ColumnsOf, which has no name \
                 filter to set"
            ),
        }
    }
}

/// A permission-ambiguity correction a [`PreparedMetadataQuery`] carries;
/// see the module documentation, "Permission failures".
///
/// Given a reference to an error produced while executing that query's
/// [`Statement`], returns the corrected error (typically the same error with
/// its `kind` changed to [`crate::ErrorKind::Permission`]) if the ambiguity
/// applies, or `None` if the error is unrelated and should pass through
/// unchanged. A plain function pointer: no allocation, no vendor-specific
/// type, and trivially satisfied by a driver with nothing to correct — see
/// [`no_error_reclassification`].
///
/// A classifier receives only `&DbError`, not owned `DbError`, so it cannot
/// take a `source` out of the original error (`DbError` lends its `source`
/// through [`std::error::Error::source`], which borrows from `&self`, and
/// has no owned-transfer form). A classifier that corrects by rebuilding a
/// new `DbError` therefore cannot carry an existing `source` chain forward
/// onto the corrected value; a driver whose errors do attach one needs to
/// either accept that a reclassified error drops it, or classify without
/// rebuilding (not possible today, since `DbError` has no in-place `kind`
/// setter). `reldex_driver_oracle_thin`'s classifier does not currently
/// attach a `source` to server-message errors, so this does not lose
/// anything there today; it would if that ever changed.
pub type MetadataErrorClassifier = fn(&DbError) -> Option<DbError>;

/// The identity [`MetadataErrorClassifier`]: never reclassifies anything.
///
/// The right classifier for a driver whose dictionary errors are already
/// unambiguous.
#[must_use]
pub fn no_error_reclassification(_error: &DbError) -> Option<DbError> {
    None
}

/// A statement a [`MetadataCatalog`] prepared, plus the column contract its
/// result is promised to match and the classifier that corrects a
/// permission-ambiguous error it can produce. See the module documentation.
#[derive(Debug, Clone)]
pub struct PreparedMetadataQuery {
    statement: Statement,
    columns: Vec<ColumnMetadata>,
    classifier: MetadataErrorClassifier,
}

impl PreparedMetadataQuery {
    /// Pairs a statement with the column contract it promises to satisfy and
    /// the classifier that corrects one of its own permission-ambiguous
    /// errors. A driver with nothing to correct passes
    /// [`no_error_reclassification`].
    #[must_use]
    pub const fn new(
        statement: Statement,
        columns: Vec<ColumnMetadata>,
        classifier: MetadataErrorClassifier,
    ) -> Self {
        Self {
            statement,
            columns,
            classifier,
        }
    }

    /// The statement to execute through the ordinary
    /// [`DatabaseConnection::execute`](crate::DatabaseConnection::execute)
    /// path.
    #[must_use]
    pub const fn statement(&self) -> &Statement {
        &self.statement
    }

    /// The declared column contract: names, order and vendor-neutral logical
    /// types the executed statement's result is promised to match.
    #[must_use]
    pub fn columns(&self) -> &[ColumnMetadata] {
        &self.columns
    }

    /// This query's [`MetadataErrorClassifier`], for a caller that wants the
    /// function pointer itself rather than [`PreparedMetadataQuery::reclassify_error`]'s
    /// convenience wrapper.
    #[must_use]
    pub const fn classifier(&self) -> MetadataErrorClassifier {
        self.classifier
    }

    /// Applies this query's classifier to `error`, correcting it if the
    /// permission ambiguity described in the module documentation applies,
    /// and returning it unchanged otherwise.
    #[must_use]
    pub fn reclassify_error(&self, error: DbError) -> DbError {
        (self.classifier)(&error).unwrap_or(error)
    }

    /// Splits this value into its statement, its declared column contract
    /// and its classifier — nothing is silently dropped. A caller that only
    /// needs to correct an execution error, without also taking the
    /// statement and columns apart, can use
    /// [`PreparedMetadataQuery::reclassify_error`] instead.
    #[must_use]
    pub fn into_parts(self) -> (Statement, Vec<ColumnMetadata>, MetadataErrorClassifier) {
        (self.statement, self.columns, self.classifier)
    }
}

/// The declared column contract for [`MetadataRequest::Schemas`].
///
/// `created` is declared nullable even though this project's own Oracle query
/// never reports NULL there: the contract promises what a caller may assume
/// across *any* driver, and not every vendor necessarily tracks a schema's
/// creation time.
#[must_use]
pub fn schemas_columns() -> Vec<ColumnMetadata> {
    vec![
        ColumnMetadata::new("name", SqlType::VARCHAR).with_nullable(false),
        ColumnMetadata::new("created", SqlType::Date).with_nullable(true),
    ]
}

/// The declared column contract for [`MetadataRequest::ObjectsOfKind`].
///
/// `status` is declared nullable: not every object kind, and not every
/// vendor, has a compiled/valid notion to report (`SPEC.md` §16) — a
/// sequence has no compiled state, so Oracle always reports `'VALID'` for
/// one, while a view, a PL/SQL unit or a synonym can genuinely be
/// `'INVALID'`. `created` and `last_modified` are not hedged the same way
/// because every kind Oracle reports here carries both; on Oracle,
/// `last_modified` is `LAST_DDL_TIME`, which moves not only when the
/// object's own definition changes but also on a `GRANT`/`REVOKE` against it
/// or a dependent's recompile — "last touched", not "last redefined".
#[must_use]
pub fn objects_of_kind_columns() -> Vec<ColumnMetadata> {
    vec![
        ColumnMetadata::new("name", SqlType::VARCHAR).with_nullable(false),
        ColumnMetadata::new("status", SqlType::VARCHAR).with_nullable(true),
        ColumnMetadata::new("created", SqlType::Date).with_nullable(false),
        ColumnMetadata::new("last_modified", SqlType::Date).with_nullable(false),
    ]
}

/// The declared column contract for [`MetadataRequest::ColumnsOf`].
///
/// Deliberately excludes a default-value column; see the module
/// documentation. `nullable` here is the target column's own nullability,
/// reported as the text `"YES"`/`"NO"` (the ANSI `information_schema`
/// convention), not whether this *result* column can hold NULL — it never
/// does. `position` is declared non-nullable on the understanding that a
/// driver hides an invisible column rather than reporting a NULL ordinal for
/// one: on Oracle, `ALL_TAB_COLUMNS` carries a NULL `COLUMN_ID` for an
/// `INVISIBLE` column, and the query filters those rows out — the same
/// default behaviour tools like SQL Developer use.
#[must_use]
pub fn columns_of_columns() -> Vec<ColumnMetadata> {
    vec![
        ColumnMetadata::new("position", SqlType::Number).with_nullable(false),
        ColumnMetadata::new("name", SqlType::VARCHAR).with_nullable(false),
        ColumnMetadata::new("type_name", SqlType::VARCHAR).with_nullable(false),
        ColumnMetadata::new("nullable", SqlType::VARCHAR).with_nullable(false),
    ]
}

/// What a driver returns from `DatabaseDriver::metadata_catalog` (`SPEC.md`
/// §16; `phase-1.md` §B4.3). See the module documentation for the whole
/// design.
pub trait MetadataCatalog: Send + Sync {
    /// Prepares `request` as a statement plus its declared column contract
    /// and permission-ambiguity classifier (see
    /// [`PreparedMetadataQuery::reclassify_error`]).
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Unsupported`] if this driver does not implement
    /// `request`'s shape (for [`MetadataRequest::ObjectsOfKind`], a future
    /// [`MetadataObjectKind`] this driver predates); any other [`DbError`] the
    /// driver hits while building the statement.
    fn prepare(&self, request: MetadataRequest) -> crate::error::DbResult<PreparedMetadataQuery>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn name_filter_pattern_escapes_wildcards_and_the_escape_character_itself() {
        assert_eq!(name_filter_pattern(""), "%%");
        assert_eq!(name_filter_pattern("abc"), "%abc%");
        assert_eq!(name_filter_pattern("50%"), "%50\\%%");
        assert_eq!(name_filter_pattern("a_b"), "%a\\_b%");
        assert_eq!(name_filter_pattern("a\\b"), "%a\\\\b%");
        assert_eq!(name_filter_pattern("100%_done\\"), "%100\\%\\_done\\\\%");
        assert_eq!(name_filter_pattern("it's \"fine\""), "%it's \"fine\"%");
        // Non-ASCII text, including Thai, passes through untouched.
        assert_eq!(name_filter_pattern("ข้อมูล"), "%ข้อมูล%");
    }

    #[test]
    fn schemas_request_builders_round_trip() {
        let limit = NonZeroU32::new(50).expect("non-zero");
        let request = MetadataRequest::schemas(limit);
        assert_eq!(
            request,
            MetadataRequest::Schemas {
                name_filter: None,
                limit
            }
        );
        let filtered = request.with_name_filter("hr");
        assert_eq!(
            filtered,
            MetadataRequest::Schemas {
                name_filter: Some("hr".to_owned()),
                limit
            }
        );
    }

    #[test]
    fn objects_of_kind_request_builders_round_trip() {
        let limit = NonZeroU32::new(10).expect("non-zero");
        let request = MetadataRequest::objects_of_kind("HR", MetadataObjectKind::Tables, limit)
            .with_name_filter("emp");
        assert_eq!(
            request,
            MetadataRequest::ObjectsOfKind {
                schema: "HR".to_owned(),
                kind: MetadataObjectKind::Tables,
                name_filter: Some("emp".to_owned()),
                limit,
            }
        );
    }

    #[test]
    #[should_panic(expected = "ColumnsOf")]
    fn with_name_filter_on_columns_of_panics_instead_of_silently_ignoring() {
        let _ = MetadataRequest::columns_of("HR", "EMPLOYEES").with_name_filter("ignored");
    }

    #[test]
    fn declared_contracts_are_ordered_and_named_as_documented() {
        let names = |columns: Vec<ColumnMetadata>| {
            columns
                .iter()
                .map(|c| c.name().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(schemas_columns()), ["name", "created"]);
        assert_eq!(
            names(objects_of_kind_columns()),
            ["name", "status", "created", "last_modified"]
        );
        assert_eq!(
            names(columns_of_columns()),
            ["position", "name", "type_name", "nullable"]
        );
    }

    #[test]
    fn a_driver_that_cannot_prepare_a_request_says_so_with_a_typed_error() {
        struct Noop;
        impl MetadataCatalog for Noop {
            fn prepare(
                &self,
                _request: MetadataRequest,
            ) -> crate::error::DbResult<PreparedMetadataQuery> {
                Err(DbError::unsupported("test stub"))
            }
        }
        // A driver that genuinely cannot prepare a request says so with a
        // typed error rather than a panic or an empty statement.
        let error = Noop
            .prepare(MetadataRequest::schemas(
                NonZeroU32::new(1).expect("non-zero"),
            ))
            .expect_err("the stub never supports anything");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn no_error_reclassification_is_the_identity_classifier() {
        let error = DbError::new(ErrorKind::Other, "boom");
        assert!(no_error_reclassification(&error).is_none());
    }

    fn stub_query(classifier: MetadataErrorClassifier) -> PreparedMetadataQuery {
        PreparedMetadataQuery::new(Statement::new("SELECT 1 FROM DUAL"), Vec::new(), classifier)
    }

    #[test]
    fn reclassify_error_applies_the_carried_classifier_when_it_matches() {
        fn always_permission(_error: &DbError) -> Option<DbError> {
            Some(DbError::new(ErrorKind::Permission, "reclassified"))
        }
        let prepared = stub_query(always_permission);
        let corrected = prepared.reclassify_error(DbError::new(ErrorKind::Syntax, "original"));
        assert_eq!(corrected.kind(), ErrorKind::Permission);
        assert_eq!(corrected.message(), "reclassified");
    }

    #[test]
    fn reclassify_error_leaves_the_error_alone_when_the_classifier_declines() {
        let prepared = stub_query(no_error_reclassification);
        let original = DbError::new(ErrorKind::Syntax, "untouched");
        let after = prepared.reclassify_error(DbError::new(ErrorKind::Syntax, "untouched"));
        assert_eq!(after.kind(), original.kind());
        assert_eq!(after.message(), original.message());
    }
}
