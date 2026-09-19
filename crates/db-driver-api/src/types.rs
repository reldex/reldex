//! Vendor-neutral column and bind type descriptions (ADR-0002 D5).
//!
//! [`SqlType`] names the families `SPEC.md` §8 requires, not every native type a
//! server has. Anything the contract cannot express is [`SqlType::Unsupported`],
//! with the server's own type name kept in
//! [`ColumnMetadata::native_type_name`] so diagnostics stay useful.

use std::fmt;

/// The vendor-neutral type family of a column or bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SqlType {
    /// Boolean.
    Boolean,
    /// Exact decimal (`NUMBER`), carried as [`crate::Number`].
    Number,
    /// 32-bit binary float (`BINARY_FLOAT`).
    BinaryFloat,
    /// 64-bit binary float (`BINARY_DOUBLE`).
    BinaryDouble,
    /// Character data (`CHAR`, `VARCHAR2`, `NCHAR`, `NVARCHAR2`).
    Text {
        /// National character set (`NCHAR`, `NVARCHAR2`).
        national: bool,
        /// Blank-padded fixed length (`CHAR`, `NCHAR`).
        fixed_length: bool,
    },
    /// A date with a time component and no zone (`DATE`).
    Date,
    /// A timestamp with no zone (`TIMESTAMP`).
    Timestamp,
    /// A timestamp carrying a UTC offset (`TIMESTAMP WITH TIME ZONE`).
    TimestampWithTimeZone,
    /// Binary data (`RAW`, `LONG RAW`).
    Raw,
    /// A character large object (`CLOB`, `NCLOB`).
    CharacterLob {
        /// National character set (`NCLOB`).
        national: bool,
    },
    /// A binary large object (`BLOB`).
    BinaryLob,
    /// JSON.
    Json,
    /// A nested cursor (`REF CURSOR`).
    Cursor,
    /// A native type this contract cannot represent.
    ///
    /// A driver must not silently coerce such a column into another family, and
    /// it must not fail the fetch either: `SELECT *` over a table with one
    /// `INTERVAL`, `ROWID`, `XMLType` or `VECTOR` column must still return the
    /// other columns (`SPEC.md` §2 ranks correctness above convenience, and
    /// refusing the whole result is neither).
    ///
    /// The driver reports `Unsupported` here, keeps the server's own type name
    /// in [`ColumnMetadata::native_type_name`], and delivers the cells as
    /// [`crate::ColumnData::Unsupported`] — a best-effort text rendering the UI
    /// can show read-only.
    Unsupported,
}

impl SqlType {
    /// Plain-text `Text` with no national character set.
    pub const VARCHAR: Self = Self::Text {
        national: false,
        fixed_length: false,
    };

    /// Whether values of this type are delivered as a [`crate::LobLocator`].
    #[must_use]
    pub const fn is_lob(self) -> bool {
        matches!(self, Self::CharacterLob { .. } | Self::BinaryLob)
    }

    /// Whether values of this type are delivered as UTF-8 text.
    #[must_use]
    pub const fn is_text(self) -> bool {
        matches!(self, Self::Text { .. } | Self::Json)
    }
}

impl fmt::Display for SqlType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::BinaryFloat => "binary-float",
            Self::BinaryDouble => "binary-double",
            Self::Text {
                national: false,
                fixed_length: false,
            } => "text",
            Self::Text {
                national: false,
                fixed_length: true,
            } => "text-fixed",
            Self::Text {
                national: true,
                fixed_length: false,
            } => "ntext",
            Self::Text {
                national: true,
                fixed_length: true,
            } => "ntext-fixed",
            Self::Date => "date",
            Self::Timestamp => "timestamp",
            Self::TimestampWithTimeZone => "timestamp-tz",
            Self::Raw => "raw",
            Self::CharacterLob { national: false } => "clob",
            Self::CharacterLob { national: true } => "nclob",
            Self::BinaryLob => "blob",
            Self::Json => "json",
            Self::Cursor => "cursor",
            Self::Unsupported => "unsupported",
        };
        f.write_str(text)
    }
}

/// Everything the contract says about one result column.
///
/// Built once per cursor, so the allocations for the names are negligible; the
/// per-row path never touches this type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnMetadata {
    name: Box<str>,
    sql_type: SqlType,
    nullable: Option<bool>,
    precision: Option<u8>,
    scale: Option<i8>,
    max_size_bytes: Option<u32>,
    native_type_name: Option<Box<str>>,
}

impl ColumnMetadata {
    /// Describes a column by name and vendor-neutral type.
    #[must_use]
    pub fn new(name: impl Into<Box<str>>, sql_type: SqlType) -> Self {
        Self {
            name: name.into(),
            sql_type,
            nullable: None,
            precision: None,
            scale: None,
            max_size_bytes: None,
            native_type_name: None,
        }
    }

    /// Records whether the column accepts NULL, when the driver knows.
    #[must_use]
    pub fn with_nullable(mut self, nullable: bool) -> Self {
        self.nullable = Some(nullable);
        self
    }

    /// Records declared decimal precision and scale.
    ///
    /// For [`SqlType::Timestamp`] and [`SqlType::TimestampWithTimeZone`], the
    /// `scale` slot carries the declared **fractional-seconds precision**
    /// (`TIMESTAMP(6)` is `scale == 6`) — that is how servers report it, and a
    /// separate field would be a second name for the same number. `precision`
    /// is not meaningful for those types. See [`ColumnMetadata::scale`].
    #[must_use]
    pub fn with_precision_scale(mut self, precision: u8, scale: i8) -> Self {
        self.precision = Some(precision);
        self.scale = Some(scale);
        self
    }

    /// Records the declared maximum size in bytes.
    #[must_use]
    pub fn with_max_size_bytes(mut self, max_size_bytes: u32) -> Self {
        self.max_size_bytes = Some(max_size_bytes);
        self
    }

    /// Records the server's own type name, for diagnostics and for
    /// [`SqlType::Unsupported`] columns.
    #[must_use]
    pub fn with_native_type_name(mut self, native_type_name: impl Into<Box<str>>) -> Self {
        self.native_type_name = Some(native_type_name.into());
        self
    }

    /// The column name as the server reported it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The vendor-neutral type family.
    #[must_use]
    pub const fn sql_type(&self) -> SqlType {
        self.sql_type
    }

    /// Whether the column accepts NULL, if the driver reported it.
    #[must_use]
    pub const fn nullable(&self) -> Option<bool> {
        self.nullable
    }

    /// Declared decimal precision, if any.
    #[must_use]
    pub const fn precision(&self) -> Option<u8> {
        self.precision
    }

    /// Declared decimal scale, if any.
    ///
    /// For [`SqlType::Timestamp`] and [`SqlType::TimestampWithTimeZone`] this is
    /// the declared fractional-seconds precision (`0..=9`) rather than a decimal
    /// scale. The declared [`ColumnMetadata::sql_type`] says which reading
    /// applies; no other field changes meaning by type.
    #[must_use]
    pub const fn scale(&self) -> Option<i8> {
        self.scale
    }

    /// Declared maximum size in bytes, if any.
    #[must_use]
    pub const fn max_size_bytes(&self) -> Option<u32> {
        self.max_size_bytes
    }

    /// The server's own type name, if the driver reported it.
    #[must_use]
    pub fn native_type_name(&self) -> Option<&str> {
        self.native_type_name.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_builder_records_what_the_driver_knows() {
        let column = ColumnMetadata::new("SALARY", SqlType::Number)
            .with_nullable(false)
            .with_precision_scale(38, 2)
            .with_native_type_name("NUMBER(38,2)");

        assert_eq!(column.name(), "SALARY");
        assert_eq!(column.sql_type(), SqlType::Number);
        assert_eq!(column.nullable(), Some(false));
        assert_eq!(column.precision(), Some(38));
        assert_eq!(column.scale(), Some(2));
        assert_eq!(column.native_type_name(), Some("NUMBER(38,2)"));
        assert_eq!(column.max_size_bytes(), None);
    }

    #[test]
    fn timestamp_scale_carries_fractional_seconds_precision() {
        let column = ColumnMetadata::new("CREATED_AT", SqlType::TimestampWithTimeZone)
            .with_precision_scale(0, 6)
            .with_native_type_name("TIMESTAMP(6) WITH TIME ZONE");
        assert_eq!(column.scale(), Some(6));
        assert_eq!(
            column.native_type_name(),
            Some("TIMESTAMP(6) WITH TIME ZONE")
        );
    }

    #[test]
    fn unsupported_columns_keep_the_servers_own_type_name() {
        let column = ColumnMetadata::new("SPAN", SqlType::Unsupported)
            .with_native_type_name("INTERVAL DAY(2) TO SECOND(6)");
        assert_eq!(column.sql_type(), SqlType::Unsupported);
        assert_eq!(
            column.native_type_name(),
            Some("INTERVAL DAY(2) TO SECOND(6)")
        );
    }

    #[test]
    fn unknown_facts_stay_unknown() {
        let column = ColumnMetadata::new("NOTE", SqlType::VARCHAR);
        assert_eq!(column.nullable(), None);
        assert_eq!(column.precision(), None);
        assert_eq!(column.native_type_name(), None);
    }

    #[test]
    fn type_families_are_classified() {
        assert!(SqlType::BinaryLob.is_lob());
        assert!(SqlType::CharacterLob { national: true }.is_lob());
        assert!(!SqlType::Raw.is_lob());
        assert!(SqlType::VARCHAR.is_text());
        assert!(SqlType::Json.is_text());
        assert!(!SqlType::Number.is_text());
    }

    #[test]
    fn display_names_are_distinct() {
        let types = [
            SqlType::Boolean,
            SqlType::Number,
            SqlType::BinaryFloat,
            SqlType::BinaryDouble,
            SqlType::VARCHAR,
            SqlType::Text {
                national: false,
                fixed_length: true,
            },
            SqlType::Text {
                national: true,
                fixed_length: false,
            },
            SqlType::Text {
                national: true,
                fixed_length: true,
            },
            SqlType::Date,
            SqlType::Timestamp,
            SqlType::TimestampWithTimeZone,
            SqlType::Raw,
            SqlType::CharacterLob { national: false },
            SqlType::CharacterLob { national: true },
            SqlType::BinaryLob,
            SqlType::Json,
            SqlType::Cursor,
            SqlType::Unsupported,
        ];
        let mut seen: Vec<String> = Vec::new();
        for sql_type in types {
            let text = sql_type.to_string();
            assert!(!seen.contains(&text), "duplicate display name {text}");
            seen.push(text);
        }
        assert_eq!(seen.len(), 18);
    }
}
