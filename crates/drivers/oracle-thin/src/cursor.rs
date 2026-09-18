//! Cursors and row-to-column batch conversion.

use std::num::NonZeroUsize;

use reldex_db_driver_api::{
    ColumnMetadata, ConnectionId, Cursor, DbError, DbResult, ErrorKind, ResultSetId, RowBatch,
    SqlType,
};

use crate::conn::Closed;
use crate::value::{ColumnBuilder, ColumnPlan, column_metadata, plan_for};

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
}

impl OracleCursor {
    /// Wraps a cursor, rejecting select lists this contract cannot express.
    ///
    /// A cursor-typed *result column* (`SELECT CURSOR(SELECT …) FROM …`) is out
    /// of scope for V1 and must be reported as
    /// [`ErrorKind::Unsupported`] rather than silently dropped
    /// (ADR-0002 lead decision 3). Every other unrepresentable type becomes a
    /// text rendering instead, so one odd column never hides a whole table.
    pub(crate) fn new(
        inner: oracledb::Cursor,
        connection: ConnectionId,
        closed: Closed,
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
            plans.push(plan_for(meta.db_type()).1);
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
