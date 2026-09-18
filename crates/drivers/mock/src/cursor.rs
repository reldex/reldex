//! [`MockCursor`] and [`MockLobStream`]: batched fetch and LOB streaming over
//! a fixed, scripted result set.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use reldex_db_driver_api::{
    BytesColumn, Column, ColumnData, ColumnKind, ColumnMetadata, ConnectionId, Cursor, DbError,
    DbResult, LobKind, LobLocator, LobStream, NullMask, Number, ResultSetId, RowBatch, SqlType,
    TextColumn, Timestamp,
};

use crate::scenario::{ColumnSpec, QueryPlan, Scenario, ScriptValue};

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
}

impl MockLobStream {
    fn new(
        connection_id: ConnectionId,
        kind: LobKind,
        bytes: Vec<u8>,
        scenario: Arc<Scenario>,
        closed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            connection_id,
            kind,
            bytes,
            position: 0,
            scenario,
            closed,
        }
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
            return Err(DbError::connection_closed("LOB stream"));
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

fn build_column(
    spec: &ColumnSpec,
    rows: &[Vec<ScriptValue>],
    index: usize,
    connection_id: ConnectionId,
    scenario: &Arc<Scenario>,
    closed: &Arc<AtomicBool>,
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

fn build_metadata(columns: &[ColumnSpec]) -> Vec<ColumnMetadata> {
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
    scenario: Arc<Scenario>,
    closed: Arc<AtomicBool>,
}

impl MockCursor {
    pub(crate) fn new(
        connection_id: ConnectionId,
        plan: QueryPlan,
        scenario: Arc<Scenario>,
        closed: Arc<AtomicBool>,
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
            scenario,
            closed,
        }
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
        if self.closed.load(Ordering::SeqCst) {
            self.exhausted = true;
            return Err(DbError::connection_closed("cursor"));
        }
        if self.exhausted {
            return Ok(RowBatch::empty());
        }
        self.calls += 1;
        if let Some((n, error)) = &self.fail_on_batch {
            if self.calls == *n {
                self.exhausted = true;
                return Err(error.build());
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

    fn close(self: Box<Self>) -> DbResult<()> {
        self.scenario.record_thread(self.connection_id);
        Ok(())
    }
}
