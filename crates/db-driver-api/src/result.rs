//! Column-oriented result batches, cursors and execution outcomes
//! (ADR-0002 D6).
//!
//! The fetch path is `Cursor -> RowBatch -> Result Store` (`SPEC.md` §12).
//! Batches are column-oriented: a column has one type, so the type tag is stored
//! once per column instead of once per cell, NULLs are a bitmask instead of a
//! per-cell `Option`, and text and byte columns keep one contiguous buffer plus
//! offsets — so a thousand-row `VARCHAR2` batch is two allocations rather than a
//! thousand. Random access by `(row, column)` stays `O(1)`.
//!
//! No Apache Arrow type appears here. Arrow remains an internal, benchmark-gated
//! option for the Result Store (`SPEC.md` §12).

use std::fmt;
use std::num::NonZeroUsize;

use crate::error::{DbError, DbResult, NativeError, SqlPosition};
use crate::ids::ResultSetId;
use crate::types::ColumnMetadata;
use crate::value::{LobLocator, Number, Timestamp, Value, ValueRef};

/// A reasonable default batch size for interactive fetching.
///
/// It is a starting point, not a tuned value: `phase-0.md` "Measurements"
/// requires fetch throughput and memory to be measured before any batch-size
/// claim is made.
pub const DEFAULT_FETCH_ROWS: NonZeroUsize = match NonZeroUsize::new(1000) {
    Some(rows) => rows,
    None => unreachable!(),
};

/// A bitmask recording which rows of a column are SQL NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NullMask {
    bits: Vec<u64>,
    len: usize,
}

impl NullMask {
    /// A mask of `len` rows with nothing marked NULL.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self {
            bits: vec![0; len.div_ceil(64)],
            len,
        }
    }

    /// Marks a row as SQL NULL. Out-of-range indices are ignored.
    pub fn set_null(&mut self, row: usize) {
        if row < self.len {
            self.bits[row / 64] |= 1_u64 << (row % 64);
        }
    }

    /// Whether the row is SQL NULL. Out-of-range indices report `false`.
    #[must_use]
    pub fn is_null(&self, row: usize) -> bool {
        row < self.len && (self.bits[row / 64] >> (row % 64)) & 1 == 1
    }

    /// Number of rows the mask covers.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the mask covers no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many rows are NULL.
    #[must_use]
    pub fn null_count(&self) -> usize {
        (0..self.len).filter(|row| self.is_null(*row)).count()
    }
}

/// Variable-length UTF-8 values stored as one buffer plus offsets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextColumn {
    buffer: String,
    offsets: Vec<usize>,
}

impl TextColumn {
    /// An empty column.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty column with room for `rows` values totalling `bytes`.
    #[must_use]
    pub fn with_capacity(rows: usize, bytes: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        Self {
            buffer: String::with_capacity(bytes),
            offsets,
        }
    }

    /// Appends a value.
    pub fn push(&mut self, text: &str) {
        if self.offsets.is_empty() {
            self.offsets.push(0);
        }
        self.buffer.push_str(text);
        self.offsets.push(self.buffer.len());
    }

    /// Appends a placeholder for a NULL row.
    ///
    /// The mask, not the buffer, records nullness; this only keeps the offsets
    /// aligned with the row numbers.
    pub fn push_null_placeholder(&mut self) {
        self.push("");
    }

    /// The value at `row`, if the row exists.
    #[must_use]
    pub fn get(&self, row: usize) -> Option<&str> {
        let start = *self.offsets.get(row)?;
        let end = *self.offsets.get(row + 1)?;
        self.buffer.get(start..end)
    }

    /// How many values the column holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Whether the column holds no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total bytes of character data held.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.buffer.len()
    }
}

/// Variable-length byte values stored as one buffer plus offsets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BytesColumn {
    buffer: Vec<u8>,
    offsets: Vec<usize>,
}

impl BytesColumn {
    /// An empty column.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty column with room for `rows` values totalling `bytes`.
    #[must_use]
    pub fn with_capacity(rows: usize, bytes: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        Self {
            buffer: Vec::with_capacity(bytes),
            offsets,
        }
    }

    /// Appends a value.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.offsets.is_empty() {
            self.offsets.push(0);
        }
        self.buffer.extend_from_slice(bytes);
        self.offsets.push(self.buffer.len());
    }

    /// Appends a placeholder for a NULL row.
    pub fn push_null_placeholder(&mut self) {
        self.push(&[]);
    }

    /// The value at `row`, if the row exists.
    #[must_use]
    pub fn get(&self, row: usize) -> Option<&[u8]> {
        let start = *self.offsets.get(row)?;
        let end = *self.offsets.get(row + 1)?;
        self.buffer.get(start..end)
    }

    /// How many values the column holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Whether the column holds no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total bytes of data held.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.buffer.len()
    }
}

/// The storage family of a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnKind {
    /// Booleans.
    Boolean,
    /// Exact decimals.
    Number,
    /// 32-bit binary floats.
    Float,
    /// 64-bit binary floats.
    Double,
    /// UTF-8 text.
    Text,
    /// Raw bytes.
    Bytes,
    /// Dates and timestamps.
    Timestamp,
    /// JSON text.
    Json,
    /// Unread large objects.
    Lob,
}

/// One column's values, laid out contiguously.
///
/// Values at NULL rows are unspecified placeholders; read cells through
/// [`Column::value`], which consults the mask first.
#[derive(Debug)]
pub enum ColumnData {
    /// Booleans.
    Boolean(Vec<bool>),
    /// Exact decimals.
    Number(Vec<Number>),
    /// 32-bit binary floats.
    Float(Vec<f32>),
    /// 64-bit binary floats.
    Double(Vec<f64>),
    /// UTF-8 text.
    Text(TextColumn),
    /// Raw bytes.
    Bytes(BytesColumn),
    /// Dates and timestamps.
    Timestamp(Vec<Timestamp>),
    /// JSON text.
    Json(TextColumn),
    /// Large objects that have not been read. `None` means the locator has
    /// already been taken out of the batch.
    Lob(Vec<Option<LobLocator>>),
}

impl ColumnData {
    /// How many rows the column holds.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Boolean(values) => values.len(),
            Self::Number(values) => values.len(),
            Self::Float(values) => values.len(),
            Self::Double(values) => values.len(),
            Self::Text(values) | Self::Json(values) => values.len(),
            Self::Bytes(values) => values.len(),
            Self::Timestamp(values) => values.len(),
            Self::Lob(values) => values.len(),
        }
    }

    /// Whether the column holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The storage family.
    #[must_use]
    pub const fn kind(&self) -> ColumnKind {
        match self {
            Self::Boolean(_) => ColumnKind::Boolean,
            Self::Number(_) => ColumnKind::Number,
            Self::Float(_) => ColumnKind::Float,
            Self::Double(_) => ColumnKind::Double,
            Self::Text(_) => ColumnKind::Text,
            Self::Bytes(_) => ColumnKind::Bytes,
            Self::Timestamp(_) => ColumnKind::Timestamp,
            Self::Json(_) => ColumnKind::Json,
            Self::Lob(_) => ColumnKind::Lob,
        }
    }
}

/// One column of a fetched batch: its values plus its NULL mask.
#[derive(Debug)]
pub struct Column {
    data: ColumnData,
    nulls: NullMask,
}

impl Column {
    /// Builds a column from values and a matching NULL mask.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ErrorKind::DriverInternal`] if the mask length does not match
    /// the value count — a driver bug the contract refuses to propagate.
    pub fn new(data: ColumnData, nulls: NullMask) -> DbResult<Self> {
        if data.len() != nulls.len() {
            return Err(DbError::internal(format!(
                "column has {} values but a NULL mask of {} rows",
                data.len(),
                nulls.len()
            )));
        }
        Ok(Self { data, nulls })
    }

    /// Builds a column in which no row is NULL.
    #[must_use]
    pub fn not_null(data: ColumnData) -> Self {
        let nulls = NullMask::new(data.len());
        Self { data, nulls }
    }

    /// How many rows the column holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the column holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The storage family.
    #[must_use]
    pub const fn kind(&self) -> ColumnKind {
        self.data.kind()
    }

    /// Whether the row is SQL NULL.
    #[must_use]
    pub fn is_null(&self, row: usize) -> bool {
        self.nulls.is_null(row)
    }

    /// How many rows are NULL.
    #[must_use]
    pub fn null_count(&self) -> usize {
        self.nulls.null_count()
    }

    /// Borrows one cell, or `None` if `row` is out of range.
    #[must_use]
    pub fn value(&self, row: usize) -> Option<ValueRef<'_>> {
        if row >= self.len() {
            return None;
        }
        if self.nulls.is_null(row) {
            return Some(ValueRef::Null);
        }
        let value = match &self.data {
            ColumnData::Boolean(values) => ValueRef::Boolean(*values.get(row)?),
            ColumnData::Number(values) => ValueRef::Number(values.get(row)?),
            ColumnData::Float(values) => ValueRef::Float(*values.get(row)?),
            ColumnData::Double(values) => ValueRef::Double(*values.get(row)?),
            ColumnData::Text(values) => ValueRef::Text(values.get(row)?),
            ColumnData::Json(values) => ValueRef::Json(values.get(row)?),
            ColumnData::Bytes(values) => ValueRef::Bytes(values.get(row)?),
            ColumnData::Timestamp(values) => ValueRef::Timestamp(*values.get(row)?),
            ColumnData::Lob(values) => match values.get(row)? {
                Some(locator) => ValueRef::Lob(locator),
                None => ValueRef::Null,
            },
        };
        Some(value)
    }

    /// Takes the large-object locator at `row` so it can be read.
    ///
    /// Reading a LOB needs ownership, so the locator leaves the batch and the
    /// row becomes NULL. Returns `None` for any other column kind, an
    /// out-of-range row, or a locator that was already taken.
    pub fn take_lob(&mut self, row: usize) -> Option<LobLocator> {
        let ColumnData::Lob(values) = &mut self.data else {
            return None;
        };
        let taken = values.get_mut(row)?.take();
        if taken.is_some() {
            self.nulls.set_null(row);
        }
        taken
    }
}

/// A batch of rows fetched from a cursor.
#[derive(Debug)]
pub struct RowBatch {
    columns: Vec<Column>,
    row_count: usize,
}

impl RowBatch {
    /// A batch with no rows and no columns.
    ///
    /// This is what [`Cursor::fetch_batch`] returns once a result is exhausted.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
            row_count: 0,
        }
    }

    /// Builds a batch from equally sized columns.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ErrorKind::DriverInternal`] if the columns disagree about the
    /// row count.
    pub fn new(columns: Vec<Column>) -> DbResult<Self> {
        let row_count = columns.first().map_or(0, Column::len);
        if let Some(bad) = columns.iter().position(|column| column.len() != row_count) {
            return Err(DbError::internal(format!(
                "column {bad} has {} rows but the batch has {row_count}",
                columns[bad].len()
            )));
        }
        Ok(Self { columns, row_count })
    }

    /// How many rows the batch holds.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// How many columns the batch holds.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Whether the batch holds no rows.
    ///
    /// An empty batch from [`Cursor::fetch_batch`] means the result is
    /// exhausted.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// All columns.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// One column.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    /// One column, mutably, for taking LOB locators out of it.
    pub fn column_mut(&mut self, index: usize) -> Option<&mut Column> {
        self.columns.get_mut(index)
    }

    /// Borrows one cell.
    #[must_use]
    pub fn value(&self, row: usize, column: usize) -> Option<ValueRef<'_>> {
        self.columns.get(column)?.value(row)
    }
}

/// A forward-only, batched result set.
///
/// A cursor is owned by the session's worker thread; `&mut self` makes that a
/// compile-time fact rather than a convention.
pub trait Cursor: Send {
    /// This result set's identifier.
    fn id(&self) -> ResultSetId;

    /// Column descriptions, in select-list order.
    fn columns(&self) -> &[ColumnMetadata];

    /// Fetches up to `max_rows` more rows.
    ///
    /// A batch with no rows means the result is exhausted; a driver must not
    /// return an empty batch while more rows remain.
    ///
    /// # Errors
    ///
    /// Any [`DbError`]. A cancelled fetch returns [`crate::ErrorKind::Cancelled`].
    fn fetch_batch(&mut self, max_rows: NonZeroUsize) -> DbResult<RowBatch>;

    /// Whether the driver already knows the result is exhausted.
    fn is_exhausted(&self) -> bool;

    /// Releases the cursor's server-side and client-side resources.
    ///
    /// # Errors
    ///
    /// Any [`DbError`] the release produced. Dropping a cursor without calling
    /// this is allowed; the driver must then release resources on drop and
    /// swallow the error.
    fn close(self: Box<Self>) -> DbResult<()>;
}

/// Why a statement produced a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WarningKind {
    /// A PL/SQL object was created but did not compile cleanly. `SPEC.md` §24.14
    /// requires these to reach the user.
    CompiledWithErrors,
    /// The server reported something worth showing that is not an error.
    Informational,
}

/// A non-fatal message produced by a statement.
#[derive(Debug, Clone)]
pub struct Warning {
    kind: WarningKind,
    message: Box<str>,
    native: Option<NativeError>,
    position: Option<SqlPosition>,
}

impl Warning {
    /// Builds a warning.
    #[must_use]
    pub fn new(kind: WarningKind, message: impl Into<Box<str>>) -> Self {
        Self {
            kind,
            message: message.into(),
            native: None,
            position: None,
        }
    }

    /// Attaches the preserved native error.
    #[must_use]
    pub fn with_native(mut self, native: NativeError) -> Self {
        self.native = Some(native);
        self
    }

    /// Attaches the position in the statement text.
    #[must_use]
    pub fn with_position(mut self, position: SqlPosition) -> Self {
        self.position = Some(position);
        self
    }

    /// Why the warning was produced.
    #[must_use]
    pub const fn kind(&self) -> WarningKind {
        self.kind
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The preserved native error, if any.
    #[must_use]
    pub fn native(&self) -> Option<&NativeError> {
        self.native.as_ref()
    }

    /// The position in the statement text, if any.
    #[must_use]
    pub fn position(&self) -> Option<&SqlPosition> {
        self.position.as_ref()
    }
}

/// Values the server wrote back through OUT and IN OUT binds.
///
/// The shape mirrors the statement's [`crate::Binds`].
#[derive(Debug)]
pub enum OutValues {
    /// The statement had no output binds.
    None,
    /// Index-aligned with the statement's positional binds; `None` marks a bind
    /// that was input-only.
    Positional(Vec<Option<Value>>),
    /// Named output binds, without their placeholder prefix.
    Named(Vec<(Box<str>, Value)>),
}

impl OutValues {
    /// The value written back for a positional bind.
    #[must_use]
    pub fn positional(&self, index: usize) -> Option<&Value> {
        match self {
            Self::Positional(values) => values.get(index)?.as_ref(),
            Self::None | Self::Named(_) => None,
        }
    }

    /// The value written back for a named bind.
    #[must_use]
    pub fn named(&self, name: &str) -> Option<&Value> {
        match self {
            Self::Named(values) => values
                .iter()
                .find(|(key, _)| &**key == name)
                .map(|(_, value)| value),
            Self::None | Self::Positional(_) => None,
        }
    }

    /// Whether anything was written back.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Positional(values) => values.iter().all(Option::is_none),
            Self::Named(values) => values.is_empty(),
        }
    }
}

/// Everything one `execute` produced.
///
/// There is one entry point for execution because a worksheet cannot know
/// whether arbitrary user text returns rows, and the core must not parse SQL to
/// find out (ADR-0002 D6).
pub struct ExecutionOutcome {
    cursor: Option<Box<dyn Cursor>>,
    rows_affected: Option<u64>,
    out_values: OutValues,
    implicit_results: Vec<Box<dyn Cursor>>,
    warnings: Vec<Warning>,
}

impl ExecutionOutcome {
    /// An outcome with no result set, no rows affected and no output.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cursor: None,
            rows_affected: None,
            out_values: OutValues::None,
            implicit_results: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Attaches the result set the statement produced.
    #[must_use]
    pub fn with_cursor(mut self, cursor: Box<dyn Cursor>) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// Records how many rows the statement changed.
    #[must_use]
    pub fn with_rows_affected(mut self, rows_affected: u64) -> Self {
        self.rows_affected = Some(rows_affected);
        self
    }

    /// Attaches the values written back through output binds.
    #[must_use]
    pub fn with_out_values(mut self, out_values: OutValues) -> Self {
        self.out_values = out_values;
        self
    }

    /// Attaches result sets the statement returned implicitly.
    #[must_use]
    pub fn with_implicit_results(mut self, implicit_results: Vec<Box<dyn Cursor>>) -> Self {
        self.implicit_results = implicit_results;
        self
    }

    /// Attaches non-fatal messages.
    #[must_use]
    pub fn with_warnings(mut self, warnings: Vec<Warning>) -> Self {
        self.warnings = warnings;
        self
    }

    /// Whether the statement produced a result set.
    #[must_use]
    pub fn has_cursor(&self) -> bool {
        self.cursor.is_some()
    }

    /// Takes the result set out of the outcome.
    pub fn take_cursor(&mut self) -> Option<Box<dyn Cursor>> {
        self.cursor.take()
    }

    /// Takes the implicitly returned result sets out of the outcome.
    pub fn take_implicit_results(&mut self) -> Vec<Box<dyn Cursor>> {
        std::mem::take(&mut self.implicit_results)
    }

    /// How many rows the statement changed, if the driver reported it.
    #[must_use]
    pub const fn rows_affected(&self) -> Option<u64> {
        self.rows_affected
    }

    /// The values written back through output binds.
    #[must_use]
    pub const fn out_values(&self) -> &OutValues {
        &self.out_values
    }

    /// Non-fatal messages the statement produced.
    #[must_use]
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// Whether any warning reports a PL/SQL object that compiled with errors.
    #[must_use]
    pub fn compiled_with_errors(&self) -> bool {
        self.warnings
            .iter()
            .any(|warning| warning.kind() == WarningKind::CompiledWithErrors)
    }
}

impl Default for ExecutionOutcome {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ExecutionOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionOutcome")
            .field("has_cursor", &self.cursor.is_some())
            .field("rows_affected", &self.rows_affected)
            .field("out_values", &self.out_values)
            .field("implicit_results", &self.implicit_results.len())
            .field("warnings", &self.warnings)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use crate::value::{LobKind, LobStream};

    fn text_column(values: &[Option<&str>]) -> Column {
        let mut data = TextColumn::with_capacity(values.len(), 32);
        let mut nulls = NullMask::new(values.len());
        for (row, value) in values.iter().enumerate() {
            match value {
                Some(text) => data.push(text),
                None => {
                    data.push_null_placeholder();
                    nulls.set_null(row);
                }
            }
        }
        Column::new(ColumnData::Text(data), nulls).expect("lengths match")
    }

    struct EmptyLob;

    impl LobStream for EmptyLob {
        fn kind(&self) -> LobKind {
            LobKind::Binary
        }

        fn size_hint(&self) -> Option<u64> {
            Some(0)
        }

        fn read_chunk(&mut self, _buf: &mut [u8]) -> DbResult<usize> {
            Ok(0)
        }
    }

    #[test]
    fn null_mask_tracks_individual_rows() {
        let mut mask = NullMask::new(130);
        assert!(!mask.is_empty());
        assert_eq!(mask.null_count(), 0);
        mask.set_null(0);
        mask.set_null(63);
        mask.set_null(64);
        mask.set_null(129);
        mask.set_null(500); // out of range, ignored
        assert!(mask.is_null(0));
        assert!(mask.is_null(63));
        assert!(mask.is_null(64));
        assert!(mask.is_null(129));
        assert!(!mask.is_null(1));
        assert!(!mask.is_null(130));
        assert_eq!(mask.null_count(), 4);
        assert!(NullMask::new(0).is_empty());
    }

    #[test]
    fn text_column_shares_one_buffer() {
        let mut column = TextColumn::new();
        column.push("alpha");
        column.push("");
        column.push("ข้อมูล");
        assert_eq!(column.len(), 3);
        assert_eq!(column.get(0), Some("alpha"));
        assert_eq!(column.get(1), Some(""));
        assert_eq!(column.get(2), Some("ข้อมูล"));
        assert_eq!(column.get(3), None);
        assert_eq!(column.total_bytes(), "alpha".len() + "ข้อมูล".len());
        assert!(TextColumn::new().is_empty());
    }

    #[test]
    fn bytes_column_shares_one_buffer() {
        let mut column = BytesColumn::with_capacity(2, 8);
        column.push(&[1, 2, 3]);
        column.push_null_placeholder();
        assert_eq!(column.len(), 2);
        assert_eq!(column.get(0), Some(&[1_u8, 2, 3][..]));
        assert_eq!(column.get(1), Some(&[][..]));
        assert_eq!(column.total_bytes(), 3);
    }

    #[test]
    fn batch_accessors_read_cells_by_row_and_column() {
        let numbers = Column::not_null(ColumnData::Number(vec![
            Number::from(1_i64),
            Number::from(2_i64),
        ]));
        let names = text_column(&[Some("a"), None]);
        let batch = RowBatch::new(vec![numbers, names]).expect("equal lengths");

        assert_eq!(batch.row_count(), 2);
        assert_eq!(batch.column_count(), 2);
        assert!(!batch.is_empty());
        assert_eq!(batch.columns().len(), 2);

        assert_eq!(
            batch.value(0, 0).and_then(|cell| cell.as_number().copied()),
            Some(Number::from(1_i64))
        );
        assert_eq!(batch.value(0, 1).and_then(|cell| cell.as_str()), Some("a"));
        assert!(batch.value(1, 1).expect("cell exists").is_null());
        assert!(batch.value(2, 0).is_none(), "row out of range");
        assert!(batch.value(0, 2).is_none(), "column out of range");
        assert_eq!(batch.column(1).map(Column::null_count), Some(1));
        assert_eq!(batch.column(1).map(Column::kind), Some(ColumnKind::Text));
    }

    #[test]
    fn mismatched_lengths_are_a_driver_bug() {
        let short = Column::not_null(ColumnData::Double(vec![1.0]));
        let long = Column::not_null(ColumnData::Double(vec![1.0, 2.0]));
        let error = RowBatch::new(vec![short, long]).expect_err("should be rejected");
        assert_eq!(error.kind(), ErrorKind::DriverInternal);

        let error = Column::new(ColumnData::Boolean(vec![true]), NullMask::new(2))
            .expect_err("should be rejected");
        assert_eq!(error.kind(), ErrorKind::DriverInternal);
    }

    #[test]
    fn an_empty_batch_means_exhausted() {
        let batch = RowBatch::empty();
        assert!(batch.is_empty());
        assert_eq!(batch.row_count(), 0);
        assert_eq!(batch.column_count(), 0);
        assert!(batch.value(0, 0).is_none());
    }

    #[test]
    fn taking_a_lob_removes_it_from_the_batch() {
        let column = Column::not_null(ColumnData::Lob(vec![Some(LobLocator::new(Box::new(
            EmptyLob,
        )))]));
        let mut batch = RowBatch::new(vec![column]).expect("single column");

        assert!(
            batch
                .value(0, 0)
                .and_then(|cell| cell.as_lob().map(LobLocator::kind))
                .is_some()
        );

        let taken = batch
            .column_mut(0)
            .and_then(|column| column.take_lob(0))
            .expect("locator present");
        assert_eq!(taken.kind(), LobKind::Binary);

        assert!(batch.value(0, 0).expect("cell exists").is_null());
        assert!(
            batch
                .column_mut(0)
                .and_then(|column| column.take_lob(0))
                .is_none(),
            "a locator can only be taken once"
        );
    }

    #[test]
    fn take_lob_only_applies_to_lob_columns() {
        let mut column = Column::not_null(ColumnData::Boolean(vec![true]));
        assert!(column.take_lob(0).is_none());
    }

    #[test]
    fn out_values_are_addressed_the_way_the_binds_were() {
        let positional = OutValues::Positional(vec![None, Some(Value::from(7_i64))]);
        assert!(positional.positional(0).is_none());
        assert!(positional.positional(1).is_some());
        assert!(positional.named("anything").is_none());
        assert!(!positional.is_empty());

        let named = OutValues::Named(vec![("result".into(), Value::from("ok"))]);
        assert!(named.named("result").is_some());
        assert!(named.named("other").is_none());
        assert!(named.positional(0).is_none());

        assert!(OutValues::None.is_empty());
        assert!(OutValues::Positional(vec![None]).is_empty());
    }

    #[test]
    fn execution_outcome_reports_compilation_warnings() {
        let outcome = ExecutionOutcome::new()
            .with_rows_affected(0)
            .with_warnings(vec![
                Warning::new(WarningKind::CompiledWithErrors, "package body has errors")
                    .with_native(NativeError::new(
                        24344,
                        "ORA-24344: success with compilation error",
                    ))
                    .with_position(SqlPosition::at_line_column(12, 3)),
            ]);

        assert!(!outcome.has_cursor());
        assert_eq!(outcome.rows_affected(), Some(0));
        assert!(outcome.compiled_with_errors());
        let warning = &outcome.warnings()[0];
        assert_eq!(warning.native().map(NativeError::code), Some(24344));
        assert_eq!(
            warning.position().and_then(|position| position.line()),
            Some(12)
        );
        assert!(outcome.out_values().is_empty());
        assert!(format!("{outcome:?}").contains("has_cursor: false"));
    }

    #[test]
    fn default_fetch_size_is_usable() {
        assert_eq!(DEFAULT_FETCH_ROWS.get(), 1000);
    }
}
