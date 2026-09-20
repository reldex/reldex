//! Batch views: what the grid reads, and the one place zero-copy actually
//! matters (ADR-0003 D4).
//!
//! # What the layout really is
//!
//! `RowBatch` is column-oriented, and `TextColumn`/`BytesColumn` are literally
//! *one contiguous buffer plus a `Vec<usize>` of offsets* — the exact shape a C
//! consumer wants. `NullMask` is a `Vec<u64>` of LSB-first bits. So text,
//! JSON, bytes and the `Unsupported` rendering cross **borrowed**: one
//! [`reldex_batch_column`] call per column per batch, then pointer arithmetic
//! per cell, with no per-cell FFI call and no copy.
//!
//! `Number` and `Timestamp` are Rust-layout types, so they cross as
//! `#[repr(C)]` mirrors, built once per column on first use and cached in the
//! batch. That is the deliberate copy ADR-0003 D4 describes: a `Number` is
//! ~46 B, a visible window is a few thousand cells, and the alternative is
//! constraining ADR-0002's contract types to a C layout.
//!
//! `bool`, `f32` and `f64` columns need neither: `Vec<f64>` *is* a `double[]`.

use std::cell::RefCell;
use std::sync::Arc;

use reldex_db_core::{ColumnMetadata, FetchedBatch};
use reldex_db_driver_api::{
    Column, ColumnData, ColumnKind, MAX_SIGNIFICANT_DIGITS, Number, SqlType, Timestamp,
};

use crate::error::set_last_argument_error;
use crate::status::{ReldexStatus, entry, entry_value};
use crate::strings::{CStruct, ReldexStr, write_out_struct};

/// The storage family of a column, which decides which fields of
/// [`ReldexColumnView`] are set.
///
/// `0` is reserved for a kind this header predates (ADR-0003 D7).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexColumnKind {
    /// A kind this header does not know. Read such a column through
    /// [`crate::reldex_batch_format_column`], which always has an answer.
    Unknown = 0,
    /// `bool`: `fixed` is a `bool[]`.
    Boolean = 1,
    /// Exact decimals: `fixed` is a [`ReldexNumber`] array.
    Number = 2,
    /// `fixed` is a `float[]`.
    Float = 3,
    /// `fixed` is a `double[]`.
    Double = 4,
    /// UTF-8 text: `data` + `offsets`, borrowed.
    Text = 5,
    /// Raw bytes: `data` + `offsets`, borrowed.
    Bytes = 6,
    /// Dates and timestamps: `fixed` is a [`ReldexTimestamp`] array.
    Timestamp = 7,
    /// JSON text: `data` + `offsets`, borrowed.
    Json = 8,
    /// An unread large object. No data crosses: the locator stays on the
    /// session's worker thread and a non-NULL row reads as *taken*, which is
    /// not NULL and must not be shown as one. Reading LOBs is M2.11.
    Lob = 9,
    /// A type the contract cannot represent, rendered by the driver as
    /// best-effort text: `data` + `offsets`, borrowed. Not text — never show
    /// it as character data.
    Unsupported = 10,
}

impl From<ColumnKind> for ReldexColumnKind {
    fn from(kind: ColumnKind) -> Self {
        match kind {
            ColumnKind::Boolean => Self::Boolean,
            ColumnKind::Number => Self::Number,
            ColumnKind::Float => Self::Float,
            ColumnKind::Double => Self::Double,
            ColumnKind::Text => Self::Text,
            ColumnKind::Bytes => Self::Bytes,
            ColumnKind::Timestamp => Self::Timestamp,
            ColumnKind::Json => Self::Json,
            ColumnKind::Lob => Self::Lob,
            ColumnKind::Unsupported => Self::Unsupported,
            // `ColumnKind` is `#[non_exhaustive]`.
            _ => Self::Unknown,
        }
    }
}

/// Whether a column accepts NULLs, when the server said.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexNullable {
    /// The driver did not report it. Not the same as "NOT NULL".
    Unknown = 0,
    /// The column accepts NULLs.
    Yes = 1,
    /// The column does not.
    No = 2,
}

/// How many significant digits a [`ReldexNumber`] carries.
///
/// Written as a literal so the generated header can use it as an array bound;
/// the assertion below keeps it equal to the contract's own constant, so the
/// two can never drift (ADR-0002 amendment S6 raised it from 38 to 40).
pub const RELDEX_NUMBER_MAX_DIGITS: usize = 40;

const _: () = assert!(
    RELDEX_NUMBER_MAX_DIGITS == MAX_SIGNIFICANT_DIGITS,
    "ReldexNumber must mirror the contract's digit capacity exactly"
);

/// An exact decimal, mirroring `db-driver-api`'s `Number` field for field.
///
/// The value is `±0.d₁…dₙ × 10^exponent`, with `digits[0..digit_count]` holding
/// digit *values* `0..=9`, most significant first. 40 significant digits, per
/// ADR-0002 amendment S6.
///
/// Deliberately **without** a `struct_size`: it is an array element, and its
/// size is reported once in [`ReldexColumnView::fixed_stride`]. Compare that
/// against `sizeof(ReldexNumber)` — a mismatch means the header and the
/// library disagree, which is exactly what `struct_size` would have caught.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexNumber {
    /// Significant digits, most significant first. Positions at or past
    /// `digit_count` are zero.
    pub digits: [u8; RELDEX_NUMBER_MAX_DIGITS],
    /// How many of `digits` are significant. Zero means the value is zero.
    pub digit_count: u8,
    /// The decimal exponent.
    pub exponent: i16,
    /// Whether the value is negative.
    pub negative: bool,
}

impl From<&Number> for ReldexNumber {
    fn from(value: &Number) -> Self {
        let mut digits = [0_u8; RELDEX_NUMBER_MAX_DIGITS];
        let source = value.digits();
        let count = source.len().min(RELDEX_NUMBER_MAX_DIGITS);
        digits[..count].copy_from_slice(&source[..count]);
        Self {
            digits,
            digit_count: u8::try_from(count).unwrap_or(u8::MAX),
            exponent: value.exponent(),
            negative: value.is_negative(),
        }
    }
}

/// A date or timestamp, mirroring `db-driver-api`'s `Timestamp`.
///
/// Without a `struct_size`, for the same reason as [`ReldexNumber`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexTimestamp {
    /// Nanoseconds within the second.
    pub nanosecond: u32,
    /// Proleptic year; negative years are BC in astronomical numbering.
    pub year: i16,
    /// Minutes east of UTC, when `has_zone`.
    pub zone_offset_minutes: i16,
    /// 1-12.
    pub month: u8,
    /// 1-31.
    pub day: u8,
    /// 0-23.
    pub hour: u8,
    /// 0-59.
    pub minute: u8,
    /// 0-59.
    pub second: u8,
    /// Whether `zone_offset_minutes` is meaningful. A `DATE` has no zone.
    pub has_zone: bool,
}

impl From<&Timestamp> for ReldexTimestamp {
    fn from(value: &Timestamp) -> Self {
        Self {
            nanosecond: value.nanosecond(),
            year: value.year(),
            zone_offset_minutes: value.zone().offset_minutes().unwrap_or(0),
            month: value.month(),
            day: value.day(),
            hour: value.hour(),
            minute: value.minute(),
            second: value.second(),
            has_zone: value.zone().offset_minutes().is_some(),
        }
    }
}

/// What one column of a batch looks like in memory.
///
/// Every pointer here **borrows from the batch** and is valid until
/// [`reldex_batch_release`]. Which pointers are set depends on `kind`; see
/// [`ReldexColumnKind`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexColumnView {
    /// `sizeof(ReldexColumnView)` on the way in; how much is valid on the way
    /// out.
    pub struct_size: u32,
    /// A [`ReldexColumnKind`].
    pub kind: i32,
    /// How many rows the column holds. The same for every column of a batch.
    pub row_count: usize,
    /// The NULL bitmap: row `i` is bit `i % 64` of word `i / 64`, LSB-first,
    /// set when the row is SQL NULL. Null only when `null_word_count` is 0.
    pub null_bits: *const u64,
    /// How many `uint64_t` words `null_bits` covers.
    pub null_word_count: usize,
    /// Text/JSON/bytes/unsupported: the one contiguous value buffer.
    pub data: *const u8,
    /// How many bytes `data` covers.
    pub data_len: usize,
    /// Text/JSON/bytes/unsupported: `row_count + 1` byte offsets into `data`,
    /// so row `i` is `data[offsets[i]..offsets[i + 1]]`.
    pub offsets: *const usize,
    /// Fixed-width kinds: the element array.
    pub fixed: *const std::ffi::c_void,
    /// The size of one element of `fixed`. Compare with `sizeof` of the
    /// matching type before reading.
    pub fixed_stride: usize,
    /// How many elements `fixed` covers; equals `row_count`.
    pub fixed_len: usize,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every field an integer or a raw
// pointer — all valid as zero.
unsafe impl CStruct for ReldexColumnView {
    const MIN_SIZE: usize = size_of::<Self>();
}

impl Default for ReldexColumnView {
    /// An empty view with `struct_size` set, which is the shape a caller is
    /// expected to pass in.
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            kind: ReldexColumnKind::Unknown as i32,
            row_count: 0,
            null_bits: std::ptr::null(),
            null_word_count: 0,
            data: std::ptr::null(),
            data_len: 0,
            offsets: std::ptr::null(),
            fixed: std::ptr::null(),
            fixed_stride: 0,
            fixed_len: 0,
        }
    }
}

/// What a column *is*, as opposed to how it is stored: the header a grid puts
/// above it.
///
/// `name` and `native_type_name` borrow from the batch.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexColumnInfo {
    /// `sizeof(ReldexColumnInfo)` on the way in; how much is valid on the way
    /// out.
    pub struct_size: u32,
    /// A [`ReldexColumnKind`] — how the batch stores this column.
    pub kind: i32,
    /// A [`ReldexNullable`].
    pub nullable: i32,
    /// Declared precision, when `has_precision`.
    pub precision: i32,
    /// Declared scale, when `has_scale`. May be negative.
    pub scale: i32,
    /// Declared maximum size in bytes, when `has_max_size_bytes`.
    pub max_size_bytes: u32,
    /// Whether `precision` is set.
    pub has_precision: bool,
    /// Whether `scale` is set.
    pub has_scale: bool,
    /// Whether `max_size_bytes` is set.
    pub has_max_size_bytes: bool,
    /// The column's name, as the server gave it.
    pub name: ReldexStr,
    /// The server's own type name, when the driver reported one — required
    /// reading for an `Unsupported` column, which is where the real type hides.
    pub native_type_name: ReldexStr,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, integers, `bool`s and
// `ReldexStr`s — all valid as zero.
unsafe impl CStruct for ReldexColumnInfo {
    const MIN_SIZE: usize = size_of::<Self>();
}

impl Default for ReldexColumnInfo {
    /// An empty description with `struct_size` set.
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            kind: ReldexColumnKind::Unknown as i32,
            nullable: ReldexNullable::Unknown as i32,
            precision: 0,
            scale: 0,
            max_size_bytes: 0,
            has_precision: false,
            has_scale: false,
            has_max_size_bytes: false,
            name: ReldexStr::empty(),
            native_type_name: ReldexStr::empty(),
        }
    }
}

/// A `#[repr(C)]` copy of a fixed-width column, built once and kept for the
/// batch's lifetime.
enum Mirror {
    Numbers(Box<[ReldexNumber]>),
    Timestamps(Box<[ReldexTimestamp]>),
}

/// One fetched batch, owned by the caller from the moment its event is handed
/// out until [`reldex_batch_release`].
///
/// Opaque. Every pointer any function here returns borrows from it.
pub struct ReldexBatch {
    batch: FetchedBatch,
    columns: Arc<[ColumnMetadata]>,
    /// Lazily built `#[repr(C)]` copies of the fixed-width columns. Each is a
    /// separate allocation, so handing out a pointer into one and then
    /// building another cannot move the first.
    mirrors: RefCell<Vec<Option<Mirror>>>,
}

impl ReldexBatch {
    pub(crate) fn new(batch: FetchedBatch, columns: Arc<[ColumnMetadata]>) -> Self {
        let column_count = batch.column_count();
        Self {
            batch,
            columns,
            mirrors: RefCell::new((0..column_count).map(|_| None).collect()),
        }
    }

    pub(crate) fn rows(&self) -> &FetchedBatch {
        &self.batch
    }

    /// Builds (once) and borrows the `#[repr(C)]` mirror of a fixed-width
    /// column, as a raw pointer and an element count.
    fn mirror_of(&self, index: usize, column: &Column) -> Option<(*const std::ffi::c_void, usize)> {
        let mut mirrors = self.mirrors.borrow_mut();
        let slot = mirrors.get_mut(index)?;
        if slot.is_none() {
            *slot = match column.data() {
                ColumnData::Number(values) => Some(Mirror::Numbers(
                    values.iter().map(ReldexNumber::from).collect(),
                )),
                ColumnData::Timestamp(values) => Some(Mirror::Timestamps(
                    values.iter().map(ReldexTimestamp::from).collect(),
                )),
                _ => return None,
            };
        }
        match slot.as_ref()? {
            Mirror::Numbers(values) => Some((values.as_ptr().cast(), values.len())),
            Mirror::Timestamps(values) => Some((values.as_ptr().cast(), values.len())),
        }
    }

    fn view_of(&self, index: usize) -> Option<ReldexColumnView> {
        let column = self.batch.column(index)?;
        let nulls = column.nulls().words();
        let mut view = ReldexColumnView {
            kind: ReldexColumnKind::from(column.kind()) as i32,
            row_count: column.len(),
            null_bits: if nulls.is_empty() {
                std::ptr::null()
            } else {
                nulls.as_ptr()
            },
            null_word_count: nulls.len(),
            ..ReldexColumnView::default()
        };
        match column.data() {
            ColumnData::Text(text) | ColumnData::Json(text) | ColumnData::Unsupported(text) => {
                view.data = text.buffer().as_ptr();
                view.data_len = text.buffer().len();
                view.offsets = text.offsets().as_ptr();
            }
            ColumnData::Bytes(bytes) => {
                view.data = bytes.buffer().as_ptr();
                view.data_len = bytes.buffer().len();
                view.offsets = bytes.offsets().as_ptr();
            }
            ColumnData::Boolean(values) => {
                view.fixed = values.as_ptr().cast();
                view.fixed_stride = size_of::<bool>();
                view.fixed_len = values.len();
            }
            ColumnData::Float(values) => {
                view.fixed = values.as_ptr().cast();
                view.fixed_stride = size_of::<f32>();
                view.fixed_len = values.len();
            }
            ColumnData::Double(values) => {
                view.fixed = values.as_ptr().cast();
                view.fixed_stride = size_of::<f64>();
                view.fixed_len = values.len();
            }
            ColumnData::Number(_) => {
                let (ptr, len) = self.mirror_of(index, column)?;
                view.fixed = ptr;
                view.fixed_stride = size_of::<ReldexNumber>();
                view.fixed_len = len;
            }
            ColumnData::Timestamp(_) => {
                let (ptr, len) = self.mirror_of(index, column)?;
                view.fixed = ptr;
                view.fixed_stride = size_of::<ReldexTimestamp>();
                view.fixed_len = len;
            }
            // A LOB column carries no data across: its locators were parked on
            // the worker thread and the cells read as *taken*. An unknown kind
            // carries none either, by construction.
            _ => {}
        }
        Some(view)
    }

    fn info_of(&self, index: usize) -> Option<ReldexColumnInfo> {
        let column = self.batch.column(index)?;
        let metadata = self.columns.get(index);
        let mut info = ReldexColumnInfo {
            kind: ReldexColumnKind::from(column.kind()) as i32,
            ..ReldexColumnInfo::default()
        };
        if let Some(metadata) = metadata {
            info.nullable = match metadata.nullable() {
                Some(true) => ReldexNullable::Yes as i32,
                Some(false) => ReldexNullable::No as i32,
                None => ReldexNullable::Unknown as i32,
            };
            info.precision = metadata.precision().map_or(0, i32::from);
            info.has_precision = metadata.precision().is_some();
            info.scale = metadata.scale().map_or(0, i32::from);
            info.has_scale = metadata.scale().is_some();
            info.max_size_bytes = metadata.max_size_bytes().unwrap_or(0);
            info.has_max_size_bytes = metadata.max_size_bytes().is_some();
            info.name = ReldexStr::borrow_bytes(metadata.name().as_bytes());
            if let Some(native) = metadata.native_type_name() {
                info.native_type_name = ReldexStr::borrow_bytes(native.as_bytes());
            } else if metadata.sql_type() == SqlType::Unsupported {
                // The contract requires a native name here; say so rather than
                // leaving the UI to guess what it is showing.
                info.native_type_name = ReldexStr::empty();
            }
        }
        Some(info)
    }
}

/// Runs `body` with the batch the caller named.
///
/// # Safety
///
/// `batch` must be null, or a live pointer from a fetched event that has not
/// been released.
unsafe fn with_batch<T>(
    batch: *const ReldexBatch,
    body: impl FnOnce(&ReldexBatch) -> T,
) -> Option<T> {
    if batch.is_null() || !batch.is_aligned() {
        return None;
    }
    // SAFETY: the caller promises the pointer is live and not released; the
    // borrow ends with this call, and ADR-0003 D5 rule 3 keeps the caller
    // single-threaded, so nothing can release it meanwhile.
    Some(body(unsafe { &*batch }))
}

/// How many rows the batch holds. Zero means the result is exhausted.
///
/// # Safety
///
/// `batch` must be null (reported as 0) or a live batch.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_batch_row_count(batch: *const ReldexBatch) -> usize {
    entry_value(0, || {
        // SAFETY: delegated to this function's contract.
        unsafe { with_batch(batch, |batch| batch.rows().row_count()) }.unwrap_or(0)
    })
}

/// How many columns the batch holds.
///
/// # Safety
///
/// `batch` must be null (reported as 0) or a live batch.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_batch_column_count(batch: *const ReldexBatch) -> usize {
    entry_value(0, || {
        // SAFETY: delegated to this function's contract.
        unsafe { with_batch(batch, |batch| batch.rows().column_count()) }.unwrap_or(0)
    })
}

/// Describes column `column`: its name, declared type and storage kind.
///
/// The strings borrow from the batch.
///
/// # Safety
///
/// `batch` must be a live batch and `out` a writable [`ReldexColumnInfo`] with
/// `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_batch_column_info(
    batch: *const ReldexBatch,
    column: usize,
    out: *mut ReldexColumnInfo,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `batch`.
        let info = unsafe { with_batch(batch, |batch| batch.info_of(column)) };
        match info {
            None => set_last_argument_error("reldex_batch_column_info: `batch` is null"),
            Some(None) => ReldexStatus::NotFound,
            Some(Some(info)) => {
                // SAFETY: delegated to this function's contract for `out`.
                if unsafe { write_out_struct(out, info) } {
                    ReldexStatus::Ok
                } else {
                    set_last_argument_error(
                        "reldex_batch_column_info: `out` is null, unaligned, or too small",
                    )
                }
            }
        }
    })
}

/// Hands out column `column`'s memory: one call per column per batch, then
/// pointer arithmetic per cell.
///
/// Every pointer in `out` borrows from `batch` and is valid until
/// [`reldex_batch_release`]. Read a cell by consulting `null_bits` first, then
/// `offsets`/`data` or `fixed` according to `kind`.
///
/// # Safety
///
/// `batch` must be a live batch and `out` a writable [`ReldexColumnView`] with
/// `struct_size` set. The returned pointers must not be used after the batch
/// is released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_batch_column(
    batch: *const ReldexBatch,
    column: usize,
    out: *mut ReldexColumnView,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `batch`.
        let view = unsafe { with_batch(batch, |batch| batch.view_of(column)) };
        match view {
            None => set_last_argument_error("reldex_batch_column: `batch` is null"),
            Some(None) => ReldexStatus::NotFound,
            Some(Some(view)) => {
                // SAFETY: delegated to this function's contract for `out`.
                if unsafe { write_out_struct(out, view) } {
                    ReldexStatus::Ok
                } else {
                    set_last_argument_error(
                        "reldex_batch_column: `out` is null, unaligned, or too small",
                    )
                }
            }
        }
    })
}

/// Releases a batch and every pointer taken from it.
///
/// A null pointer is a no-op. Releasing twice is undefined behaviour, like
/// every `free`. The adapter must release every batch it takes from an event;
/// a batch still sitting in the queue when the hub is destroyed is released
/// for it.
///
/// # Safety
///
/// `batch` must be null, or a pointer from a fetched event that has not been
/// released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_batch_release(batch: *mut ReldexBatch) {
    entry_value((), || {
        if batch.is_null() {
            return;
        }
        // SAFETY: the caller promises this came from `Box::into_raw` in this
        // library and has not been released.
        drop(unsafe { Box::from_raw(batch) });
    });
}
