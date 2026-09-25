//! [`MockCursor`] and [`MockLobStream`]: batched fetch and LOB streaming over
//! a fixed, scripted result set.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use reldex_db_driver_api::{
    BytesColumn, Column, ColumnData, ColumnKind, ColumnMetadata, ConnectionId, Cursor, DbError,
    DbResult, ErrorKind, LobKind, LobLocator, LobStream, NullMask, Number, ResultSetId, RowBatch,
    SessionState, SqlType, TextColumn, Timestamp,
};

use crate::scenario::{ColumnSpec, QueryPlan, Scenario, ScriptValue, TransactionEpoch};

/// A large object served from an in-memory byte buffer, in caller-sized
/// chunks, never splitting a UTF-8 sequence for a character kind (contract
/// requirement on [`LobStream::read_chunk`]).
pub(crate) struct MockLobStream {
    connection_id: ConnectionId,
    kind: LobKind,
    bytes: Vec<u8>,
    position: usize,
    scenario: Arc<Scenario>,
    closed: Arc<AtomicBool>,
    epoch: TransactionEpoch,
    /// Set once a read reported an error. "After any error the stream is
    /// finished" (ADR-0002 D2), so it must keep reporting rather than resume.
    failed: bool,
}

impl MockLobStream {
    pub(crate) fn new(
        connection_id: ConnectionId,
        kind: LobKind,
        bytes: Vec<u8>,
        scenario: Arc<Scenario>,
        closed: Arc<AtomicBool>,
        epoch: TransactionEpoch,
    ) -> Self {
        Self {
            connection_id,
            kind,
            bytes,
            position: 0,
            scenario,
            closed,
            epoch,
            failed: false,
        }
    }
}

impl Drop for MockLobStream {
    /// Releasing a locator is driver work, so it records the thread like every
    /// other driver call.
    ///
    /// A real driver's locator `Drop` talks to the connection (or at least to
    /// the client object that owns it), which is exactly why a `RowBatch` still
    /// holding one may not be dropped off the owning worker thread. Recording
    /// here is what lets a test *see* that happen instead of taking the rule on
    /// trust.
    fn drop(&mut self) {
        self.scenario.record_thread(self.connection_id);
    }
}

impl LobStream for MockLobStream {
    fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    fn kind(&self) -> LobKind {
        self.kind
    }

    fn size_hint(&self) -> Option<u64> {
        Some(self.bytes.len() as u64)
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> DbResult<usize> {
        self.scenario.record_thread(self.connection_id);
        if self.closed.load(Ordering::SeqCst) {
            self.failed = true;
            return Err(DbError::connection_closed("LOB stream"));
        }
        if let Some(error) = self.epoch.check("LOB locator") {
            self.failed = true;
            return Err(error);
        }
        if self.failed {
            return Err(DbError::internal(
                "reldex-driver-mock: LOB stream read after a failure; the only legal action \
                 was to drop it",
            ));
        }
        let remaining = &self.bytes[self.position..];
        if remaining.is_empty() || buf.is_empty() {
            return Ok(0);
        }
        let mut take = remaining.len().min(buf.len());
        if self.kind.is_character() && take < remaining.len() {
            // Back off to the nearest character boundary at or before `take`
            // (a continuation byte has its top two bits `10`). Only give up
            // the guarantee when even one character does not fit, which is
            // outside what the contract promises for `buf.len() < 4`.
            let mut boundary = take;
            while boundary > 0 && (remaining[boundary] & 0xC0) == 0x80 {
                boundary -= 1;
            }
            if boundary > 0 {
                take = boundary;
            }
        }
        buf[..take].copy_from_slice(&remaining[..take]);
        self.position += take;
        Ok(take)
    }
}

fn type_mismatch(row: usize, column: &str, expected: &str) -> DbError {
    DbError::internal(format!(
        "reldex-driver-mock: row {row} column `{column}` is not a {expected} value"
    ))
}

/// Builds one column of a batch from row-major scripted values.
///
/// Shared with [`crate::generated::GeneratedCursor`], which builds the same
/// shape of column from rows it produces on demand rather than from a
/// pre-scripted [`QueryPlan`](crate::scenario::QueryPlan): the conversion from
/// [`ScriptValue`] to [`ColumnData`] is identical either way, and duplicating
/// it would risk the two cursors disagreeing on how a type is represented.
pub(crate) fn build_column(
    spec: &ColumnSpec,
    rows: &[Vec<ScriptValue>],
    index: usize,
    connection_id: ConnectionId,
    scenario: &Arc<Scenario>,
    closed: &Arc<AtomicBool>,
    epoch: &TransactionEpoch,
) -> DbResult<Column> {
    let len = rows.len();
    let mut nulls = NullMask::new(len);
    let name = spec.name();

    let data = match spec.sql_type() {
        SqlType::Number => {
            let mut values = Vec::with_capacity(len);
            for (row_index, row) in rows.iter().enumerate() {
                match &row[index] {
                    ScriptValue::Null => {
                        nulls.set_null(row_index)?;
                        values.push(Number::ZERO);
                    }
                    ScriptValue::Number(value) => values.push(*value),
                    _ => return Err(type_mismatch(row_index, name, "Number")),
                }
            }
            ColumnData::Number(values)
        }
        SqlType::Text { .. } => {
            let mut values = TextColumn::new();
            for (row_index, row) in rows.iter().enumerate() {
                match &row[index] {
                    ScriptValue::Null => {
                        nulls.set_null(row_index)?;
                        values.push_null_placeholder();
                    }
                    ScriptValue::Text(text) => values.push(text),
                    _ => return Err(type_mismatch(row_index, name, "Text")),
                }
            }
            ColumnData::Text(values)
        }
        SqlType::Raw => {
            let mut values = BytesColumn::new();
            for (row_index, row) in rows.iter().enumerate() {
                match &row[index] {
                    ScriptValue::Null => {
                        nulls.set_null(row_index)?;
                        values.push_null_placeholder();
                    }
                    ScriptValue::Bytes(bytes) => values.push(bytes),
                    _ => return Err(type_mismatch(row_index, name, "Bytes")),
                }
            }
            ColumnData::Bytes(values)
        }
        SqlType::Date | SqlType::Timestamp | SqlType::TimestampWithTimeZone => {
            let placeholder = Timestamp::new(1, 1, 1, 0, 0, 0)
                .expect("1-01-01 00:00:00 is a valid placeholder timestamp");
            let mut values = Vec::with_capacity(len);
            for (row_index, row) in rows.iter().enumerate() {
                match &row[index] {
                    ScriptValue::Null => {
                        nulls.set_null(row_index)?;
                        values.push(placeholder);
                    }
                    ScriptValue::Timestamp(value) => values.push(*value),
                    _ => return Err(type_mismatch(row_index, name, "Timestamp")),
                }
            }
            ColumnData::Timestamp(values)
        }
        SqlType::Unsupported => {
            let mut values = TextColumn::new();
            for (row_index, row) in rows.iter().enumerate() {
                match &row[index] {
                    ScriptValue::Null => {
                        nulls.set_null(row_index)?;
                        values.push_null_placeholder();
                    }
                    ScriptValue::Unsupported(text) => values.push(text),
                    _ => return Err(type_mismatch(row_index, name, "Unsupported")),
                }
            }
            ColumnData::Unsupported(values)
        }
        SqlType::CharacterLob { .. } | SqlType::BinaryLob => {
            let mut values = Vec::with_capacity(len);
            for (row_index, row) in rows.iter().enumerate() {
                match &row[index] {
                    ScriptValue::Null => {
                        nulls.set_null(row_index)?;
                        values.push(None);
                    }
                    ScriptValue::Lob { kind, bytes } => {
                        let stream = MockLobStream::new(
                            connection_id,
                            *kind,
                            bytes.clone(),
                            Arc::clone(scenario),
                            Arc::clone(closed),
                            epoch.clone(),
                        );
                        values.push(Some(LobLocator::new(Box::new(stream))));
                    }
                    _ => return Err(type_mismatch(row_index, name, "Lob")),
                }
            }
            ColumnData::Lob(values)
        }
        other => {
            return Err(DbError::internal(format!(
                "reldex-driver-mock: column `{name}` declares unsupported-by-the-mock type {other}"
            )));
        }
    };
    debug_assert_eq!(data.kind(), column_kind_for(spec.sql_type()));
    Column::new(data, nulls)
}

const fn column_kind_for(sql_type: SqlType) -> ColumnKind {
    match sql_type {
        SqlType::Number => ColumnKind::Number,
        SqlType::Text { .. } => ColumnKind::Text,
        SqlType::Raw => ColumnKind::Bytes,
        SqlType::Date | SqlType::Timestamp | SqlType::TimestampWithTimeZone => {
            ColumnKind::Timestamp
        }
        SqlType::Unsupported => ColumnKind::Unsupported,
        SqlType::CharacterLob { .. } | SqlType::BinaryLob => ColumnKind::Lob,
        _ => ColumnKind::Unsupported,
    }
}

/// Builds column metadata from column specs. Shared with
/// [`crate::generated::GeneratedCursor`]; see [`build_column`].
pub(crate) fn build_metadata(columns: &[ColumnSpec]) -> Vec<ColumnMetadata> {
    columns
        .iter()
        .map(|column| {
            let mut metadata = ColumnMetadata::new(column.name(), column.sql_type());
            if let Some(native_type_name) = column.native_type_name() {
                metadata = metadata.with_native_type_name(native_type_name);
            }
            metadata
        })
        .collect()
}

/// A forward-only cursor over a scripted, fixed result set.
///
/// The whole result set is materialized in the scenario's [`QueryPlan`]
/// ahead of time (this is a test double, not a streaming driver); what
/// `fetch_batch` actually exercises is the *batching, exhaustion and fault
/// injection* contract, not lazy production of rows.
pub struct MockCursor {
    id: ResultSetId,
    connection_id: ConnectionId,
    columns: Vec<ColumnSpec>,
    metadata: Vec<ColumnMetadata>,
    rows: Vec<Vec<ScriptValue>>,
    position: usize,
    fail_on_batch: Option<(usize, crate::scenario::ScriptedError)>,
    calls: usize,
    exhausted: bool,
    /// The error this cursor reported, kept so every later `fetch_batch`
    /// reports it again. "After any error the only legal call is `close()`"
    /// (ADR-0002 D2) — and a driver enforces that by *reporting*, never by
    /// returning an empty batch that reads as a complete result.
    failed: Option<(ErrorKind, String, SessionState)>,
    scenario: Arc<Scenario>,
    closed: Arc<AtomicBool>,
    epoch: TransactionEpoch,
}

impl MockCursor {
    pub(crate) fn new(
        connection_id: ConnectionId,
        plan: QueryPlan,
        scenario: Arc<Scenario>,
        closed: Arc<AtomicBool>,
        epoch: TransactionEpoch,
    ) -> Self {
        let metadata = build_metadata(&plan.columns);
        Self {
            id: ResultSetId::allocate(),
            connection_id,
            columns: plan.columns,
            metadata,
            rows: plan.rows,
            position: 0,
            fail_on_batch: plan.fail_on_batch,
            calls: 0,
            exhausted: false,
            failed: None,
            scenario,
            closed,
            epoch,
        }
    }

    /// Remembers enough of `error` to report it again, and hands it back.
    /// ([`DbError`] is not `Clone`, so the parts are kept rather than the
    /// value.)
    fn fail(&mut self, error: DbError) -> DbError {
        self.exhausted = true;
        self.failed = Some((
            error.kind(),
            error.message().to_owned(),
            error.session_state(),
        ));
        error
    }

    fn repeat_failure(&self) -> Option<DbError> {
        self.failed.as_ref().map(|(kind, message, session_state)| {
            DbError::new(*kind, message.clone()).with_session_state(*session_state)
        })
    }
}

impl Drop for MockCursor {
    /// Dropping a cursor is driver work too; see `MockLobStream`'s `Drop`.
    fn drop(&mut self) {
        self.scenario.record_thread(self.connection_id);
    }
}

impl Cursor for MockCursor {
    fn id(&self) -> ResultSetId {
        self.id
    }

    fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    fn columns(&self) -> &[ColumnMetadata] {
        &self.metadata
    }

    fn fetch_batch(&mut self, max_rows: std::num::NonZeroUsize) -> DbResult<RowBatch> {
        self.scenario.record_thread(self.connection_id);
        // A cursor that already failed keeps reporting. Returning the empty
        // batch that means "exhausted" would turn a partial result into one
        // that looks complete, which is the failure `SPEC.md` §2 ranks worst.
        if let Some(error) = self.repeat_failure() {
            return Err(error);
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(self.fail(DbError::connection_closed("cursor")));
        }
        if let Some(error) = self.epoch.check("cursor") {
            return Err(self.fail(error));
        }
        if self.exhausted {
            return Ok(RowBatch::empty());
        }
        self.calls += 1;
        if let Some((n, error)) = &self.fail_on_batch {
            if self.calls == *n {
                let error = error.build();
                return Err(self.fail(error));
            }
        }
        if self.position >= self.rows.len() {
            self.exhausted = true;
            return Ok(RowBatch::empty());
        }
        let end = (self.position + max_rows.get()).min(self.rows.len());
        let slice = &self.rows[self.position..end];
        let columns = (0..self.columns.len())
            .map(|index| {
                build_column(
                    &self.columns[index],
                    slice,
                    index,
                    self.connection_id,
                    &self.scenario,
                    &self.closed,
                    &self.epoch,
                )
            })
            .collect::<DbResult<Vec<_>>>()?;
        self.position = end;
        if self.position >= self.rows.len() {
            self.exhausted = true;
        }
        RowBatch::new(columns)
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Releases the cursor.
    ///
    /// Always `Ok(())`, including after the connection was closed: per the
    /// reconciled rule in ADR-0002 D2, `close` is idempotent and reports
    /// success when there is nothing left to release. Every *other* method on a
    /// handle whose connection is gone reports
    /// [`DbError::connection_closed`].
    fn close(self: Box<Self>) -> DbResult<()> {
        self.scenario.record_thread(self.connection_id);
        self.scenario.record_cursor_closed();
        Ok(())
    }
}
