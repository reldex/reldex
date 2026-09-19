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
use crate::ids::{ConnectionId, ResultSetId};
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

    /// Marks a row as SQL NULL.
    ///
    /// An out-of-range `row` is refused rather than ignored. Silently dropping
    /// it would produce a batch in which a NULL cell reads back as data — a
    /// correctness failure (`SPEC.md` §2) that would surface as wrong values in
    /// a grid, far from its cause.
    ///
    /// Returning a `Result` rather than panicking keeps a driver bug reportable
    /// through the normal error path, and this is not the hot path: it is called
    /// once per NULL cell, not once per cell.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::DriverInternal`] if `row` is not below
    /// [`NullMask::len`].
    pub fn set_null(&mut self, row: usize) -> DbResult<()> {
        if row >= self.len {
            return Err(DbError::internal(format!(
                "row {row} marked NULL in a mask of {} rows",
                self.len
            )));
        }
        self.bits[row / 64] |= 1_u64 << (row % 64);
        Ok(())
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
}

/// The storage family of a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
    /// Values of a type the contract cannot represent, as best-effort text.
    Unsupported,
}

/// One column's values, laid out contiguously.
///
/// Values at NULL rows are unspecified placeholders; read cells through
/// [`Column::value`], which consults the mask first.
#[derive(Debug)]
#[non_exhaustive]
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
    /// already been taken out of the batch — which is *not* the same as SQL
    /// NULL; see [`Column::take_lob`].
    ///
    /// These are live driver handles. A column that still holds any of them
    /// pins its whole [`RowBatch`] to the connection's owning worker thread,
    /// for dropping as much as for reading; see [`RowBatch`].
    Lob(Vec<Option<LobLocator>>),
    /// Values of a type the contract has no variant for, rendered by the driver
    /// as best-effort text.
    ///
    /// This is what keeps `SELECT *` working over a table with an `INTERVAL`,
    /// `ROWID`, `TIMESTAMP WITH LOCAL TIME ZONE`, `XMLType` or `VECTOR` column.
    /// The alternative — failing the fetch, as the contract used to require —
    /// makes a single unsupported column hide an entire table from the user,
    /// which is a worse answer than showing the server's own rendering of it.
    ///
    /// The column's [`crate::SqlType`] is [`crate::SqlType::Unsupported`] and
    /// its [`ColumnMetadata::native_type_name`] names the real type. Cells read
    /// back as [`ValueRef::Unsupported`], never as [`ValueRef::Text`], so
    /// nothing downstream can mistake the rendering for character data.
    Unsupported(TextColumn),
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
            Self::Text(values) | Self::Json(values) | Self::Unsupported(values) => values.len(),
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
            Self::Unsupported(_) => ColumnKind::Unsupported,
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

    /// Whether the value at `row` was moved out of the batch by
    /// [`Column::take_lob`].
    ///
    /// A taken cell is not NULL and must never be reported as one.
    #[must_use]
    pub fn is_taken(&self, row: usize) -> bool {
        match &self.data {
            ColumnData::Lob(values) => {
                row < self.len() && !self.nulls.is_null(row) && values[row].is_none()
            }
            _ => false,
        }
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
            ColumnData::Unsupported(values) => ValueRef::Unsupported(values.get(row)?),
            ColumnData::Lob(values) => match values.get(row)? {
                Some(locator) => ValueRef::Lob(locator),
                // Not NULL: the row held a LOB and something took it.
                None => ValueRef::Taken,
            },
        };
        Some(value)
    }

    /// Takes the large-object locator at `row` so it can be read.
    ///
    /// Reading a LOB needs ownership, so the locator leaves the batch. The NULL
    /// mask is **not** touched: the cell becomes [`ValueRef::Taken`], which is a
    /// distinct state from SQL NULL. Marking it NULL — as this method used to —
    /// would make an exporter that re-reads the batch write an empty cell where
    /// the database holds a value.
    ///
    /// Returns `None` for any other column kind, an out-of-range row, a row that
    /// is SQL NULL, or a locator that was already taken.
    ///
    /// Taking must happen on the worker thread that owns the connection, and so
    /// must dropping a batch from which locators have *not* been taken; see
    /// [`RowBatch`]'s "Thread affinity while it still holds locators".
    pub fn take_lob(&mut self, row: usize) -> Option<LobLocator> {
        if self.nulls.is_null(row) {
            return None;
        }
        let ColumnData::Lob(values) = &mut self.data else {
            return None;
        };
        values.get_mut(row)?.take()
    }
}

/// A batch of rows fetched from a cursor.
///
/// # Thread affinity while it still holds locators
///
/// A batch is `Send`, and once every [`ColumnData::Lob`] locator has been taken
/// out of it with [`Column::take_lob`] it is exactly what D1 calls "plain data,
/// no handles" and may go anywhere. **Until then it is not.** A
/// [`crate::LobLocator`] is a handle derived from the connection, and dropping
/// one runs driver code — so a batch that still holds un-taken locators must be
/// consumed *or dropped* on the worker thread that owns its connection.
/// Dropping it elsewhere issues driver work on a thread that does not own the
/// connection, which with a driver that serialises through an internal mutex is
/// a deadlock rather than a diagnosable error.
///
/// `db-core` satisfies this by taking every locator out on the worker thread
/// before the batch is sent anywhere, and handing the caller its own handles
/// instead; the cells it emptied read back as [`ValueRef::Taken`].
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
/// # Thread affinity
///
/// A cursor is `Send`, and `&mut self` on [`Cursor::fetch_batch`] serializes
/// calls *to that cursor*. Neither fact ties it to a connection: a cursor is an
/// independent handle that a driver may hand to any thread, and two cursors on
/// one connection can exist at once. The rule the type system cannot express is
/// therefore stated here, and `db-core` enforces it:
///
/// > **A cursor is used only on the worker thread that owns its connection.**
/// > Of everything a fetch produces, only [`RowBatch`] — plain data, no handles
/// > — may cross a thread boundary.
///
/// [`Cursor::connection_id`] exists so `db-core` can assert this rather than
/// hope. The same rule applies to [`crate::LobStream`] and to a nested cursor
/// returned as a [`crate::Value`].
///
/// # Lifecycle
///
/// - **After any error** from [`Cursor::fetch_batch`], the only legal call is
///   [`Cursor::close`]. A driver must not assume the caller obeys this: a
///   further `fetch_batch` has to return a [`DbError`], never panic, never block
///   and never silently resume a half-consumed result set.
/// - **After [`Cursor::close`]**, the cursor is gone — `close` consumes it, so
///   this is enforced.
/// - **After the owning connection is closed**
///   ([`DatabaseConnection::close`](crate::DatabaseConnection::close)), every
///   cursor derived from it returns [`DbError::connection_closed`] from
///   `fetch_batch` — and from every method **except** [`Cursor::close`], which
///   is always idempotent and reports `Ok(())` when there is nothing left to
///   release. It must never panic and never block on a connection that no
///   longer exists.
/// - **A commit or rollback may invalidate an open cursor.** Servers differ, and
///   `ROLLBACK` in particular commonly closes cursors. A driver must not pretend
///   otherwise: the next `fetch_batch` reports an ordinary [`DbError`]
///   (`ErrorKind::Transaction` when the server says so) and `db-core` surfaces
///   it rather than returning a truncated result as if it were complete
///   (`SPEC.md` §2). See
///   [`DatabaseConnection::commit`](crate::DatabaseConnection::commit).
pub trait Cursor: Send {
    /// This result set's identifier.
    fn id(&self) -> ResultSetId;

    /// The connection this cursor fetches over.
    ///
    /// `db-core` asserts that a cursor is only touched on the worker thread that
    /// owns this connection. Must be cheap: an accessor on a field the driver
    /// already holds, never a round trip.
    fn connection_id(&self) -> ConnectionId;

    /// Column descriptions, in select-list order.
    fn columns(&self) -> &[ColumnMetadata];

    /// Fetches up to `max_rows` more rows.
    ///
    /// A batch with no rows means the result is exhausted; a driver must not
    /// return an empty batch while more rows remain.
    ///
    /// `max_rows` bounds this call. The statement-level
    /// [`Statement::fetch_rows`](crate::Statement::fetch_rows) hint, which a
    /// driver needs *before* execute to size its own array fetch, is a separate
    /// knob; see that method.
    ///
    /// # Errors
    ///
    /// Any [`DbError`]. A cancelled fetch returns
    /// [`crate::ErrorKind::Cancelled`]. After any error the cursor is finished;
    /// see the trait's lifecycle rules.
    fn fetch_batch(&mut self, max_rows: NonZeroUsize) -> DbResult<RowBatch>;

    /// Whether the driver already knows the result is exhausted.
    fn is_exhausted(&self) -> bool;

    /// Releases the cursor's server-side and client-side resources.
    ///
    /// This is the one call that stays legal after a failed fetch, and after the
    /// owning connection has been closed. It is **idempotent and
    /// infallible-in-spirit**: when there is nothing left to release — because
    /// the connection is gone, or the fetch already failed — it reports
    /// `Ok(())`. Only a *real* failure to release something is an error.
    ///
    /// This is deliberately the opposite of the rule for every other method on a
    /// derived handle, which reports [`DbError::connection_closed`] once its
    /// connection is gone. The asymmetry is the useful one: `close` exists to
    /// let go, and a release path that fails when there is nothing to release
    /// only teaches callers to ignore its result.
    ///
    /// # Errors
    ///
    /// Any [`DbError`] the release actually produced. Dropping a cursor without
    /// calling this is allowed; the driver must then release resources on drop
    /// and swallow the error.
    fn close(self: Box<Self>) -> DbResult<()>;
}

/// Why a statement produced a warning.
///
/// The set is deliberately tiny, and it must stay that way: a driver may have
/// nothing better than a free-text warning string to classify — the primary
/// driver's `last_warning()` returns exactly that (ADR-0001, `oracledb` review)
/// — so kind detection can be **text-based** inside a driver. A large taxonomy
/// would therefore be a taxonomy of substring matches pretending to be types.
/// [`Warning::message`] and [`Warning::native`] carry the detail; the kind only
/// says whether the UI must act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WarningKind {
    /// A PL/SQL object was created but did not compile cleanly. `SPEC.md` §24.14
    /// requires these to reach the user.
    CompiledWithErrors,
    /// The server reported something worth showing that is not an error.
    ///
    /// This is the honest destination for a warning a driver cannot classify —
    /// which, for a driver whose upstream gives it only a `String`, is most of
    /// them. Without it such a warning would have to be dropped or mislabelled.
    Informational,
}

/// What kind of statement the server just ran.
///
/// Reported by the driver because `db-core` must not parse SQL (ADR-0002 D6) and
/// the server is the only authority. Two things need it:
///
/// - **DDL commits.** Most SQL servers commit the open transaction before *and*
///   after a DDL statement, whatever the client's auto-commit setting. The
///   contract cannot forbid this and must not hide it: `SPEC.md` §10's "never
///   silently commit" is a rule about *Reldex's* behaviour, and the honest way
///   to keep it here is to tell the user it happened. A driver reporting
///   [`StatementKind::Ddl`] lets `db-core` reset its transaction tracking and
///   lets the UI say so.
/// - **Conservative transaction tracking.** A driver that cannot observe
///   server-side transaction state ([`Capabilities::exact_transaction_state`] is
///   false) still knows that a `SELECT` did not open a write transaction and a
///   `COMMIT` closed one.
///
/// A driver that genuinely cannot tell reports [`StatementKind::Other`], which
/// is the default, and `db-core` stays conservative.
///
/// [`Capabilities::exact_transaction_state`]: crate::Capabilities::exact_transaction_state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum StatementKind {
    /// A query that returns rows (`SELECT`, `WITH … SELECT`).
    Query,
    /// Data manipulation (`INSERT`, `UPDATE`, `DELETE`, `MERGE`).
    Dml,
    /// Data definition (`CREATE`, `ALTER`, `DROP`, `TRUNCATE`, `GRANT`).
    ///
    /// See [`StatementKind::commits_implicitly`].
    Ddl,
    /// An anonymous PL/SQL block or a call to stored code, which may have done
    /// anything, including committing.
    PlSqlBlock,
    /// `COMMIT`, `ROLLBACK`, `SAVEPOINT` or `SET TRANSACTION` submitted as text
    /// rather than through [`crate::DatabaseConnection::commit`] and friends.
    TransactionControl,
    /// `ALTER SESSION`, `SET ROLE` and similar statements that change session
    /// state a worksheet owns (`SPEC.md` §9).
    SessionControl,
    /// Something else, or the driver could not classify it.
    #[default]
    Other,
}

impl StatementKind {
    /// Whether running this kind of statement commits the open transaction on
    /// the server regardless of any client setting.
    ///
    /// True for [`StatementKind::Ddl`]. `db-core` must treat the transaction as
    /// resolved and tell the user, because it could neither prevent nor undo it.
    #[must_use]
    pub const fn commits_implicitly(self) -> bool {
        matches!(self, Self::Ddl)
    }

    /// Whether this kind may have changed the transaction state in a way the
    /// core cannot predict, so tracking must fall back to
    /// [`crate::TransactionState::Unknown`].
    #[must_use]
    pub const fn transaction_state_is_unpredictable(self) -> bool {
        matches!(self, Self::PlSqlBlock | Self::Other)
    }
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
///
/// # Borrowing and owning
///
/// [`OutValues::positional`] and [`OutValues::named`] borrow, which is enough
/// for plain data. A [`Value::Lob`] or a [`Value::Cursor`] is not plain data:
/// every useful method on [`Cursor`] needs ownership
/// ([`Cursor::fetch_batch`] takes `&mut self`, [`Cursor::close`] takes
/// `Box<Self>`), so a `REF CURSOR` delivered through an OUT bind is unreadable
/// through a shared reference. [`OutValues::take_named`] and
/// [`OutValues::take_positional`] move the value out instead, leaving
/// [`Value::Taken`] in the slot — a state distinct from SQL NULL, for the same
/// reason [`Column::take_lob`] leaves [`ValueRef::Taken`] rather than marking
/// the row NULL.
#[derive(Debug)]
#[non_exhaustive]
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

    /// Takes the value written back for a positional bind, so a driver-owned
    /// handle inside it can be used.
    ///
    /// The slot is left holding [`Value::Taken`], which is *not* SQL NULL: the
    /// bind did carry a value and something took it. Returns `None` when there
    /// is nothing to take — no such slot, an input-only bind, or a slot that was
    /// already taken — which makes repeated calls safe rather than a way to own
    /// the same live cursor twice.
    pub fn take_positional(&mut self, index: usize) -> Option<Value> {
        match self {
            Self::Positional(values) => take_slot(values.get_mut(index)?.as_mut()?),
            Self::None | Self::Named(_) => None,
        }
    }

    /// Takes the value written back for a named bind. See
    /// [`OutValues::take_positional`] for what is left behind.
    pub fn take_named(&mut self, name: &str) -> Option<Value> {
        match self {
            Self::Named(values) => {
                let slot = values
                    .iter_mut()
                    .find(|(key, _)| &**key == name)
                    .map(|(_, value)| value)?;
                take_slot(slot)
            }
            Self::None | Self::Positional(_) => None,
        }
    }

    /// Whether anything was written back.
    ///
    /// Still true of a slot whose value has been taken: the statement did have
    /// an output bind, and reporting otherwise would hide it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Positional(values) => values.iter().all(Option::is_none),
            Self::Named(values) => values.is_empty(),
        }
    }
}

impl OutValues {
    /// The same container with every value replaced by [`Value::Taken`].
    ///
    /// This is what [`ExecutionOutcome::take_out_values`] leaves behind, so an
    /// outcome whose values have been moved out still reports the binds it had
    /// rather than claiming it had none.
    fn same_shape_all_taken(&self) -> Self {
        match self {
            Self::None => Self::None,
            Self::Positional(values) => Self::Positional(
                values
                    .iter()
                    .map(|slot| slot.as_ref().map(|_| Value::Taken))
                    .collect(),
            ),
            Self::Named(values) => Self::Named(
                values
                    .iter()
                    .map(|(name, _)| (name.clone(), Value::Taken))
                    .collect(),
            ),
        }
    }
}

/// Moves a value out of a slot, leaving [`Value::Taken`]; `None` if it was
/// already taken.
fn take_slot(slot: &mut Value) -> Option<Value> {
    if slot.is_taken() {
        return None;
    }
    Some(std::mem::replace(slot, Value::Taken))
}

/// Everything one `execute` produced.
///
/// There is one entry point for execution because a worksheet cannot know
/// whether arbitrary user text returns rows, and the core must not parse SQL to
/// find out (ADR-0002 D6).
pub struct ExecutionOutcome {
    cursor: Option<Box<dyn Cursor>>,
    rows_affected: Option<u64>,
    statement_kind: StatementKind,
    out_values: OutValues,
    warnings: Vec<Warning>,
}

impl ExecutionOutcome {
    /// An outcome with no result set, no rows affected and no output.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cursor: None,
            rows_affected: None,
            statement_kind: StatementKind::Other,
            out_values: OutValues::None,
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

    /// Records what kind of statement the server ran. See [`StatementKind`].
    #[must_use]
    pub fn with_statement_kind(mut self, statement_kind: StatementKind) -> Self {
        self.statement_kind = statement_kind;
        self
    }

    /// Attaches the values written back through output binds.
    #[must_use]
    pub fn with_out_values(mut self, out_values: OutValues) -> Self {
        self.out_values = out_values;
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

    /// How many rows the statement changed, if the driver reported it.
    #[must_use]
    pub const fn rows_affected(&self) -> Option<u64> {
        self.rows_affected
    }

    /// What kind of statement the server ran, as far as the driver can tell.
    #[must_use]
    pub const fn statement_kind(&self) -> StatementKind {
        self.statement_kind
    }

    /// Whether the server committed the open transaction as a side effect of
    /// running this statement, whatever auto-commit said.
    ///
    /// `db-core` must reset its transaction tracking and tell the user; there
    /// was nothing it could have done to prevent it. See [`StatementKind::Ddl`].
    #[must_use]
    pub const fn committed_implicitly(&self) -> bool {
        self.statement_kind.commits_implicitly()
    }

    /// The values written back through output binds.
    #[must_use]
    pub const fn out_values(&self) -> &OutValues {
        &self.out_values
    }

    /// Takes the output-bind values out of the outcome, so the caller owns any
    /// driver handle inside them.
    ///
    /// Mirrors [`ExecutionOutcome::take_cursor`]. The outcome keeps a
    /// same-shaped [`OutValues`] whose slots read back as [`Value::Taken`], so
    /// it still reports that the statement had output binds and never
    /// misreports a consumed slot as SQL NULL.
    ///
    /// A caller that only needs one value can leave the container in place and
    /// use [`OutValues::take_named`] through
    /// [`ExecutionOutcome::out_values_mut`] instead.
    pub fn take_out_values(&mut self) -> OutValues {
        let emptied = self.out_values.same_shape_all_taken();
        std::mem::replace(&mut self.out_values, emptied)
    }

    /// The values written back through output binds, mutably, for taking a
    /// driver-owned handle out of one. Mirrors [`RowBatch::column_mut`].
    pub fn out_values_mut(&mut self) -> &mut OutValues {
        &mut self.out_values
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
            .field("statement_kind", &self.statement_kind)
            .field("out_values", &self.out_values)
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
                    nulls.set_null(row).expect("row is in range");
                }
            }
        }
        Column::new(ColumnData::Text(data), nulls).expect("lengths match")
    }

    struct EmptyLob;

    impl LobStream for EmptyLob {
        fn connection_id(&self) -> ConnectionId {
            ConnectionId::from_raw(3)
        }

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

    fn lob_column(rows: &[bool]) -> Column {
        let mut values = Vec::with_capacity(rows.len());
        let mut nulls = NullMask::new(rows.len());
        for (row, present) in rows.iter().enumerate() {
            if *present {
                values.push(Some(LobLocator::new(Box::new(EmptyLob))));
            } else {
                values.push(None);
                nulls.set_null(row).expect("row is in range");
            }
        }
        Column::new(ColumnData::Lob(values), nulls).expect("lengths match")
    }

    #[test]
    fn null_mask_tracks_individual_rows() {
        let mut mask = NullMask::new(130);
        assert!(!mask.is_empty());
        for row in [0, 63, 64, 129] {
            mask.set_null(row).expect("row is in range");
        }
        assert!(mask.is_null(0));
        assert!(mask.is_null(63));
        assert!(mask.is_null(64));
        assert!(mask.is_null(129));
        assert!(!mask.is_null(1));
        assert!(!mask.is_null(130));
        assert!(NullMask::new(0).is_empty());
    }

    #[test]
    fn marking_a_row_outside_the_mask_is_reported_not_ignored() {
        // Swallowing this used to turn a decoder bug into a batch whose NULLs
        // silently read back as data.
        let mut mask = NullMask::new(2);
        let error = mask.set_null(2).expect_err("row 2 is out of range");
        assert_eq!(error.kind(), ErrorKind::DriverInternal);
        assert!(error.message().contains('2'), "{error}");
        assert!(!mask.is_null(0), "a rejected call must not touch the mask");

        let error = NullMask::new(0)
            .set_null(0)
            .expect_err("an empty mask has no rows at all");
        assert_eq!(error.kind(), ErrorKind::DriverInternal);
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
        assert_eq!(batch.column(1).map(Column::kind), Some(ColumnKind::Text));
        assert_eq!(batch.column(1).map(|column| column.is_null(1)), Some(true));
    }

    #[test]
    fn an_unsupported_column_keeps_the_rest_of_the_row_readable() {
        // `SELECT *` over a table with an INTERVAL column must still return the
        // other columns. It used to be a hard error for the whole fetch.
        let mut rendering = TextColumn::with_capacity(2, 64);
        rendering.push("+000000002 03:04:05.000000");
        rendering.push_null_placeholder();
        let mut nulls = NullMask::new(2);
        nulls.set_null(1).expect("row is in range");

        let unsupported =
            Column::new(ColumnData::Unsupported(rendering), nulls).expect("lengths match");
        let names = text_column(&[Some("a"), Some("b")]);
        let batch = RowBatch::new(vec![names, unsupported]).expect("equal lengths");

        assert_eq!(
            batch.column(1).map(Column::kind),
            Some(ColumnKind::Unsupported)
        );
        assert_eq!(
            batch
                .value(0, 1)
                .and_then(|cell| cell.as_unsupported_text()),
            Some("+000000002 03:04:05.000000")
        );
        // It is not text, and it is not NULL.
        assert!(batch.value(0, 1).and_then(|cell| cell.as_str()).is_none());
        assert!(!batch.value(0, 1).expect("cell exists").is_null());
        // A genuine NULL in an unsupported column is still a NULL.
        assert!(batch.value(1, 1).expect("cell exists").is_null());
        // The supported columns are unaffected, which is the whole point.
        assert_eq!(batch.value(0, 0).and_then(|cell| cell.as_str()), Some("a"));
        assert_eq!(batch.value(1, 0).and_then(|cell| cell.as_str()), Some("b"));
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
    fn a_consumed_lob_is_distinguishable_from_sql_null() {
        // Row 0 holds a LOB, row 1 is SQL NULL.
        let mut batch = RowBatch::new(vec![lob_column(&[true, false])]).expect("single column");

        assert!(
            batch
                .value(0, 0)
                .and_then(|cell| cell.as_lob().map(LobLocator::kind))
                .is_some()
        );
        assert!(batch.value(1, 0).expect("cell exists").is_null());

        let taken = batch
            .column_mut(0)
            .and_then(|column| column.take_lob(0))
            .expect("locator present");
        assert_eq!(taken.kind(), LobKind::Binary);

        // The row is now "taken", NOT null: the database did hold a value, and
        // an exporter re-reading the batch must not write an empty cell.
        let cell = batch.value(0, 0).expect("cell exists");
        assert!(!cell.is_null(), "a consumed LOB must not impersonate NULL");
        assert!(cell.is_taken());
        assert_eq!(batch.column(0).map(|column| column.is_taken(0)), Some(true));
        assert_eq!(batch.column(0).map(|column| column.is_null(0)), Some(false));

        // The genuinely NULL row is untouched and is still NULL, not "taken".
        assert!(batch.value(1, 0).expect("cell exists").is_null());
        assert_eq!(
            batch.column(0).map(|column| column.is_taken(1)),
            Some(false)
        );

        assert!(
            batch
                .column_mut(0)
                .and_then(|column| column.take_lob(0))
                .is_none(),
            "a locator can only be taken once"
        );
        assert!(
            batch
                .column_mut(0)
                .and_then(|column| column.take_lob(1))
                .is_none(),
            "there is nothing to take from a SQL NULL row"
        );
    }

    #[test]
    fn take_lob_only_applies_to_lob_columns() {
        let mut column = Column::not_null(ColumnData::Boolean(vec![true]));
        assert!(column.take_lob(0).is_none());
        assert!(!column.is_taken(0));
        assert!(!column.is_taken(99));
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

    /// A stand-in for a driver's nested `REF CURSOR`: nothing about it works
    /// through a shared reference, which is the whole of contract gap C-1.
    struct StubCursor {
        columns: Vec<ColumnMetadata>,
    }

    impl StubCursor {
        fn boxed() -> Box<dyn Cursor> {
            Box::new(Self {
                columns: vec![ColumnMetadata::new("ID", crate::SqlType::Number)],
            })
        }
    }

    impl Cursor for StubCursor {
        fn id(&self) -> ResultSetId {
            ResultSetId::from_raw(11)
        }

        fn connection_id(&self) -> ConnectionId {
            ConnectionId::from_raw(3)
        }

        fn columns(&self) -> &[ColumnMetadata] {
            &self.columns
        }

        fn fetch_batch(&mut self, _max_rows: NonZeroUsize) -> DbResult<RowBatch> {
            Ok(RowBatch::empty())
        }

        fn is_exhausted(&self) -> bool {
            true
        }

        fn close(self: Box<Self>) -> DbResult<()> {
            Ok(())
        }
    }

    #[test]
    fn a_ref_cursor_out_value_can_be_owned_and_fetched() {
        // Contract gap C-1: `OutValues` used to expose only `&Value`, and every
        // useful method on `Cursor` needs ownership — so a REF CURSOR could be
        // described but never read, which made `Capabilities::ref_cursor`
        // unmeetable.
        let mut outcome = ExecutionOutcome::new()
            .with_statement_kind(StatementKind::PlSqlBlock)
            .with_out_values(OutValues::Named(vec![(
                "rc".into(),
                Value::Cursor(StubCursor::boxed()),
            )]));

        let taken = outcome
            .out_values_mut()
            .take_named("rc")
            .expect("the cursor is there");
        let Value::Cursor(mut cursor) = taken else {
            panic!("expected a cursor, got {taken:?}");
        };
        assert_eq!(cursor.columns().len(), 1);
        assert!(
            cursor
                .fetch_batch(NonZeroUsize::new(10).expect("non-zero"))
                .expect("fetch")
                .is_empty()
        );
        cursor.close().expect("close");
    }

    #[test]
    fn a_taken_out_value_is_distinguishable_from_null_and_cannot_be_taken_twice() {
        let mut values = OutValues::Named(vec![
            ("rc".into(), Value::Cursor(StubCursor::boxed())),
            ("nothing".into(), Value::Null),
        ]);

        assert!(values.take_named("rc").is_some());
        // The slot is now "taken", NOT null: the bind did carry a value, and a
        // caller that read this as NULL would report the wrong thing.
        let slot = values.named("rc").expect("the slot is still there");
        assert!(slot.is_taken());
        assert!(
            !slot.is_null(),
            "a consumed value must not impersonate NULL"
        );
        assert_eq!(slot.type_name(), "taken");
        // Owning the same live cursor twice is exactly what must not happen.
        assert!(values.take_named("rc").is_none());
        assert!(values.take_named("absent").is_none());
        // `is_empty` still reports that the statement had output binds.
        assert!(!values.is_empty());

        // A genuine NULL is taken as a NULL, and is not confused with the above.
        let null = values
            .take_named("nothing")
            .expect("a NULL is still a value");
        assert!(null.is_null());
        assert!(!null.is_taken());
    }

    #[test]
    fn positional_out_values_are_taken_by_index_and_input_slots_stay_empty() {
        let mut values = OutValues::Positional(vec![None, Some(Value::from(7_i64))]);
        assert!(
            values.take_positional(0).is_none(),
            "an input-only bind has nothing to take"
        );
        assert!(values.take_positional(9).is_none(), "index out of range");
        let taken = values.take_positional(1).expect("an output bind");
        assert!(matches!(taken, Value::Number(_)), "{taken:?}");
        assert!(values.take_positional(1).is_none());
        assert!(values.positional(1).is_some_and(Value::is_taken));
        // Taking from the wrong shape is a no-op, not a panic.
        assert!(OutValues::None.take_positional(0).is_none());
        assert!(OutValues::None.take_named("x").is_none());
        assert!(
            OutValues::Positional(vec![Some(Value::Null)])
                .take_named("x")
                .is_none()
        );
    }

    #[test]
    fn taking_every_out_value_leaves_the_shape_behind() {
        // `take_out_values` is the wholesale form `db-core` uses: it must not
        // turn "this statement had output binds" into "it had none".
        let mut outcome = ExecutionOutcome::new().with_out_values(OutValues::Named(vec![
            ("rc".into(), Value::Cursor(StubCursor::boxed())),
            ("n".into(), Value::from(1_i64)),
        ]));
        let mut owned = outcome.take_out_values();
        assert!(matches!(owned.take_named("rc"), Some(Value::Cursor(_))));
        assert!(matches!(owned.take_named("n"), Some(Value::Number(_))));

        let left = outcome.out_values();
        assert!(!left.is_empty(), "the binds existed and must still show");
        assert!(left.named("rc").is_some_and(Value::is_taken));
        assert!(left.named("n").is_some_and(Value::is_taken));
        assert!(outcome.out_values_mut().take_named("rc").is_none());

        // The positional shape survives the same way, input slots included.
        let mut outcome = ExecutionOutcome::new()
            .with_out_values(OutValues::Positional(vec![None, Some(Value::from(2_i64))]));
        assert!(outcome.take_out_values().take_positional(1).is_some());
        assert!(outcome.out_values().positional(0).is_none());
        assert!(
            outcome
                .out_values()
                .positional(1)
                .is_some_and(Value::is_taken)
        );

        // Nothing to take is still nothing.
        let mut empty = ExecutionOutcome::new();
        assert!(empty.take_out_values().is_empty());
        assert!(empty.out_values().is_empty());
    }

    #[test]
    fn execution_outcome_reports_compilation_warnings() {
        let outcome = ExecutionOutcome::new()
            .with_rows_affected(0)
            .with_statement_kind(StatementKind::PlSqlBlock)
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
    fn a_driver_that_cannot_classify_a_warning_still_reports_it() {
        // The primary driver's upstream exposes warnings as a bare `String`, so
        // classification may be text-based. `Informational` is where an
        // unclassifiable warning goes; without it a driver would have to drop
        // the warning or mislabel it as a compilation failure.
        let outcome = ExecutionOutcome::new().with_warnings(vec![Warning::new(
            WarningKind::Informational,
            "ORA-24344: success with compilation error",
        )]);
        assert_eq!(outcome.warnings().len(), 1);
        assert_eq!(outcome.warnings()[0].kind(), WarningKind::Informational);
        assert!(
            !outcome.compiled_with_errors(),
            "an unclassified warning must not be promoted to a compilation failure"
        );
    }

    #[test]
    fn ddl_reports_the_commit_the_contract_cannot_forbid() {
        // `SPEC.md` §10 forbids Reldex committing silently. A server that
        // commits around DDL whatever the client asked for is not something the
        // contract can prevent — so it reports it and the user gets told.
        let ddl = ExecutionOutcome::new().with_statement_kind(StatementKind::Ddl);
        assert_eq!(ddl.statement_kind(), StatementKind::Ddl);
        assert!(ddl.committed_implicitly());

        for kind in [
            StatementKind::Query,
            StatementKind::Dml,
            StatementKind::PlSqlBlock,
            StatementKind::TransactionControl,
            StatementKind::SessionControl,
            StatementKind::Other,
        ] {
            let outcome = ExecutionOutcome::new().with_statement_kind(kind);
            assert!(!outcome.committed_implicitly(), "{kind:?}");
        }

        // A driver that cannot classify says so, and the core stays cautious.
        assert_eq!(StatementKind::default(), StatementKind::Other);
        assert!(ExecutionOutcome::new().statement_kind() == StatementKind::Other);
        assert!(StatementKind::Other.transaction_state_is_unpredictable());
        assert!(StatementKind::PlSqlBlock.transaction_state_is_unpredictable());
        assert!(!StatementKind::Query.transaction_state_is_unpredictable());
    }

    #[test]
    fn default_fetch_size_is_usable() {
        assert_eq!(DEFAULT_FETCH_ROWS.get(), 1000);
    }
}
