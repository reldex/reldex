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
//!
//! # What is thread-safe about a batch, exactly
//!
//! A `ReldexBatch*` is **`Sync`**, and the assertion below makes the compiler
//! keep it that way. Concurrently calling the read-only functions
//! ([`reldex_batch_row_count`], [`reldex_batch_column_count`],
//! [`reldex_batch_column_info`], [`reldex_batch_column`]) on one batch from
//! several threads is sound, including the first call on a `NUMBER` or
//! `TIMESTAMP` column, which builds that column's mirror: the mirrors are
//! `OnceLock`s, so exactly one thread builds each and every thread gets the
//! same pointer.
//!
//! What is **not** safe, and what the adapter must serialize:
//!
//! * [`reldex_batch_release`] against any other use of the same batch. Release
//!   consumes it; a call racing it is a use-after-free like any other.
//! * [`crate::reldex_batch_format_column`] against another call on the same
//!   **arena**: it takes the arena by `&mut`. Two threads may format from one
//!   batch at the same time only if each has its own arena.
//!
//! ADR-0003 D5 rule 3 still says the adapter makes these calls from the Qt
//! main thread. This paragraph is about what the library *guarantees*, so that
//! a worker thread rendering into its own arena is a design choice rather than
//! a latent race.

use std::sync::{Arc, OnceLock};

use reldex_db_core::{ColumnMetadata, FetchedBatch};
use reldex_db_driver_api::{
    Column, ColumnData, ColumnKind, MAX_SIGNIFICANT_DIGITS, Number, Timestamp,
};

use crate::error::{set_last_argument_error, set_last_error};
use crate::status::{ReldexStatus, entry, entry_value};
use crate::strings::{CStruct, OwnedStr, ReldexStr, write_out_struct};

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
    ///
    /// **What a grid should do:** route the column through
    /// [`crate::reldex_batch_format_column`] like any other non-text kind,
    /// rather than reading `data`/`offsets` directly as UTF-8. The bytes are
    /// the driver's rendering, not necessarily valid UTF-8 and not necessarily
    /// the user's idea of the value, so the formatter's escaping — and the
    /// fact that it yields the same "cannot be shown faithfully" form
    /// everywhere — is what keeps a `SELECT *` over a table with one
    /// `INTERVAL` or `XMLType` column honest instead of silently wrong. The
    /// column's `native_type_name` is what to show the user when asked what
    /// the type actually is.
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
    /// The element array, for a fixed-width kind whose Rust storage already
    /// *is* a C array: `BOOLEAN` (`bool[]`), `FLOAT` (`float[]`), `DOUBLE`
    /// (`double[]`). Borrowed, zero-copy, no allocation.
    ///
    /// **Always test `fixed != NULL` before indexing it.** It is NULL for
    /// `NUMBER` and `TIMESTAMP`, whose Rust storage is not C-compatible and
    /// has to be mirrored: describing a column never builds that mirror. Ask
    /// for it explicitly with [`reldex_batch_column_fixed`], which says what
    /// it costs (ADR-0003 A19). Most callers should not — the bulk formatter
    /// reads the *source* column and needs no mirror at all. It is NULL for
    /// every variable-width kind too, which uses `data`/`offsets`.
    ///
    /// Element widths, so a cast is never guessed at: `BOOLEAN` is **one byte
    /// per element and strictly 0 or 1** — never another non-zero value, so a
    /// byte compare is as valid as a truth test — `FLOAT` is four, `DOUBLE`
    /// eight, and the two mirrored kinds report `sizeof(ReldexNumber)` and
    /// `sizeof(ReldexTimestamp)`. `fixed_stride` says the same thing at run
    /// time, and is set even when `fixed` is NULL.
    pub fixed: *const std::ffi::c_void,
    /// The size of one element of `fixed`. Reported for `NUMBER` and
    /// `TIMESTAMP` too, even though `fixed` is NULL there, so a caller can
    /// check it against its own `sizeof` before asking for the elements.
    pub fixed_stride: usize,
    /// How many elements `fixed` covers; equals `row_count` when `fixed` is
    /// set, and `0` when it is NULL.
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

/// One column's description, with its strings already copied into the
/// NUL-terminated form the boundary promises.
struct ColumnDescription {
    metadata: ColumnMetadata,
    /// `ColumnMetadata::name()` borrows a `Box<str>`, which is **not**
    /// NUL-terminated — reading `ptr[len]` on one is a byte past the
    /// allocation. The copy happens here, once per result set, and every batch
    /// that result produces shares it through an `Arc`.
    name: OwnedStr,
    native_type_name: Option<OwnedStr>,
}

/// One result set's columns, built when the statement executes and shared by
/// every batch it produces.
///
/// The sharing is the point: a million-row result arrives as a thousand
/// batches, and the column names are copied **once** for all of them, not once
/// per batch and certainly not once per cell.
pub(crate) struct ResultColumns {
    columns: Box<[ColumnDescription]>,
}

impl ResultColumns {
    pub(crate) fn new(metadata: Vec<ColumnMetadata>) -> Arc<Self> {
        Arc::new(Self {
            columns: metadata
                .into_iter()
                .map(|metadata| ColumnDescription {
                    name: OwnedStr::new(metadata.name().to_owned()),
                    native_type_name: metadata
                        .native_type_name()
                        .map(|native| OwnedStr::new(native.to_owned())),
                    metadata,
                })
                .collect(),
        })
    }

    fn get(&self, index: usize) -> Option<&ColumnDescription> {
        self.columns.get(index)
    }

    pub(crate) fn len(&self) -> usize {
        self.columns.len()
    }

    /// Describes column `index` from the metadata alone.
    ///
    /// `kind` is the storage family the column's **declared** type maps to.
    /// [`reldex_batch_column_info`] reports the storage the driver actually
    /// used for one batch; the two agree for every type the ADR-0002 contract
    /// can represent, and where they cannot — a driver that fell back to
    /// `Unsupported` text for a column it declared as something else — the
    /// batch is the one telling the truth about the bytes.
    pub(crate) fn info(&self, index: usize) -> Option<ReldexColumnInfo> {
        let described = self.get(index)?;
        let metadata = &described.metadata;
        Some(ReldexColumnInfo {
            kind: ReldexColumnKind::from(metadata.sql_type()) as i32,
            nullable: match metadata.nullable() {
                Some(true) => ReldexNullable::Yes as i32,
                Some(false) => ReldexNullable::No as i32,
                None => ReldexNullable::Unknown as i32,
            },
            precision: metadata.precision().map_or(0, i32::from),
            has_precision: metadata.precision().is_some(),
            scale: metadata.scale().map_or(0, i32::from),
            has_scale: metadata.scale().is_some(),
            max_size_bytes: metadata.max_size_bytes().unwrap_or(0),
            has_max_size_bytes: metadata.max_size_bytes().is_some(),
            name: described.name.as_reldex_str(),
            // An `Unsupported` column with no native name is a driver that did
            // not keep its side of the ADR-0002 contract. The empty string is
            // the honest answer: the UI shows "unknown type" rather than
            // inventing one.
            native_type_name: described
                .native_type_name
                .as_ref()
                .map_or_else(ReldexStr::empty, OwnedStr::as_reldex_str),
            ..ReldexColumnInfo::default()
        })
    }
}

/// The storage family a declared type maps to.
///
/// Used only where there is no batch to ask — [`crate::
/// reldex_session_result_column`], which answers as soon as `EXECUTED` is
/// drained and therefore before any rows exist.
impl From<reldex_db_driver_api::SqlType> for ReldexColumnKind {
    fn from(sql_type: reldex_db_driver_api::SqlType) -> Self {
        use reldex_db_driver_api::SqlType;
        match sql_type {
            SqlType::Boolean => Self::Boolean,
            SqlType::Number => Self::Number,
            SqlType::BinaryFloat => Self::Float,
            SqlType::BinaryDouble => Self::Double,
            SqlType::Text { .. } => Self::Text,
            SqlType::Date | SqlType::Timestamp | SqlType::TimestampWithTimeZone => Self::Timestamp,
            SqlType::Raw => Self::Bytes,
            SqlType::CharacterLob { .. } | SqlType::BinaryLob => Self::Lob,
            SqlType::Json => Self::Json,
            // A `REF CURSOR` column has no cell data of its own, and an
            // unrepresentable type crosses as the driver's best-effort text.
            SqlType::Cursor => Self::Unknown,
            SqlType::Unsupported => Self::Unsupported,
            // `SqlType` is `#[non_exhaustive]`.
            _ => Self::Unknown,
        }
    }
}

/// One fetched batch, owned by the caller from the moment its event is handed
/// out until [`reldex_batch_release`].
///
/// Opaque. Every pointer any function here returns borrows from it.
pub struct ReldexBatch {
    batch: FetchedBatch,
    columns: Arc<ResultColumns>,
    /// Lazily built `#[repr(C)]` copies of the fixed-width columns.
    ///
    /// A `OnceLock` per column rather than one `RefCell<Vec<_>>`: every
    /// read-only entry point takes `&ReldexBatch` and C has no way to promise
    /// it called from one thread, so the cache has to be sound under
    /// concurrent first use, not merely unlikely to be hit. Each mirror is
    /// also its own allocation, so building a later one cannot move a pointer
    /// already handed out for an earlier one.
    mirrors: Box<[OnceLock<Option<Mirror>>]>,
}

// The `Sync` bound is load-bearing (see this module's docs): the read-only
// entry points take `&ReldexBatch` and are documented as safe to call
// concurrently. If a future field is not `Sync`, this stops compiling rather
// than quietly making that documentation false.
const _: () = {
    const fn assert_sync<T: Sync>() {}
    assert_sync::<ReldexBatch>();
};

impl Drop for ReldexBatch {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::Batch);
    }
}

impl ReldexBatch {
    pub(crate) fn new(batch: FetchedBatch, columns: Arc<ResultColumns>) -> Self {
        let column_count = batch.column_count();
        crate::counters::created(crate::counters::Kind::Batch);
        Self {
            batch,
            columns,
            mirrors: (0..column_count).map(|_| OnceLock::new()).collect(),
        }
    }

    pub(crate) fn rows(&self) -> &FetchedBatch {
        &self.batch
    }

    /// Builds (once) and borrows the `#[repr(C)]` mirror of a fixed-width
    /// column, as a raw pointer and an element count.
    ///
    /// Called **only** from [`reldex_batch_column_fixed`]. Describing a column
    /// must never reach here: that is what made viewing a batch cost 62 bytes
    /// a row for elements nobody read (ADR-0003 A19).
    ///
    /// Concurrent first calls are sound: `OnceLock::get_or_init` runs the
    /// initializer on exactly one thread and every caller gets that value, so
    /// two threads asking for the same column see the same pointer.
    fn mirror_of(&self, index: usize, column: &Column) -> Option<(*const std::ffi::c_void, usize)> {
        let slot = self.mirrors.get(index)?;
        let mirror = slot.get_or_init(|| match column.data() {
            ColumnData::Number(values) => Some(Mirror::Numbers(
                values.iter().map(ReldexNumber::from).collect(),
            )),
            ColumnData::Timestamp(values) => Some(Mirror::Timestamps(
                values.iter().map(ReldexTimestamp::from).collect(),
            )),
            _ => None,
        });
        match mirror.as_ref()? {
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
            // The two mirrored kinds. `fixed` stays NULL here and the
            // mirror is *not* built: see `reldex_batch_column_fixed`, which is
            // the only thing that builds one. The stride is still reported, so
            // a caller that is about to ask for the elements can check its
            // `sizeof` first.
            ColumnData::Number(_) => {
                view.fixed_stride = size_of::<ReldexNumber>();
            }
            ColumnData::Timestamp(_) => {
                view.fixed_stride = size_of::<ReldexTimestamp>();
            }
            // A LOB column carries no data across: its locators were parked on
            // the worker thread and the cells read as *taken*. An unknown kind
            // carries none either, by construction.
            _ => {}
        }
        Some(view)
    }

    /// [`view_of`](Self::view_of), plus the mirror for the two kinds that
    /// need one. The only path that allocates.
    fn view_with_mirror(&self, index: usize) -> Option<ReldexColumnView> {
        let mut view = self.view_of(index)?;
        let column = self.batch.column(index)?;
        match column.data() {
            ColumnData::Number(_) | ColumnData::Timestamp(_) => {
                let (ptr, len) = self.mirror_of(index, column)?;
                view.fixed = ptr;
                view.fixed_len = len;
            }
            // Every other kind is already whatever it is going to be: the
            // C-compatible fixed kinds were borrowed directly by `view_of`,
            // and text, bytes and LOB have no element array at all.
            _ => {}
        }
        Some(view)
    }

    fn info_of(&self, index: usize) -> Option<ReldexColumnInfo> {
        let column = self.batch.column(index)?;
        // The shared description if the result has one, else just the kind:
        // the same answer `reldex_session_result_column` gives, except that
        // `kind` is this batch's *actual* storage rather than the declared
        // type's mapping.
        let mut info = self.columns.info(index).unwrap_or_default();
        info.kind = ReldexColumnKind::from(column.kind()) as i32;
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
    // SAFETY: the caller promises the pointer is live and not released, and
    // the borrow ends with this call. A *shared* borrow is all this takes, and
    // `ReldexBatch: Sync` (asserted above), so another thread holding one at
    // the same time is sound; what the caller's contract rules out is a
    // concurrent `reldex_batch_release`, which no amount of internal
    // synchronisation could make safe.
    Some(body(unsafe { &*batch }))
}

/// Records why a column index was refused, so a caller that got
/// `RELDEX_STATUS_NOT_FOUND` can find out from `reldex_last_error_take()`
/// rather than reading whatever error was left over from an earlier call.
fn column_not_found(what: &str, column: usize, count: usize) -> ReldexStatus {
    set_last_error(reldex_db_core::DbError::internal(format!(
        "reldex-ffi: {what}: column {column} is out of range; the batch has {count}"
    )));
    ReldexStatus::NotFound
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
/// **A terminal, zero-row batch holds none.** The batch that reports a result
/// exhausted carries no column storage, so this returns `0` and
/// [`reldex_batch_column`], [`reldex_batch_column_info`] and
/// [`reldex_batch_column_fixed`] all report `RELDEX_STATUS_NOT_FOUND` on it. A
/// caller that reads its headers from "whichever batch it has" must skip that
/// one — or, better, not read headers from batches at all and use
/// [`crate::reldex_session_result_column`], which answers from the `EXECUTED`
/// event and reports the same description every non-empty batch of the result
/// does.
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
/// `name` and `native_type_name` borrow from the batch and are
/// NUL-terminated, so they can be handed straight to `QString::fromUtf8` with
/// or without the length.
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
        let info = unsafe {
            with_batch(batch, |batch| {
                (batch.info_of(column), batch.rows().column_count())
            })
        };
        match info {
            None => set_last_argument_error("reldex_batch_column_info: `batch` is null"),
            Some((None, count)) => column_not_found("reldex_batch_column_info", column, count),
            Some((Some(info), _)) => {
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
/// **This allocates nothing and retains nothing.** Every pointer it hands back
/// already existed inside the batch. `NUMBER` and `TIMESTAMP` therefore come
/// back with `fixed == NULL`: their C mirror is built only by
/// [`reldex_batch_column_fixed`], which is where its cost is stated. Viewing
/// every column of every batch used to build those mirrors and keep them, at a
/// measured 62.0 bytes per row that no consumer read — about 59 MiB for a
/// million rows, roughly 30% of spike criterion K3's 200 MB budget, spent on
/// nothing (ADR-0003 A19).
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
        let view = unsafe {
            with_batch(batch, |batch| {
                (batch.view_of(column), batch.rows().column_count())
            })
        };
        match view {
            None => set_last_argument_error("reldex_batch_column: `batch` is null"),
            Some((None, count)) => column_not_found("reldex_batch_column", column, count),
            Some((Some(view), _)) => {
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

/// Builds — and from then on retains — the `#[repr(C)]` element array for a
/// `NUMBER` or `TIMESTAMP` column, and points `out->fixed` at it.
///
/// # Read this before calling it
///
/// **This is the only function here that allocates**, and what it allocates it
/// keeps until the batch is released:
///
/// | kind | bytes per row |
/// | --- | --- |
/// | `RELDEX_COLUMN_KIND_NUMBER` | `sizeof(ReldexNumber)` (46) |
/// | `RELDEX_COLUMN_KIND_TIMESTAMP` | `sizeof(ReldexTimestamp)` (16) |
///
/// For a million-row result held in memory that is 46 MB for one `NUMBER`
/// column, on top of the rows themselves. Built once per column per batch and
/// cached, so calling it repeatedly costs nothing more — but there is no way
/// to give the memory back short of releasing the batch.
///
/// **Most callers want the bulk formatter instead.**
/// [`crate::reldex_batch_format_column`] renders a window from the *source*
/// column and builds no mirror at all, which is why the Qt adapter never calls
/// this. Reach for it when you genuinely need the raw exact decimal or the
/// raw date parts — an export, a chart, a comparison — and preferably for the
/// visible window rather than for every batch you are holding.
///
/// For every other kind this is exactly [`reldex_batch_column`]: the
/// C-compatible fixed kinds (`BOOLEAN`, `FLOAT`, `DOUBLE`) are already
/// zero-copy there and nothing is built, and a text or bytes column comes back
/// with `fixed == NULL` as usual.
///
/// # Safety
///
/// `batch` must be a live batch and `out` a writable [`ReldexColumnView`] with
/// `struct_size` set. The returned pointers must not be used after the batch
/// is released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_batch_column_fixed(
    batch: *const ReldexBatch,
    column: usize,
    out: *mut ReldexColumnView,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `batch`.
        let view = unsafe {
            with_batch(batch, |batch| {
                (batch.view_with_mirror(column), batch.rows().column_count())
            })
        };
        match view {
            None => set_last_argument_error("reldex_batch_column_fixed: `batch` is null"),
            Some((None, count)) => column_not_found("reldex_batch_column_fixed", column, count),
            Some((Some(view), _)) => {
                // SAFETY: delegated to this function's contract for `out`.
                if unsafe { write_out_struct(out, view) } {
                    ReldexStatus::Ok
                } else {
                    set_last_argument_error(
                        "reldex_batch_column_fixed: `out` is null, unaligned, or too small",
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
