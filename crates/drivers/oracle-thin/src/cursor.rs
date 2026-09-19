//! Cursors and row-to-column batch conversion.

use std::num::NonZeroUsize;

use reldex_db_driver_api::{
    ColumnMetadata, ConnectionId, Cursor, DbError, DbResult, ErrorKind, ResultSetId, RowBatch,
    SqlType,
};

use crate::conn::{Closed, ResultSetGuard};
use crate::value::{ColumnBuilder, ColumnPlan, column_metadata, native_type_name, plan_for};

/// The refusal a `TIMESTAMP WITH TIME ZONE` column earns by default.
///
/// See [`OracleCursor::new`] and the crate documentation ("Known limitations").
fn timestamp_with_time_zone_is_refused(column: &str) -> DbError {
    DbError::new(
        ErrorKind::Unsupported,
        format!(
            "column \"{column}\" is a TIMESTAMP WITH TIME ZONE, which this driver \
             refuses to fetch on oracledb 26.0.0-beta.3: a value whose zone is a \
             named region (for example TIMESTAMP '2026-01-01 00:00:00 Asia/Bangkok') \
             reaches an unimplemented branch in the upstream decoder and takes the \
             whole process down with it, and a region cannot be told from a plain \
             offset before the value is decoded. Cast the column in the statement \
             (TO_CHAR(c, 'YYYY-MM-DD HH24:MI:SS TZR')) to read it, or set the \
             connection extension \"{allow}\" to accept the risk",
            allow = crate::conn::EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE,
        ),
    )
}

/// The refusal a column this upstream version cannot decode at all earns.
///
/// Unlike `ROWID` or an `INTERVAL`, there is no text to fall back to: upstream's
/// `DbValue::from_response` has no branch for these types and returns
/// `UnsupportedDbType` while deserializing the row. Reporting that from
/// `fetch_batch` would fail a result set **after** earlier batches had already
/// been handed to the caller, which is worse than not starting: ADR-0002 M1 asks
/// for a clean answer, and at describe time there still is one.
fn undecodable_column_is_refused(column: &str, native: &str) -> DbError {
    DbError::new(
        ErrorKind::Unsupported,
        format!(
            "column \"{column}\" is of type {native}, which oracledb 26.0.0-beta.3 cannot \
             decode into any value this driver can render, so the whole statement is \
             refused here rather than part-way through fetching it. Convert the column in \
             the statement — TO_CLOB, JSON_SERIALIZE, XMLSERIALIZE or the type's own \
             accessor — to read it as text"
        ),
    )
}

/// A forward-only, batched result set over an `oracledb` cursor.
pub(crate) struct OracleCursor {
    id: ResultSetId,
    connection: ConnectionId,
    columns: Vec<ColumnMetadata>,
    plans: Vec<ColumnPlan>,
    inner: oracledb::Cursor,
    closed: Closed,
    /// Set after any failure. The contract then allows only `close`, and the
    /// driver enforces that by reporting rather than by trusting the caller
    /// (ADR-0002 M4).
    failed: bool,
    exhausted: bool,
    /// Keeps the connection's open-result-set count accurate for as long as this
    /// cursor exists; see `conn::OpenResultSets`.
    _open: ResultSetGuard,
}

impl OracleCursor {
    /// Wraps a cursor, rejecting select lists this contract cannot express or
    /// this upstream version cannot decode safely.
    ///
    /// This runs on the **describe**, before a single value has been decoded:
    /// `conn::execute_query` asks `oracledb` for zero prefetched rows so the
    /// execute round trip returns column metadata only, and a nested cursor
    /// never prefetches at all. That is what makes the two refusals below
    /// possible rather than theoretical.
    ///
    /// - A cursor-typed *result column* (`SELECT CURSOR(SELECT …) FROM …`) is
    ///   out of scope for V1 and must be reported as [`ErrorKind::Unsupported`]
    ///   rather than silently dropped (ADR-0002 lead decision 3).
    /// - A `TIMESTAMP WITH TIME ZONE` column is refused unless the caller opted
    ///   in, because decoding one whose zone is a named region aborts the
    ///   process (upstream U-3 compounded by U-4) and the two forms cannot be
    ///   told apart before the decode happens.
    /// - A column of a type upstream cannot decode at all — `JSON`, `XMLTYPE`,
    ///   `VECTOR`, an object type, `BFILE` — is refused here rather than from
    ///   `fetch_batch`, so a result set never fails after part of it has been
    ///   delivered.
    ///
    /// Every type the contract cannot express but upstream *can* decode —
    /// `ROWID`, both `INTERVAL`s, `TIMESTAMP WITH LOCAL TIME ZONE` — becomes a
    /// text rendering instead, so one odd column of those kinds never hides a
    /// whole table.
    pub(crate) fn new(
        inner: oracledb::Cursor,
        connection: ConnectionId,
        closed: Closed,
        allow_timestamp_with_time_zone: bool,
        open: ResultSetGuard,
    ) -> DbResult<Self> {
        let mut columns = Vec::with_capacity(inner.columns().len());
        let mut plans = Vec::with_capacity(inner.columns().len());
        for meta in inner.columns() {
            let metadata = column_metadata(meta);
            if metadata.sql_type() == SqlType::Cursor {
                return Err(DbError::new(
                    ErrorKind::Unsupported,
                    format!(
                        "column \"{}\" is a nested cursor in the select list, \
                         which this driver does not support",
                        metadata.name()
                    ),
                ));
            }
            if metadata.sql_type() == SqlType::TimestampWithTimeZone
                && !allow_timestamp_with_time_zone
            {
                return Err(timestamp_with_time_zone_is_refused(metadata.name()));
            }
            let plan = plan_for(meta.db_type()).1;
            if plan == ColumnPlan::Unsupported {
                return Err(undecodable_column_is_refused(
                    metadata.name(),
                    native_type_name(meta.db_type()),
                ));
            }
            plans.push(plan);
            columns.push(metadata);
        }
        Ok(Self {
            id: ResultSetId::allocate(),
            connection,
            columns,
            plans,
            inner,
            closed,
            failed: false,
            exhausted: false,
            _open: open,
        })
    }

    fn guard(&self) -> DbResult<()> {
        if self.closed.is_closed() {
            return Err(DbError::connection_closed("cursor"));
        }
        if self.failed {
            return Err(DbError::internal(
                "cursor used after a failed fetch; the only legal call was close",
            ));
        }
        Ok(())
    }
}

impl Cursor for OracleCursor {
    fn id(&self) -> ResultSetId {
        self.id
    }

    fn connection_id(&self) -> ConnectionId {
        self.connection
    }

    fn columns(&self) -> &[ColumnMetadata] {
        &self.columns
    }

    fn fetch_batch(&mut self, max_rows: NonZeroUsize) -> DbResult<RowBatch> {
        self.guard()?;
        if self.exhausted {
            return Ok(RowBatch::empty());
        }

        let capacity = max_rows.get().min(4096);
        let mut builders: Vec<ColumnBuilder> = self
            .plans
            .iter()
            .map(|plan| ColumnBuilder::new(*plan, capacity))
            .collect();

        let mut rows = 0_usize;
        while rows < max_rows.get() {
            let Some(row) = self.inner.next() else {
                self.exhausted = true;
                break;
            };
            let mut row = match row {
                Ok(row) => row,
                Err(error) => {
                    self.failed = true;
                    return Err(crate::error::map(&error));
                }
            };
            for (index, builder) in builders.iter_mut().enumerate() {
                if let Err(error) = builder.push(&mut row, index, self.connection, &self.closed) {
                    self.failed = true;
                    return Err(error);
                }
            }
            rows += 1;
        }

        if rows == 0 {
            return Ok(RowBatch::empty());
        }

        let mut columns = Vec::with_capacity(builders.len());
        for builder in builders {
            columns.push(builder.finish()?);
        }
        RowBatch::new(columns)
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    fn close(self: Box<Self>) -> DbResult<()> {
        // `oracledb` releases the server-side cursor when its own `Cursor` (and
        // the statement holder inside it) is dropped, piggybacked on the next
        // round trip. There is nothing to report and nothing that can fail.
        drop(self);
        Ok(())
    }
}
