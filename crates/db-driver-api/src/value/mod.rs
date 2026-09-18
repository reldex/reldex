//! The vendor-neutral value model (ADR-0002 D5).
//!
//! Three shapes, because the three directions have genuinely different needs:
//!
//! - [`BindValue`] is owned, plain data, and travels *into* the driver as a bind
//!   parameter. It is [`Clone`], so a prepared [`crate::Statement`] can be
//!   executed more than once — re-running a query is the normal case, not an
//!   exotic one.
//! - [`Value`] is owned and comes *out* of a driver through an OUT bind. It can
//!   additionally hold live driver resources (a LOB locator, a nested cursor),
//!   which is exactly why it is not `Clone`: duplicating a live handle silently
//!   would be a lie.
//! - [`ValueRef`] borrows from a fetched [`crate::RowBatch`], so reading a cell
//!   copies nothing.
//!
//! Splitting input from output also removes a rule the contract used to state in
//! prose and could not enforce ("a driver must reject a LOB or cursor used as an
//! IN bind"): there is now no way to write one.
//!
//! NULL is an explicit variant, never a sentinel. `NUMBER` is exact
//! ([`Number`]); nothing converts through `f64` implicitly. Large objects stay
//! lazy ([`LobLocator`]).

mod lob;
mod number;
mod temporal;

pub use lob::{LobKind, LobLocator, LobStream};
pub use number::{MAX_EXPONENT, MAX_SIGNIFICANT_DIGITS, MIN_EXPONENT, Number, NumberError};
pub use temporal::{TemporalError, TimeZone, Timestamp};

use std::fmt;

use crate::error::{DbError, ErrorKind};
use crate::result::Cursor;

/// An owned, cloneable input value: what a bind parameter carries.
///
/// Plain data only. A statement holding these is [`Clone`], so `db-core` can
/// re-execute the same statement — after a reconnect, for a "run again", or once
/// per row of a form — without rebuilding every bind.
///
/// Driver-owned resources ([`LobLocator`], a nested cursor) are deliberately
/// absent: they can only be produced by a driver, so they belong in [`Value`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum BindValue {
    /// SQL NULL.
    Null,
    /// A boolean (PL/SQL `BOOLEAN`; not a column type on every server).
    Boolean(bool),
    /// An exact decimal (`NUMBER`).
    Number(Number),
    /// A 32-bit binary float (`BINARY_FLOAT`).
    Float(f32),
    /// A 64-bit binary float (`BINARY_DOUBLE`).
    Double(f64),
    /// Character data (`CHAR`, `VARCHAR2`, `NCHAR`, `NVARCHAR2`) as UTF-8.
    Text(String),
    /// Binary data (`RAW`, `LONG RAW`).
    Bytes(Vec<u8>),
    /// A date or timestamp; the declared [`crate::SqlType`] says which.
    Timestamp(Timestamp),
    /// JSON, carried as UTF-8 JSON text.
    Json(String),
}

impl BindValue {
    /// Whether this is SQL NULL.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// A short, stable name for the variant, for diagnostics.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean(_) => "boolean",
            Self::Number(_) => "number",
            Self::Float(_) => "float",
            Self::Double(_) => "double",
            Self::Text(_) => "text",
            Self::Bytes(_) => "bytes",
            Self::Timestamp(_) => "timestamp",
            Self::Json(_) => "json",
        }
    }
}

macro_rules! bind_value_from {
    ($($source:ty => |$binding:ident| $body:expr),* $(,)?) => {$(
        impl From<$source> for BindValue {
            fn from($binding: $source) -> Self {
                $body
            }
        }
    )*};
}

bind_value_from! {
    bool => |value| Self::Boolean(value),
    Number => |value| Self::Number(value),
    i64 => |value| Self::Number(Number::from(value)),
    i32 => |value| Self::Number(Number::from(value)),
    f64 => |value| Self::Double(value),
    String => |value| Self::Text(value),
    &str => |value| Self::Text(value.to_owned()),
    Vec<u8> => |value| Self::Bytes(value),
    Timestamp => |value| Self::Timestamp(value),
}

impl<T: Into<Self>> From<Option<T>> for BindValue {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// An owned database value produced by a driver.
///
/// Returned through OUT and IN OUT binds. Unlike [`BindValue`] it can hold live
/// driver resources — [`Value::Lob`] and [`Value::Cursor`] — which is why it is
/// [`Send`] but deliberately not [`Clone`]: duplicating a live handle silently
/// would be a lie. Those two variants carry the thread-affinity and lifecycle
/// rules of [`LobStream`] and [`crate::Cursor`] respectively.
///
/// Because those handles need ownership to be useful, they are moved out of an
/// [`crate::OutValues`] with [`crate::OutValues::take_named`] /
/// [`crate::OutValues::take_positional`], which leave [`Value::Taken`] behind —
/// the same distinction [`crate::Column::take_lob`] draws between a consumed
/// cell and SQL NULL.
#[non_exhaustive]
pub enum Value {
    /// SQL NULL.
    Null,
    /// A boolean (PL/SQL `BOOLEAN`; not a column type on every server).
    Boolean(bool),
    /// An exact decimal (`NUMBER`).
    Number(Number),
    /// A 32-bit binary float (`BINARY_FLOAT`).
    Float(f32),
    /// A 64-bit binary float (`BINARY_DOUBLE`).
    Double(f64),
    /// Character data (`CHAR`, `VARCHAR2`, `NCHAR`, `NVARCHAR2`) as UTF-8.
    Text(String),
    /// Binary data (`RAW`, `LONG RAW`).
    Bytes(Vec<u8>),
    /// A date or timestamp; the declared [`crate::SqlType`] says which.
    Timestamp(Timestamp),
    /// JSON, carried as UTF-8 JSON text.
    Json(String),
    /// A large object, not yet read.
    Lob(LobLocator),
    /// A nested cursor (`REF CURSOR`), returned through an OUT bind or an
    /// implicit result.
    Cursor(Box<dyn Cursor>),
    /// The value used to be here and was moved out with
    /// [`crate::OutValues::take_named`] or
    /// [`crate::OutValues::take_positional`].
    ///
    /// Distinct from [`Value::Null`] on purpose, and for the same reason
    /// [`crate::ValueRef::Taken`] is: the bind *did* carry a value, so code that
    /// read "taken" as "NULL" would report the wrong thing to the user. It also
    /// makes taking idempotent — a live cursor or LOB locator can be owned only
    /// once.
    Taken,
}

impl Value {
    /// Whether this is SQL NULL.
    ///
    /// False for [`Value::Taken`]: a consumed value was not a NULL.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Whether the value has already been moved out. See [`Value::Taken`].
    #[must_use]
    pub const fn is_taken(&self) -> bool {
        matches!(self, Self::Taken)
    }

    /// Whether this value may only be produced by a driver, never bound as input.
    #[must_use]
    pub const fn is_driver_owned(&self) -> bool {
        matches!(self, Self::Lob(_) | Self::Cursor(_))
    }

    /// A short, stable name for the variant, for diagnostics.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean(_) => "boolean",
            Self::Number(_) => "number",
            Self::Float(_) => "float",
            Self::Double(_) => "double",
            Self::Text(_) => "text",
            Self::Bytes(_) => "bytes",
            Self::Timestamp(_) => "timestamp",
            Self::Json(_) => "json",
            Self::Lob(_) => "lob",
            Self::Cursor(_) => "cursor",
            Self::Taken => "taken",
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("Null"),
            Self::Boolean(value) => write!(f, "Boolean({value})"),
            Self::Number(value) => write!(f, "Number({value})"),
            Self::Float(value) => write!(f, "Float({value})"),
            Self::Double(value) => write!(f, "Double({value})"),
            Self::Text(value) => write!(f, "Text({value:?})"),
            Self::Bytes(value) => write!(f, "Bytes({} bytes)", value.len()),
            Self::Timestamp(value) => write!(f, "Timestamp({value})"),
            Self::Json(value) => write!(f, "Json({value:?})"),
            Self::Lob(value) => write!(f, "Lob({value:?})"),
            Self::Cursor(_) => f.write_str("Cursor(..)"),
            Self::Taken => f.write_str("Taken"),
        }
    }
}

impl From<BindValue> for Value {
    fn from(value: BindValue) -> Self {
        match value {
            BindValue::Null => Self::Null,
            BindValue::Boolean(inner) => Self::Boolean(inner),
            BindValue::Number(inner) => Self::Number(inner),
            BindValue::Float(inner) => Self::Float(inner),
            BindValue::Double(inner) => Self::Double(inner),
            BindValue::Text(inner) => Self::Text(inner),
            BindValue::Bytes(inner) => Self::Bytes(inner),
            BindValue::Timestamp(inner) => Self::Timestamp(inner),
            BindValue::Json(inner) => Self::Json(inner),
        }
    }
}

macro_rules! value_from {
    ($($source:ty),* $(,)?) => {$(
        impl From<$source> for Value {
            fn from(value: $source) -> Self {
                Self::from(BindValue::from(value))
            }
        }
    )*};
}

value_from!(
    bool,
    Number,
    i64,
    i32,
    f64,
    String,
    &str,
    Vec<u8>,
    Timestamp
);

impl<T: Into<BindValue>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        Self::from(BindValue::from(value))
    }
}

/// A borrowed view of one cell in a fetched batch.
///
/// Reading a cell copies nothing: text and bytes borrow the column's contiguous
/// buffer, and a LOB is still just a handle.
///
/// Three variants describe *absence*, and they mean different things:
/// [`ValueRef::Null`] is SQL NULL, [`ValueRef::Taken`] is a value that was moved
/// out of the batch, and [`ValueRef::Unsupported`] is a value the contract has no
/// type for. Collapsing any of them into NULL would make the UI show a lie.
#[derive(Debug)]
#[non_exhaustive]
pub enum ValueRef<'a> {
    /// SQL NULL.
    Null,
    /// A boolean.
    Boolean(bool),
    /// An exact decimal.
    Number(&'a Number),
    /// A 32-bit binary float.
    Float(f32),
    /// A 64-bit binary float.
    Double(f64),
    /// Character data as UTF-8.
    Text(&'a str),
    /// Binary data.
    Bytes(&'a [u8]),
    /// A date or timestamp.
    Timestamp(Timestamp),
    /// JSON text.
    Json(&'a str),
    /// A large object that has not been read. Take it from the column to read it.
    Lob(&'a LobLocator),
    /// A value of a type this contract cannot represent, rendered by the driver
    /// as best-effort text.
    ///
    /// The column's [`crate::SqlType`] is [`crate::SqlType::Unsupported`] and
    /// [`crate::ColumnMetadata::native_type_name`] holds the server's own type
    /// name. The text is for display and export only: it must not be parsed back
    /// into a typed value, and Reldex must not offer to edit such a cell.
    Unsupported(&'a str),
    /// The value used to be here and was moved out of the batch — today only via
    /// [`crate::Column::take_lob`].
    ///
    /// Distinct from [`ValueRef::Null`] on purpose: the row was *not* NULL in the
    /// database, and code that treats "taken" as "NULL" would export the wrong
    /// data.
    Taken,
}

impl<'a> ValueRef<'a> {
    /// Whether this is SQL NULL.
    ///
    /// False for [`ValueRef::Taken`] and [`ValueRef::Unsupported`]: neither is a
    /// NULL in the database.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Whether the value was moved out of the batch. See [`ValueRef::Taken`].
    #[must_use]
    pub const fn is_taken(&self) -> bool {
        matches!(self, Self::Taken)
    }

    /// The driver's best-effort text for a value of an unrepresentable type.
    ///
    /// Deliberately separate from [`ValueRef::as_str`]: this text is not
    /// character data, and must never be mistaken for it.
    #[must_use]
    pub const fn as_unsupported_text(&self) -> Option<&'a str> {
        match self {
            Self::Unsupported(value) => Some(*value),
            _ => None,
        }
    }

    /// The boolean, if this cell holds one.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    /// The exact decimal, if this cell holds one.
    #[must_use]
    pub const fn as_number(&self) -> Option<&'a Number> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    /// The text or JSON text, if this cell holds either.
    #[must_use]
    pub const fn as_str(&self) -> Option<&'a str> {
        match self {
            Self::Text(value) | Self::Json(value) => Some(*value),
            _ => None,
        }
    }

    /// The raw bytes, if this cell holds them.
    #[must_use]
    pub const fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            Self::Bytes(value) => Some(*value),
            _ => None,
        }
    }

    /// The timestamp, if this cell holds one.
    #[must_use]
    pub const fn as_timestamp(&self) -> Option<Timestamp> {
        match self {
            Self::Timestamp(value) => Some(*value),
            _ => None,
        }
    }

    /// The binary float value, widened, if this cell holds one.
    #[must_use]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Double(value) => Some(*value),
            Self::Float(value) => Some(*value as f64),
            _ => None,
        }
    }

    /// The large-object handle, if this cell holds one.
    #[must_use]
    pub const fn as_lob(&self) -> Option<&'a LobLocator> {
        match self {
            Self::Lob(value) => Some(*value),
            _ => None,
        }
    }
}

impl From<NumberError> for DbError {
    fn from(error: NumberError) -> Self {
        Self::new(ErrorKind::DataConversion, error.to_string())
    }
}

impl From<TemporalError> for DbError {
    fn from(error: TemporalError) -> Self {
        Self::new(ErrorKind::DataConversion, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_conversions() {
        assert!(Value::Null.is_null());
        assert!(!Value::from(1_i64).is_null());
        assert!(Value::from(None::<i64>).is_null());
        assert_eq!(Value::from(Some(7_i64)).type_name(), "number");
        assert_eq!(Value::from("x").type_name(), "text");
        assert_eq!(Value::from(1.5_f64).type_name(), "double");
        assert_eq!(Value::from(true).type_name(), "boolean");
        assert_eq!(Value::from(vec![1_u8, 2]).type_name(), "bytes");
    }

    #[test]
    fn bind_values_are_cloneable_so_binds_can_be_reused() {
        // `Value` cannot be `Clone` (it may own a live LOB or cursor), which used
        // to make a bound statement single-use. Input values are a separate,
        // plain-data type for exactly this reason.
        let original = BindValue::from("ข้อมูล");
        let copy = original.clone();
        assert_eq!(original, copy);
        assert_eq!(copy.type_name(), "text");

        assert!(BindValue::Null.is_null());
        assert!(BindValue::from(None::<i64>).is_null());
        assert_eq!(BindValue::from(Some(7_i64)).type_name(), "number");
        assert_eq!(BindValue::from(1.5_f64), BindValue::Double(1.5));
        assert_eq!(BindValue::from(true), BindValue::Boolean(true));
        assert_eq!(BindValue::from(vec![1_u8, 2]).type_name(), "bytes");
        assert_eq!(
            BindValue::from(Timestamp::date(2026, 9, 19).expect("valid")).type_name(),
            "timestamp"
        );
    }

    #[test]
    fn a_bind_value_widens_into_an_output_value() {
        let value = Value::from(BindValue::Json("{}".to_owned()));
        assert_eq!(value.type_name(), "json");
        assert!(!value.is_driver_owned());
    }

    #[test]
    fn debug_of_bytes_shows_length_not_content() {
        let rendered = format!("{:?}", Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(rendered, "Bytes(4 bytes)");
    }

    #[test]
    fn value_ref_accessors_are_type_checked() {
        let number = Number::from(42_i64);
        let cell = ValueRef::Number(&number);
        assert_eq!(cell.as_number().map(|n| n.to_i64()), Some(Some(42)));
        assert!(cell.as_str().is_none());
        assert!(!cell.is_null());

        assert_eq!(ValueRef::Text("abc").as_str(), Some("abc"));
        assert_eq!(ValueRef::Json("{}").as_str(), Some("{}"));
        assert_eq!(ValueRef::Bytes(&[1, 2]).as_bytes(), Some(&[1_u8, 2][..]));
        assert_eq!(ValueRef::Double(1.5).as_f64(), Some(1.5));
        assert_eq!(ValueRef::Float(0.5).as_f64(), Some(0.5));
        assert!(ValueRef::Null.is_null());
        assert!(ValueRef::Null.as_bool().is_none());
    }

    #[test]
    fn absence_variants_do_not_impersonate_sql_null() {
        let unsupported = ValueRef::Unsupported("+000000002 03:04:05.000000");
        assert!(!unsupported.is_null(), "an INTERVAL value is not NULL");
        assert!(!unsupported.is_taken());
        assert_eq!(
            unsupported.as_unsupported_text(),
            Some("+000000002 03:04:05.000000")
        );
        // The rendering is not character data and must not be read as such.
        assert!(unsupported.as_str().is_none());

        let taken = ValueRef::Taken;
        assert!(
            !taken.is_null(),
            "a consumed LOB was not a NULL in the database"
        );
        assert!(taken.is_taken());
        assert!(taken.as_unsupported_text().is_none());

        assert!(!ValueRef::Null.is_taken());
        assert!(ValueRef::Text("x").as_unsupported_text().is_none());
    }

    #[test]
    fn conversion_errors_map_to_data_conversion() {
        let error = DbError::from(NumberError::TooManyDigits);
        assert_eq!(error.kind(), ErrorKind::DataConversion);
        assert!(error.message().contains("40"));

        let error = DbError::from(TemporalError::MonthOutOfRange);
        assert_eq!(error.kind(), ErrorKind::DataConversion);
    }

    #[test]
    fn driver_owned_values_are_flagged() {
        assert!(!Value::Null.is_driver_owned());
        assert!(!Value::from(1_i64).is_driver_owned());
    }

    #[test]
    fn value_sizes_stay_where_the_adr_says_they_are() {
        // ADR-0002 D5 quotes these; a change here is a change to that claim and
        // to the fetch-path memory argument, so it should be deliberate.
        assert_eq!(size_of::<Number>(), 44, "Number");
        assert_eq!(size_of::<Value>(), 48, "Value");
        assert_eq!(size_of::<BindValue>(), 48, "BindValue");
        assert_eq!(size_of::<ValueRef<'_>>(), 24, "ValueRef");
        // Widening `Number` from 38 to 40 digits cost two bytes and did not
        // change `Value` at all: it was already padded to 48.
        assert_eq!(size_of::<Value>(), size_of::<BindValue>());
    }
}
