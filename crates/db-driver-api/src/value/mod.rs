//! The vendor-neutral value model (ADR-0002 D5).
//!
//! Two shapes, for two directions:
//!
//! - [`Value`] is owned and travels *into* the driver as a bind and *out of* it
//!   as an OUT-bind result.
//! - [`ValueRef`] borrows from a fetched [`crate::RowBatch`], so reading a cell
//!   copies nothing.
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

/// An owned database value.
///
/// Used for bind parameters and for values a driver returns through OUT binds.
/// [`Value::Lob`] and [`Value::Cursor`] are produced by drivers only: a driver
/// must reject them as IN binds with [`ErrorKind::Unsupported`].
///
/// `Value` is [`Send`] but deliberately not `Clone` — a LOB locator and a nested
/// cursor are live driver resources, and duplicating them silently would be a
/// lie.
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
}

impl Value {
    /// Whether this is SQL NULL.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
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
        }
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<Number> for Value {
    fn from(value: Number) -> Self {
        Self::Number(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Number(Number::from(value))
    }
}

impl From<i32> for Value {
    fn from(value: i32) -> Self {
        Self::Number(Number::from(value))
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Double(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

impl From<Timestamp> for Value {
    fn from(value: Timestamp) -> Self {
        Self::Timestamp(value)
    }
}

impl<T: Into<Self>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// A borrowed view of one cell in a fetched batch.
///
/// Reading a cell copies nothing: text and bytes borrow the column's contiguous
/// buffer, and a LOB is still just a handle.
#[derive(Debug)]
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
}

impl<'a> ValueRef<'a> {
    /// Whether this is SQL NULL.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
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
    fn conversion_errors_map_to_data_conversion() {
        let error = DbError::from(NumberError::TooManyDigits);
        assert_eq!(error.kind(), ErrorKind::DataConversion);
        assert!(error.message().contains("38"));

        let error = DbError::from(TemporalError::MonthOutOfRange);
        assert_eq!(error.kind(), ErrorKind::DataConversion);
    }

    #[test]
    fn driver_owned_values_are_flagged() {
        assert!(!Value::Null.is_driver_owned());
        assert!(!Value::from(1_i64).is_driver_owned());
    }
}
