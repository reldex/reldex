//! The bulk formatter and its arena (ADR-0003 D4).
//!
//! **Mechanism here, policy from the caller.** C++ must not re-implement
//! 40-digit decimals or the historical mixed calendar, and Rust must not
//! decide what a decimal separator is. So the rendering lives here and every
//! choice that is a user setting arrives in [`ReldexFormatOptions`].
//!
//! One call formats a *window* — a column, a row range — into a
//! [`ReldexTextArena`]: one buffer plus offsets, the same shape a text column
//! has, so the adapter reads the results with pointer arithmetic. One call per
//! visible window, not one per cell.

use reldex_db_driver_api::ValueRef;

use crate::batch::ReldexBatch;
use crate::error::set_last_argument_error;
use crate::status::{ReldexStatus, entry, entry_value};
use crate::strings::{CStruct, ReldexStr, read_in_struct, write_out_struct};

/// How a timestamp is rendered.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexTimestampStyle {
    /// A style this header does not know; treated as `ISO_8601`.
    Unknown = 0,
    /// `2026-01-02T13:45:30.123456789+07:00`, with the fractional part and the
    /// zone present only when the value has them.
    Iso8601 = 1,
    /// The same, with a space instead of the `T`.
    Iso8601Space = 2,
    /// `2026-01-02`: the date only.
    DateOnly = 3,
}

/// How raw bytes are rendered.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexBytesStyle {
    /// A style this header does not know; treated as `HEX_UPPER`.
    Unknown = 0,
    /// `0A1B2C`.
    HexUpper = 1,
    /// `0a1b2c`.
    HexLower = 2,
}

/// The user's formatting settings, passed in per call.
///
/// Every field is optional in the ADR-0003 D7 sense: zero means the documented
/// default, so `NULL` options (or a zeroed struct with only `struct_size` set)
/// give sensible output.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexFormatOptions {
    /// `sizeof(ReldexFormatOptions)`.
    pub struct_size: u32,
    /// The decimal separator, as a Unicode scalar. `0` means `.`.
    pub decimal_separator: u32,
    /// The digit-group separator, as a Unicode scalar. `0` means no grouping.
    pub grouping_separator: u32,
    /// How many digits per group. `0` means 3 when grouping is on.
    pub grouping_size: u32,
    /// A [`ReldexTimestampStyle`].
    pub timestamp_style: i32,
    /// A [`ReldexBytesStyle`].
    pub bytes_style: i32,
    /// How many fraction digits to show. Negative means "as stored"; `0`
    /// means "as stored" too, since a caller that zeroes the struct is asking
    /// for defaults, not for integers.
    pub max_fraction_digits: i32,
    /// How many bytes of a `RAW` column to render before eliding the rest.
    /// `0` means 32.
    pub max_bytes_rendered: u32,
    /// What to write for SQL NULL. Empty means the empty string — which is why
    /// a grid must also *visualize* NULL rather than relying on the text
    /// (`SPEC.md` §24: NULL, taken and empty are three different states).
    pub null_text: ReldexStr,
    /// What to write for a cell whose large object was taken out of the batch.
    /// Empty means `[LOB]`.
    pub taken_text: ReldexStr,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, integers and `ReldexStr`s — valid
// when zeroed (a zeroed `ReldexStr` is null+0, which the reader treats as
// empty).
unsafe impl CStruct for ReldexFormatOptions {
    const MIN_SIZE: usize = size_of::<u32>() + size_of::<u32>();
}

impl Default for ReldexFormatOptions {
    /// `.` as the decimal separator, no grouping, ISO-8601 timestamps,
    /// uppercase hex, values as stored.
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            decimal_separator: 0,
            grouping_separator: 0,
            grouping_size: 0,
            timestamp_style: ReldexTimestampStyle::Iso8601 as i32,
            bytes_style: ReldexBytesStyle::HexUpper as i32,
            max_fraction_digits: -1,
            max_bytes_rendered: 0,
            null_text: ReldexStr::empty(),
            taken_text: ReldexStr::empty(),
        }
    }
}

/// The resolved settings, with every default already applied.
struct Resolved<'a> {
    decimal: char,
    grouping: Option<char>,
    grouping_size: usize,
    timestamp_style: i32,
    bytes_upper: bool,
    max_fraction_digits: Option<usize>,
    max_bytes: usize,
    null_text: &'a str,
    taken_text: &'a str,
}

impl ReldexFormatOptions {
    /// Applies the documented defaults.
    ///
    /// # Safety
    ///
    /// The two `ReldexStr` fields must point at readable bytes for the
    /// returned borrow's life.
    unsafe fn resolve<'a>(&self) -> Resolved<'a> {
        Resolved {
            // `char::from_u32(0)` is `Some(NUL)`, not `None`: zero means
            // "not set" here, so it has to be filtered out explicitly or a
            // zeroed options struct would render digits separated by NULs.
            decimal: char::from_u32(self.decimal_separator)
                .filter(|scalar| *scalar != '\0')
                .unwrap_or('.'),
            grouping: char::from_u32(self.grouping_separator).filter(|scalar| *scalar != '\0'),
            grouping_size: if self.grouping_size == 0 {
                3
            } else {
                self.grouping_size as usize
            },
            timestamp_style: self.timestamp_style,
            bytes_upper: self.bytes_style != ReldexBytesStyle::HexLower as i32,
            max_fraction_digits: usize::try_from(self.max_fraction_digits)
                .ok()
                .filter(|digits| *digits > 0),
            max_bytes: if self.max_bytes_rendered == 0 {
                32
            } else {
                self.max_bytes_rendered as usize
            },
            // SAFETY: delegated to this function's contract.
            null_text: unsafe { self.null_text.as_str() }.unwrap_or(""),
            // SAFETY: as above.
            taken_text: unsafe { self.taken_text.as_str() }.unwrap_or("[LOB]"),
        }
    }
}

/// Somewhere for formatted text to land: one buffer plus offsets, reusable.
///
/// Opaque. Create one per grid (not per call), [`reldex_text_arena_clear`] it
/// before each window, and release it with [`reldex_text_arena_release`].
pub struct ReldexTextArena {
    buffer: String,
    offsets: Vec<usize>,
}

impl ReldexTextArena {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            offsets: vec![0],
        }
    }

    /// Appends one string.
    pub(crate) fn push(&mut self, text: &str) {
        self.buffer.push_str(text);
        self.offsets.push(self.buffer.len());
    }

    fn clear(&mut self) {
        self.buffer.clear();
        self.offsets.clear();
        self.offsets.push(0);
    }

    fn count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    fn view(&self) -> ReldexArenaView {
        ReldexArenaView {
            data: self.buffer.as_ptr(),
            data_len: self.buffer.len(),
            offsets: self.offsets.as_ptr(),
            count: self.count(),
            ..ReldexArenaView::default()
        }
    }
}

/// What an arena holds: the same buffer-plus-offsets shape a text column has.
///
/// Both pointers borrow from the arena and are invalidated by the next
/// [`reldex_batch_format_column`], [`reldex_text_arena_clear`] or
/// [`reldex_text_arena_release`] on it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexArenaView {
    /// `sizeof(ReldexArenaView)` on the way in; how much is valid on the way
    /// out.
    pub struct_size: u32,
    /// The one contiguous UTF-8 buffer.
    pub data: *const u8,
    /// How many bytes `data` covers.
    pub data_len: usize,
    /// `count + 1` byte offsets: string `i` is `data[offsets[i]..offsets[i+1]]`.
    pub offsets: *const usize,
    /// How many strings the arena holds.
    pub count: usize,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, integers and raw pointers — valid
// when zeroed.
unsafe impl CStruct for ReldexArenaView {
    const MIN_SIZE: usize = size_of::<Self>();
}

impl Default for ReldexArenaView {
    /// An empty view with `struct_size` set.
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            data: std::ptr::null(),
            data_len: 0,
            offsets: std::ptr::null(),
            count: 0,
        }
    }
}

/// Creates an arena. Returns `NULL` only if allocation failed.
#[unsafe(no_mangle)]
pub extern "C" fn reldex_text_arena_create() -> *mut ReldexTextArena {
    entry_value(std::ptr::null_mut(), || {
        Box::into_raw(Box::new(ReldexTextArena::new()))
    })
}

/// Releases an arena and every pointer taken from it. A null pointer is a
/// no-op.
///
/// # Safety
///
/// `arena` must be null, or a pointer [`reldex_text_arena_create`] returned
/// that has not been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_text_arena_release(arena: *mut ReldexTextArena) {
    entry_value((), || {
        if arena.is_null() {
            return;
        }
        // SAFETY: the caller promises this came from `Box::into_raw` in this
        // library and has not been released.
        drop(unsafe { Box::from_raw(arena) });
    });
}

/// Empties an arena, keeping its allocation for the next window.
///
/// # Safety
///
/// `arena` must be a live arena.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_text_arena_clear(arena: *mut ReldexTextArena) {
    entry_value((), || {
        if arena.is_null() || !arena.is_aligned() {
            return;
        }
        // SAFETY: the caller promises `arena` is live, and D5 rule 3 keeps the
        // caller single-threaded, so nothing else holds a borrow.
        unsafe { &mut *arena }.clear();
    });
}

/// How many strings an arena holds.
///
/// # Safety
///
/// `arena` must be null (reported as 0) or a live arena.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_text_arena_count(arena: *const ReldexTextArena) -> usize {
    entry_value(0, || {
        if arena.is_null() || !arena.is_aligned() {
            return 0;
        }
        // SAFETY: the caller promises `arena` is live.
        unsafe { &*arena }.count()
    })
}

/// Describes an arena's buffer and offsets.
///
/// # Safety
///
/// `arena` must be a live arena and `out` a writable [`ReldexArenaView`] with
/// `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_text_arena_view(
    arena: *const ReldexTextArena,
    out: *mut ReldexArenaView,
) -> ReldexStatus {
    entry(|| {
        if arena.is_null() || !arena.is_aligned() {
            return set_last_argument_error("reldex_text_arena_view: `arena` is null or unaligned");
        }
        // SAFETY: the caller promises `arena` is live.
        let view = unsafe { &*arena }.view();
        // SAFETY: delegated to this function's contract for `out`.
        if unsafe { write_out_struct(out, view) } {
            ReldexStatus::Ok
        } else {
            set_last_argument_error(
                "reldex_text_arena_view: `out` is null, unaligned, or too small",
            )
        }
    })
}

/// Formats `row_count` rows of one column into `arena`, appending one string
/// per row.
///
/// This is the *bulk* formatter ADR-0003 D4 asks for: one call per visible
/// window, never one per cell. A row past the end of the batch is skipped, so
/// the arena holds as many strings as rows actually existed — read
/// [`reldex_text_arena_count`] rather than assuming `row_count`.
///
/// NULL renders as `options->null_text` and a taken large object as
/// `options->taken_text`; a grid must still *visualize* the difference rather
/// than rely on the text.
///
/// # Safety
///
/// `batch` must be a live batch, `arena` a live arena, and `options` null (for
/// the defaults) or a [`ReldexFormatOptions`] with `struct_size` set and any
/// strings in it readable for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_batch_format_column(
    batch: *const ReldexBatch,
    column: usize,
    row_start: usize,
    row_count: usize,
    options: *const ReldexFormatOptions,
    arena: *mut ReldexTextArena,
) -> ReldexStatus {
    entry(|| {
        if batch.is_null() || !batch.is_aligned() {
            return set_last_argument_error(
                "reldex_batch_format_column: `batch` is null or unaligned",
            );
        }
        if arena.is_null() || !arena.is_aligned() {
            return set_last_argument_error(
                "reldex_batch_format_column: `arena` is null or unaligned",
            );
        }
        let options = if options.is_null() {
            ReldexFormatOptions::default()
        } else {
            // SAFETY: delegated to this function's contract for `options`.
            match unsafe { read_in_struct(options) } {
                Some(options) => options,
                None => {
                    return set_last_argument_error(
                        "reldex_batch_format_column: `options` is unaligned or its struct_size \
                         is too small",
                    );
                }
            }
        };
        // SAFETY: the strings in `options` are promised readable for this call,
        // and `resolved` does not outlive it.
        let resolved = unsafe { options.resolve() };
        // SAFETY: the caller promises `batch` and `arena` are live and, per D5
        // rule 3, that no other thread is using them.
        let (batch, arena) = unsafe { (&*batch, &mut *arena) };
        if column >= batch.rows().column_count() {
            return ReldexStatus::NotFound;
        }
        let kind = batch
            .rows()
            .column(column)
            .map(reldex_db_driver_api::Column::kind);
        let rows = batch.rows().row_count();
        for row in row_start..row_start.saturating_add(row_count).min(rows) {
            let text = match batch.rows().value(row, column) {
                None => resolved.null_text.to_owned(),
                Some(value) => format_value(&value, kind, &resolved),
            };
            arena.push(&text);
        }
        ReldexStatus::Ok
    })
}

fn format_value(
    value: &ValueRef<'_>,
    kind: Option<reldex_db_driver_api::ColumnKind>,
    options: &Resolved<'_>,
) -> String {
    match value {
        ValueRef::Null => options.null_text.to_owned(),
        ValueRef::Taken => options.taken_text.to_owned(),
        ValueRef::Boolean(value) => (if *value { "true" } else { "false" }).to_owned(),
        ValueRef::Number(number) => format_number(&number.to_string(), options),
        ValueRef::Float(value) => format_number(&value.to_string(), options),
        ValueRef::Double(value) => format_number(&value.to_string(), options),
        ValueRef::Text(text) | ValueRef::Json(text) | ValueRef::Unsupported(text) => {
            (*text).to_owned()
        }
        ValueRef::Bytes(bytes) => format_bytes(bytes, options),
        ValueRef::Timestamp(timestamp) => format_timestamp(&timestamp.to_string(), options),
        // `ValueRef` is `#[non_exhaustive]`, and a LOB cell that still holds a
        // locator cannot reach here (`db-core` parks them). Say what it is
        // rather than inventing a value.
        _ => match kind {
            Some(reldex_db_driver_api::ColumnKind::Lob) => options.taken_text.to_owned(),
            _ => String::new(),
        },
    }
}

/// Applies the caller's separators and fraction limit to a canonical decimal
/// rendering (`-1234.5`, never scientific — `Number`'s `Display` normalizes
/// that away).
fn format_number(canonical: &str, options: &Resolved<'_>) -> String {
    let (sign, rest) = match canonical.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", canonical),
    };
    let (integer, fraction) = match rest.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (rest, ""),
    };
    // A value the canonical form rendered in some other shape (an infinity, a
    // NaN from a `BINARY_DOUBLE`) is passed through untouched rather than
    // mangled by digit grouping.
    if !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return canonical.to_owned();
    }
    let fraction = match options.max_fraction_digits {
        Some(limit) if fraction.len() > limit => round_fraction(integer, fraction, limit),
        _ => RoundedFraction {
            integer: integer.to_owned(),
            fraction: fraction.to_owned(),
        },
    };
    let mut out = String::with_capacity(canonical.len() + 8);
    out.push_str(sign);
    match options.grouping {
        None => out.push_str(&fraction.integer),
        Some(separator) => {
            let digits = fraction.integer.as_bytes();
            for (index, digit) in digits.iter().enumerate() {
                let remaining = digits.len() - index;
                if index > 0 && options.grouping_size > 0 && remaining % options.grouping_size == 0
                {
                    out.push(separator);
                }
                out.push(char::from(*digit));
            }
        }
    }
    if !fraction.fraction.is_empty() {
        out.push(options.decimal);
        out.push_str(&fraction.fraction);
    }
    out
}

struct RoundedFraction {
    integer: String,
    fraction: String,
}

/// Rounds a decimal digit string half-away-from-zero at `limit` fraction
/// digits.
///
/// Truncating would be the cheaper answer and a quietly wrong one: a grid that
/// shows `0.1` for `0.19` is misreporting the database.
fn round_fraction(integer: &str, fraction: &str, limit: usize) -> RoundedFraction {
    let mut digits: Vec<u8> = integer
        .bytes()
        .chain(fraction.bytes().take(limit))
        .map(|byte| byte - b'0')
        .collect();
    let round_up = fraction
        .as_bytes()
        .get(limit)
        .is_some_and(|byte| *byte >= b'5');
    if round_up {
        let mut index = digits.len();
        loop {
            if index == 0 {
                digits.insert(0, 1);
                return split_digits(&digits, integer.len() + 1, limit);
            }
            index -= 1;
            if digits[index] == 9 {
                digits[index] = 0;
            } else {
                digits[index] += 1;
                break;
            }
        }
    }
    split_digits(&digits, integer.len(), limit)
}

fn split_digits(digits: &[u8], integer_len: usize, limit: usize) -> RoundedFraction {
    let render =
        |slice: &[u8]| -> String { slice.iter().map(|digit| char::from(b'0' + digit)).collect() };
    let integer_len = integer_len.min(digits.len());
    let integer = render(&digits[..integer_len]);
    let fraction = render(&digits[integer_len..(integer_len + limit).min(digits.len())]);
    RoundedFraction {
        integer: if integer.is_empty() {
            "0".to_owned()
        } else {
            integer
        },
        fraction: fraction.trim_end_matches('0').to_owned(),
    }
}

fn format_bytes(bytes: &[u8], options: &Resolved<'_>) -> String {
    let shown = bytes.len().min(options.max_bytes);
    let mut out = String::with_capacity(shown * 2 + 8);
    for byte in &bytes[..shown] {
        if options.bytes_upper {
            out.push_str(&format!("{byte:02X}"));
        } else {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    if shown < bytes.len() {
        out.push('…');
    }
    out
}

/// Reshapes the contract's canonical ISO-8601 rendering.
///
/// The canonical form is what `Timestamp`'s `Display` produces, which already
/// handles the historical mixed calendar and the zone; this only applies the
/// caller's *style*, so no date arithmetic happens in C++ or here.
fn format_timestamp(canonical: &str, options: &Resolved<'_>) -> String {
    if options.timestamp_style == ReldexTimestampStyle::DateOnly as i32 {
        return canonical
            .split_once('T')
            .map_or_else(|| canonical.to_owned(), |(date, _)| date.to_owned());
    }
    if options.timestamp_style == ReldexTimestampStyle::Iso8601Space as i32 {
        return canonical.replacen('T', " ", 1);
    }
    canonical.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        ReldexBytesStyle, ReldexFormatOptions, ReldexTimestampStyle, Resolved, format_bytes,
        format_number, format_timestamp,
    };

    fn options() -> Resolved<'static> {
        Resolved {
            decimal: '.',
            grouping: None,
            grouping_size: 3,
            timestamp_style: ReldexTimestampStyle::Iso8601 as i32,
            bytes_upper: true,
            max_fraction_digits: None,
            max_bytes: 32,
            null_text: "",
            taken_text: "[LOB]",
        }
    }

    #[test]
    fn grouping_and_separators_come_from_the_caller() {
        let mut settings = options();
        settings.grouping = Some(',');
        assert_eq!(format_number("1234567.25", &settings), "1,234,567.25");
        settings.decimal = ',';
        settings.grouping = Some('.');
        assert_eq!(format_number("1234567.25", &settings), "1.234.567,25");
        settings.grouping = None;
        assert_eq!(format_number("-0.5", &settings), "-0,5");
        assert_eq!(format_number("1500", &settings), "1500");
    }

    #[test]
    fn a_fraction_limit_rounds_rather_than_truncating() {
        // Truncation would show 0.1 for 0.19 — a grid quietly misreporting the
        // database, which `SPEC.md` §2 ranks worst.
        let mut settings = options();
        settings.max_fraction_digits = Some(1);
        assert_eq!(format_number("0.19", &settings), "0.2");
        assert_eq!(format_number("-0.19", &settings), "-0.2");
        assert_eq!(format_number("9.99", &settings), "10");
        assert_eq!(format_number("0.14", &settings), "0.1");
        settings.max_fraction_digits = Some(2);
        assert_eq!(format_number("1.005", &settings), "1.01");
        assert_eq!(
            format_number("1.2", &settings),
            "1.2",
            "shorter is untouched"
        );
    }

    #[test]
    fn a_value_that_is_not_a_plain_decimal_is_passed_through() {
        let settings = options();
        assert_eq!(format_number("inf", &settings), "inf");
        assert_eq!(format_number("NaN", &settings), "NaN");
    }

    #[test]
    fn bytes_and_timestamps_follow_the_style() {
        let mut settings = options();
        assert_eq!(format_bytes(&[0x0a, 0xff], &settings), "0AFF");
        settings.bytes_upper = false;
        assert_eq!(format_bytes(&[0x0a, 0xff], &settings), "0aff");
        settings.max_bytes = 1;
        assert_eq!(format_bytes(&[0x0a, 0xff], &settings), "0a…");

        let canonical = "2026-01-02T13:45:30+07:00";
        assert_eq!(format_timestamp(canonical, &settings), canonical);
        settings.timestamp_style = ReldexTimestampStyle::Iso8601Space as i32;
        assert_eq!(
            format_timestamp(canonical, &settings),
            "2026-01-02 13:45:30+07:00"
        );
        settings.timestamp_style = ReldexTimestampStyle::DateOnly as i32;
        assert_eq!(format_timestamp(canonical, &settings), "2026-01-02");
    }

    #[test]
    fn the_default_options_are_the_documented_ones() {
        let defaults = ReldexFormatOptions::default();
        // SAFETY: the default's strings are the static empty string.
        let resolved = unsafe { defaults.resolve() };
        assert_eq!(resolved.decimal, '.');
        assert!(resolved.grouping.is_none());
        assert!(resolved.bytes_upper);
        assert_eq!(resolved.max_bytes, 32);
        assert!(resolved.max_fraction_digits.is_none());
        assert_eq!(defaults.bytes_style, ReldexBytesStyle::HexUpper as i32);
    }
}
