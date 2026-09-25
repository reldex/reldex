//! One fetched batch, compacted: the unit the result store retains
//! (ADR-0004 RS1).
//!
//! A [`ResultSegment`] is built **on the session's worker thread**, as part
//! of the fetch whose reply goes to a store, after every large-object locator
//! in the batch has been parked there, and before the reply leaves the
//! worker. From then on it is immutable plain data behind an `Arc`: never
//! mutated, moved or evicted, and `Send + Sync`, so it may be read from any
//! thread (ADR-0003 A12) while the worker keeps fetching into new segments.
//!
//! # Representation per column kind
//!
//! | Kind | Kept as |
//! | --- | --- |
//! | `NUMBER`, every non-NULL value scaling exactly (scale ≤ 18, mantissa in `i64`) | one `i64` per row plus one scale for the column |
//! | `NUMBER`, otherwise | the driver's `Vec<Number>`, at its exact size |
//! | text, JSON, unsupported-as-text, bytes | the driver's buffer + offsets, at their exact size |
//! | timestamps, booleans, binary floats | the driver's vector, at its exact size |
//! | large objects | one `u64` id per row (0 for NULL); the locator stays parked on the worker |
//! | NULL | the driver's bit mask, unchanged |
//!
//! A driver vector with no spare capacity is moved in as it is. One with spare
//! capacity — a driver reserves before it knows the lengths — is copied into
//! an exact allocation, and the batch is freed only once every column is
//! copied; `Spent` below says why that beats shrinking in place, with the
//! numbers. The only per-cell work beyond that copy is the `NUMBER`
//! re-encoding and the LOB parking.

use reldex_db_driver_api::{
    BytesColumn, ColumnData, ColumnKind, DbError, DbResult, LobLocator, NullMask, Number, RowBatch,
    TextColumn, Timestamp,
};

use crate::ids::{LobHandle, SessionId};
use crate::store::scaled::{NumberValue, ScaledNumber};

/// What one retained large-object cell is charged against the byte cap, in
/// place of its 8-byte id: ADR-0004's nominal 256 bytes for the core-side
/// structures plus an allowance for the driver's parked locator, until M5.5
/// measures the real per-locator cost.
pub(crate) const LOB_CELL_BYTES: usize = 256;

/// How a `NUMBER` column of one segment is stored. See the module
/// documentation.
#[derive(Debug)]
enum NumberStorage {
    Scaled { values: Vec<i64>, scale: u8 },
    Decimal(Vec<Number>),
}

#[derive(Debug)]
enum Storage {
    Boolean(Vec<bool>),
    Number(NumberStorage),
    Float(Vec<f32>),
    Double(Vec<f64>),
    Text(TextColumn),
    Bytes(BytesColumn),
    Timestamp(Vec<Timestamp>),
    Json(TextColumn),
    Unsupported(TextColumn),
    /// One id per row; `0` for a NULL cell. The id is a [`LobHandle`]'s
    /// serial within the segment's session.
    Lob(Vec<u64>),
}

/// One column of a [`ResultSegment`]: its values and its NULL mask.
#[derive(Debug)]
pub struct SegmentColumn {
    nulls: NullMask,
    storage: Storage,
}

/// A borrowed, typed view of how one [`SegmentColumn`] stores its values —
/// what a consumer that reads a whole column at once (the FFI's column view,
/// ADR-0004 RS5) needs, without the storage itself being public.
///
/// Values at NULL rows are unspecified placeholders; consult
/// [`SegmentColumn::nulls`] (or read cells with [`SegmentColumn::value`],
/// which does). `#[non_exhaustive]`: a later encoding is an addition.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum SegmentData<'a> {
    /// Booleans.
    Boolean(&'a [bool]),
    /// `NUMBER`, scaled: value = `values[row] × 10^-scale`.
    ScaledNumber {
        /// One mantissa per row.
        values: &'a [i64],
        /// The column's scale in this segment.
        scale: u8,
    },
    /// `NUMBER`, as the driver delivered it.
    Number(&'a [Number]),
    /// 32-bit binary floats.
    Float(&'a [f32]),
    /// 64-bit binary floats.
    Double(&'a [f64]),
    /// UTF-8 text: one buffer plus offsets, exactly sized.
    Text(&'a TextColumn),
    /// Raw bytes: one buffer plus offsets, exactly sized.
    Bytes(&'a BytesColumn),
    /// Dates and timestamps.
    Timestamp(&'a [Timestamp]),
    /// JSON text.
    Json(&'a TextColumn),
    /// A type the contract cannot represent, as the driver's text rendering.
    Unsupported(&'a TextColumn),
    /// Large objects: one id per row, `0` for NULL. An id is only meaningful
    /// together with the segment's session ([`SegmentColumn::lob`] builds
    /// the handle), and whether it can still be read is the store's to say
    /// ([`crate::ResultStore::lob`]).
    Lob(&'a [u64]),
}

/// One cell as the store holds it.
///
/// Mirrors [`reldex_db_driver_api::ValueRef`] with two differences: a
/// `NUMBER` may be scaled ([`NumberValue`]), and a large object is a
/// [`LobHandle`] or a statement that it can no longer be read — never a
/// driver locator and never NULL. There is no "taken" state: the store never
/// holds a locator to take.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum CellValue<'a> {
    /// SQL NULL.
    Null,
    /// A boolean.
    Boolean(bool),
    /// An exact decimal.
    Number(NumberValue<'a>),
    /// A 32-bit binary float.
    Float(f32),
    /// A 64-bit binary float.
    Double(f64),
    /// Text.
    Text(&'a str),
    /// Raw bytes.
    Bytes(&'a [u8]),
    /// A date or timestamp.
    Timestamp(Timestamp),
    /// JSON text.
    Json(&'a str),
    /// The driver's text rendering of a type the contract cannot represent.
    Unsupported(&'a str),
    /// A large object, parked on the session's worker, read through
    /// [`crate::DatabaseSession::read_lob_chunk`].
    Lob(LobHandle),
    /// A large object that can no longer be read, and why. Never NULL: the
    /// database holds a value here (ADR-0004 RS2).
    LobUnavailable(crate::store::LobUnavailable),
}

impl SegmentColumn {
    /// The storage family, as the driver contract names it.
    #[must_use]
    pub const fn kind(&self) -> ColumnKind {
        match &self.storage {
            Storage::Boolean(_) => ColumnKind::Boolean,
            Storage::Number(_) => ColumnKind::Number,
            Storage::Float(_) => ColumnKind::Float,
            Storage::Double(_) => ColumnKind::Double,
            Storage::Text(_) => ColumnKind::Text,
            Storage::Bytes(_) => ColumnKind::Bytes,
            Storage::Timestamp(_) => ColumnKind::Timestamp,
            Storage::Json(_) => ColumnKind::Json,
            Storage::Unsupported(_) => ColumnKind::Unsupported,
            Storage::Lob(_) => ColumnKind::Lob,
        }
    }

    /// Which rows are SQL NULL; the same bitmap the driver delivered.
    #[must_use]
    pub const fn nulls(&self) -> &NullMask {
        &self.nulls
    }

    /// How the values are stored. See [`SegmentData`].
    #[must_use]
    pub fn data(&self) -> SegmentData<'_> {
        match &self.storage {
            Storage::Boolean(values) => SegmentData::Boolean(values),
            Storage::Number(NumberStorage::Scaled { values, scale }) => SegmentData::ScaledNumber {
                values,
                scale: *scale,
            },
            Storage::Number(NumberStorage::Decimal(values)) => SegmentData::Number(values),
            Storage::Float(values) => SegmentData::Float(values),
            Storage::Double(values) => SegmentData::Double(values),
            Storage::Text(values) => SegmentData::Text(values),
            Storage::Bytes(values) => SegmentData::Bytes(values),
            Storage::Timestamp(values) => SegmentData::Timestamp(values),
            Storage::Json(values) => SegmentData::Json(values),
            Storage::Unsupported(values) => SegmentData::Unsupported(values),
            Storage::Lob(ids) => SegmentData::Lob(ids),
        }
    }

    /// Whether `row` is SQL NULL. Out of range reads `false`.
    #[must_use]
    pub fn is_null(&self, row: usize) -> bool {
        self.nulls.is_null(row)
    }

    /// Reads one cell, or `None` when `row` is out of range. `session` is
    /// the segment's own, for building a LOB cell's handle.
    ///
    /// Allocation-free: this is the per-cell read path.
    fn value(&self, session: SessionId, row: usize) -> Option<CellValue<'_>> {
        if row >= self.nulls.len() {
            return None;
        }
        if self.nulls.is_null(row) {
            return Some(CellValue::Null);
        }
        Some(match &self.storage {
            Storage::Boolean(values) => CellValue::Boolean(*values.get(row)?),
            Storage::Number(NumberStorage::Scaled { values, scale }) => {
                let scaled = ScaledNumber::new(*values.get(row)?, *scale)?;
                CellValue::Number(NumberValue::Scaled(scaled))
            }
            Storage::Number(NumberStorage::Decimal(values)) => {
                CellValue::Number(NumberValue::Decimal(values.get(row)?))
            }
            Storage::Float(values) => CellValue::Float(*values.get(row)?),
            Storage::Double(values) => CellValue::Double(*values.get(row)?),
            Storage::Text(values) => CellValue::Text(values.get(row)?),
            Storage::Bytes(values) => CellValue::Bytes(values.get(row)?),
            Storage::Timestamp(values) => CellValue::Timestamp(*values.get(row)?),
            Storage::Json(values) => CellValue::Json(values.get(row)?),
            Storage::Unsupported(values) => CellValue::Unsupported(values.get(row)?),
            Storage::Lob(ids) => CellValue::Lob(LobHandle::from_serial(session, *ids.get(row)?)),
        })
    }

    /// The heap bytes this column holds: its mask, and its values'
    /// capacity. A LOB cell is charged [`LOB_CELL_BYTES`] instead of its id.
    fn heap_bytes(&self) -> usize {
        let nulls = size_of_val(self.nulls.words());
        let values = match &self.storage {
            Storage::Boolean(values) => values.capacity() * size_of::<bool>(),
            Storage::Number(NumberStorage::Scaled { values, .. }) => {
                values.capacity() * size_of::<i64>()
            }
            Storage::Number(NumberStorage::Decimal(values)) => {
                values.capacity() * size_of::<Number>()
            }
            Storage::Float(values) => values.capacity() * size_of::<f32>(),
            Storage::Double(values) => values.capacity() * size_of::<f64>(),
            Storage::Text(values) | Storage::Json(values) | Storage::Unsupported(values) => {
                values.heap_bytes()
            }
            Storage::Bytes(values) => values.heap_bytes(),
            Storage::Timestamp(values) => values.capacity() * size_of::<Timestamp>(),
            Storage::Lob(ids) => {
                let present = ids.iter().filter(|id| **id != 0).count();
                ids.capacity() * size_of::<u64>() + present * (LOB_CELL_BYTES - size_of::<u64>())
            }
        };
        nulls + values
    }
}

/// One fetched batch, compacted and immutable. See the module documentation.
#[derive(Debug)]
pub struct ResultSegment {
    session: SessionId,
    rows: usize,
    columns: Box<[SegmentColumn]>,
    accounted_bytes: usize,
}

impl ResultSegment {
    /// Compacts a batch that holds **no large objects**, on the calling
    /// thread.
    ///
    /// For tools and measurements that have a plain batch in hand. A batch
    /// with a LOB column is handed back untouched in `Err`: its locators must
    /// be parked on the worker thread that owns the connection, and only the
    /// store's own fetch does that ([`crate::DatabaseSession::fetch_segment`]).
    /// Handing it back, rather than dropping it here, leaves the caller on the
    /// thread it must be dropped on.
    ///
    /// # Errors
    ///
    /// The batch, unchanged, when it has a LOB column or a column of a
    /// storage kind this core does not know.
    pub fn compact(batch: RowBatch) -> Result<Self, RowBatch> {
        let retainable = batch.columns().iter().all(|column| {
            matches!(
                column.kind(),
                ColumnKind::Boolean
                    | ColumnKind::Number
                    | ColumnKind::Float
                    | ColumnKind::Double
                    | ColumnKind::Text
                    | ColumnKind::Bytes
                    | ColumnKind::Timestamp
                    | ColumnKind::Json
                    | ColumnKind::Unsupported
            )
        });
        if !retainable {
            return Err(batch);
        }
        // No LOB column, so the parking callback is never called and the
        // session a LOB handle would name is irrelevant; and every column is
        // of a kind `compact_batch` retains, so it cannot fail. The empty
        // batch below is therefore unreachable, and harmless if it were not.
        compact_batch(SessionId::UNOWNED, batch, |_, _| {
            Err(DbError::internal(
                "reldex-db-core: a batch without LOB columns asked to park a locator",
            ))
        })
        .map_err(|_| RowBatch::empty())
    }

    /// How many rows the segment holds.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.rows
    }

    /// How many columns the segment holds.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Whether the segment holds no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// One column.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&SegmentColumn> {
        self.columns.get(index)
    }

    /// All columns.
    #[must_use]
    pub fn columns(&self) -> &[SegmentColumn] {
        &self.columns
    }

    /// Reads one cell, or `None` when out of range. A LOB cell reads as its
    /// handle whatever has happened since; [`crate::ResultStore::value`] is
    /// the read that also says whether it can still be read.
    #[must_use]
    pub fn value(&self, row: usize, column: usize) -> Option<CellValue<'_>> {
        self.columns.get(column)?.value(self.session, row)
    }

    /// The bytes this segment holds, as the byte cap counts them
    /// (ADR-0004 RS3): the capacity of every value vector, buffer, offset
    /// array and NULL mask, [`LOB_CELL_BYTES`] per LOB cell, and the
    /// segment's own structs. Not the allocator's overhead, which is why the
    /// process's private bytes run a few percent above it (ADR-0004 Table 1).
    #[must_use]
    pub const fn accounted_bytes(&self) -> usize {
        self.accounted_bytes
    }

    /// The session whose worker holds this segment's large objects. A
    /// segment built by [`ResultSegment::compact`] holds none and names no
    /// session that exists.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }
}

/// The batch's own buffers, kept until every column has been copied out of
/// them, then freed together.
///
/// A column whose buffers have spare capacity is **copied** into exact
/// allocations rather than shrunk in place, and its original is only freed
/// once the whole batch is copied. Both halves matter to what the process
/// really holds, which the byte cap cannot see (ADR-0004 accepted limitation
/// 2). Measured on `text5date2` at 1,000,000 rows, private bytes over the
/// accounted ones (`phase-1-m5-2-data/README.md`):
///
/// - `shrink_to_fit` in place: +59%. Each buffer's spare tail becomes a free
///   fragment between retained segments that the next batch's growing
///   buffers cannot use.
/// - Copying, freeing each column's original at once: +14–17%. The next
///   column's copy lands inside the block just freed and splits it.
/// - Copying while the whole batch is alive, then freeing it: the copies sit
///   side by side, and the batch's buffers come back as one region the next
///   batch is built in.
type Spent = Vec<ColumnData>;

/// A vector holding exactly its contents: moved in when it has no spare
/// capacity, otherwise copied, with the original kept in `spent`.
fn exact<T: Clone>(values: Vec<T>, spent: &mut Spent, wrap: fn(Vec<T>) -> ColumnData) -> Vec<T> {
    if values.capacity() == values.len() {
        return values;
    }
    let copy = values.as_slice().to_vec();
    spent.push(wrap(values));
    copy
}

/// [`exact`] for a text column: its buffer and its offsets.
fn exact_text(
    values: TextColumn,
    spent: &mut Spent,
    wrap: fn(TextColumn) -> ColumnData,
) -> TextColumn {
    if values.heap_bytes() == values.buffer().len() + size_of_val(values.offsets()) {
        return values;
    }
    // A derived `Clone` allocates each vector at exactly its length.
    let copy = values.clone();
    spent.push(wrap(values));
    copy
}

/// [`exact`] for a bytes column.
fn exact_bytes(values: BytesColumn, spent: &mut Spent) -> BytesColumn {
    if values.heap_bytes() == values.buffer().len() + size_of_val(values.offsets()) {
        return values;
    }
    let copy = values.clone();
    spent.push(ColumnData::Bytes(values));
    copy
}

/// Chooses how a `NUMBER` column of one segment is stored: scaled when every
/// non-NULL value scales exactly at one common scale, otherwise unchanged.
fn compact_numbers(values: Vec<Number>, nulls: &NullMask, spent: &mut Spent) -> NumberStorage {
    let mut scale = 0_u8;
    for (row, value) in values.iter().enumerate() {
        if nulls.is_null(row) {
            continue;
        }
        match ScaledNumber::required_scale(value) {
            Some(needed) => scale = scale.max(needed),
            None => return NumberStorage::Decimal(exact(values, spent, ColumnData::Number)),
        }
    }
    let mut scaled = Vec::with_capacity(values.len());
    for (row, value) in values.iter().enumerate() {
        if nulls.is_null(row) {
            scaled.push(0);
            continue;
        }
        match ScaledNumber::from_number(value, scale) {
            Some(cell) => scaled.push(cell.mantissa()),
            // One value whose mantissa does not fit an i64 at this scale
            // keeps the whole segment column lossless (ADR-0004 accepted
            // limitation 9).
            None => return NumberStorage::Decimal(exact(values, spent, ColumnData::Number)),
        }
    }
    spent.push(ColumnData::Number(values));
    NumberStorage::Scaled {
        values: scaled,
        scale,
    }
}

/// Compacts one batch into a segment, parking every large-object locator
/// through `park`, which returns the id the cell keeps (never 0).
///
/// Runs on the worker thread that owns the connection: `park` is how a
/// locator stays there. On an error the locators already parked stay parked,
/// scoped to their result, and are released with it.
pub(crate) fn compact_batch(
    session: SessionId,
    batch: RowBatch,
    mut park: impl FnMut(usize, LobLocator) -> DbResult<u64>,
) -> DbResult<ResultSegment> {
    let rows = batch.row_count();
    let mut columns = Vec::with_capacity(batch.column_count());
    let mut spent = Spent::with_capacity(batch.column_count());
    for column in batch.into_columns() {
        let (data, nulls) = column.into_parts();
        let storage = match data {
            ColumnData::Boolean(values) => {
                Storage::Boolean(exact(values, &mut spent, ColumnData::Boolean))
            }
            ColumnData::Number(values) => {
                Storage::Number(compact_numbers(values, &nulls, &mut spent))
            }
            ColumnData::Float(values) => {
                Storage::Float(exact(values, &mut spent, ColumnData::Float))
            }
            ColumnData::Double(values) => {
                Storage::Double(exact(values, &mut spent, ColumnData::Double))
            }
            ColumnData::Text(values) => {
                Storage::Text(exact_text(values, &mut spent, ColumnData::Text))
            }
            ColumnData::Bytes(values) => Storage::Bytes(exact_bytes(values, &mut spent)),
            ColumnData::Timestamp(values) => {
                Storage::Timestamp(exact(values, &mut spent, ColumnData::Timestamp))
            }
            ColumnData::Json(values) => {
                Storage::Json(exact_text(values, &mut spent, ColumnData::Json))
            }
            ColumnData::Unsupported(values) => {
                Storage::Unsupported(exact_text(values, &mut spent, ColumnData::Unsupported))
            }
            ColumnData::Lob(locators) => {
                let mut ids = Vec::with_capacity(locators.len());
                for (row, locator) in locators.into_iter().enumerate() {
                    match locator {
                        // A NULL cell carries no locator. Nothing has taken
                        // one from a non-NULL cell on this path, so `None`
                        // there would be a driver breaking the contract; its
                        // id 0 names no parked object and reads fail.
                        None => ids.push(0),
                        Some(locator) => ids.push(park(row, locator)?),
                    }
                }
                Storage::Lob(ids)
            }
            // `ColumnData` is `#[non_exhaustive]`: a storage kind this core
            // does not know cannot be retained faithfully, so say so rather
            // than guess.
            other => {
                return Err(DbError::internal(format!(
                    "reldex-db-core: the result store cannot retain a {:?} column",
                    other.kind()
                )));
            }
        };
        columns.push(SegmentColumn { nulls, storage });
    }
    let columns = columns.into_boxed_slice();
    // Every copy is made: the batch's buffers go back together.
    drop(spent);
    let accounted_bytes = size_of::<ResultSegment>()
        // The `Arc`'s two counters.
        + 2 * size_of::<usize>()
        + columns.len() * size_of::<SegmentColumn>()
        + columns.iter().map(SegmentColumn::heap_bytes).sum::<usize>();
    Ok(ResultSegment {
        session,
        rows,
        columns,
        accounted_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::{CellValue, ResultSegment, SegmentData};
    use crate::store::scaled::NumberValue;
    use reldex_db_driver_api::{
        Column, ColumnData, NullMask, Number, RowBatch, TextColumn, Timestamp,
    };

    fn number(text: &str) -> Number {
        text.parse().expect("valid")
    }

    fn numbers(values: &[Option<&str>]) -> Column {
        let mut nulls = NullMask::new(values.len());
        let mut data = Vec::with_capacity(4096);
        for (row, value) in values.iter().enumerate() {
            match value {
                Some(text) => data.push(number(text)),
                None => {
                    nulls.set_null(row).expect("in range");
                    data.push(Number::ZERO);
                }
            }
        }
        Column::new(ColumnData::Number(data), nulls).expect("aligned")
    }

    #[test]
    fn a_number_column_scales_when_every_value_is_exact() {
        let batch = RowBatch::new(vec![numbers(&[
            Some("1.5"),
            None,
            Some("-0.25"),
            Some("7"),
        ])])
        .expect("batch");
        let segment = ResultSegment::compact(batch).expect("no LOBs");
        let column = segment.column(0).expect("column");
        let SegmentData::ScaledNumber { values, scale } = column.data() else {
            panic!("expected a scaled column, got {:?}", column.data());
        };
        assert_eq!(scale, 2);
        assert_eq!(values, &[150, 0, -25, 700]);
        assert_eq!(values.len(), 4, "sized exactly, not the driver's 4096");
        let text: Vec<String> = (0..4)
            .map(|row| match segment.value(row, 0).expect("cell") {
                CellValue::Number(value) => value.to_string(),
                CellValue::Null => "NULL".to_owned(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(text, ["1.5", "NULL", "-0.25", "7"]);
    }

    #[test]
    fn one_inexact_value_keeps_its_segment_column_lossless() {
        let quotient = "0.1428571428571428571428571428571428571429";
        let batch = RowBatch::new(vec![numbers(&[Some("1"), Some(quotient)])]).expect("batch");
        let segment = ResultSegment::compact(batch).expect("no LOBs");
        let column = segment.column(0).expect("column");
        assert!(matches!(column.data(), SegmentData::Number(values) if values.len() == 2));
        assert_eq!(
            segment.value(1, 0),
            Some(CellValue::Number(NumberValue::Decimal(&number(quotient))))
        );
    }

    #[test]
    fn text_keeps_its_layout_and_loses_the_driver_slack() {
        let mut text = TextColumn::with_capacity(1_000, 16_000);
        text.push("row 1");
        text.push("ข้อมูล");
        let slack = text.heap_bytes();
        let stamp = Timestamp::from_source(2026, 1, 2, 3, 4, 5).expect("valid");
        let mut stamps = Vec::with_capacity(1_000);
        stamps.extend([stamp, stamp]);
        let batch = RowBatch::new(vec![
            Column::not_null(ColumnData::Text(text)),
            Column::not_null(ColumnData::Timestamp(stamps)),
        ])
        .expect("batch");
        let segment = ResultSegment::compact(batch).expect("no LOBs");
        let SegmentData::Text(kept) = segment.column(0).expect("text").data() else {
            panic!("text column");
        };
        assert!(kept.heap_bytes() < slack);
        assert_eq!(kept.get(1), Some("ข้อมูล"));
        assert_eq!(segment.value(0, 1), Some(CellValue::Timestamp(stamp)));
        assert!(
            segment.accounted_bytes() < 1_000,
            "{}",
            segment.accounted_bytes()
        );
        assert!(segment.value(2, 0).is_none(), "row out of range");
        assert!(segment.value(0, 2).is_none(), "column out of range");
    }
}
