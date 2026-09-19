//! Column type mapping and row-to-column batch conversion (ADR-0002 D5, D6).
//!
//! `oracledb` fetches row-at-a-time into a private `DbValue` enum that is not
//! nameable from outside the crate, so every cell is read back through
//! `Row::get::<T>` / `Row::take::<T>` with a concrete type chosen from the
//! column's `DbType`. The values are appended to column-oriented builders here,
//! which cost one allocation per column rather than one per cell.
//!
//! `NUMBER` goes through `OracleNumber`'s `Display` into one reusable buffer per
//! column and then [`Number::parse`], because `OracleNumber`'s digits and
//! exponent are private. That route is lossless — `Display` writes every stored
//! digit and `Number` holds 40 of them — but it is a string round trip per
//! numeric cell, which is worth an upstream request for digit accessors.

use reldex_db_driver_api::{
    BytesColumn, Column, ColumnData, ColumnMetadata, DbError, DbResult, ErrorKind, LobKind,
    LobLocator, NullMask, Number, SqlType, TextColumn, TimeZone, Timestamp,
};

use oracledb::{
    DB_TYPE_BFILE, DB_TYPE_BINARY_DOUBLE, DB_TYPE_BINARY_FLOAT, DB_TYPE_BLOB, DB_TYPE_BOOLEAN,
    DB_TYPE_CHAR, DB_TYPE_CLOB, DB_TYPE_CURSOR, DB_TYPE_DATE, DB_TYPE_INTERVAL_DS,
    DB_TYPE_INTERVAL_YM, DB_TYPE_JSON, DB_TYPE_LONG, DB_TYPE_LONG_NVARCHAR, DB_TYPE_LONG_RAW,
    DB_TYPE_NCHAR, DB_TYPE_NCLOB, DB_TYPE_NUMBER, DB_TYPE_NVARCHAR, DB_TYPE_OBJECT, DB_TYPE_RAW,
    DB_TYPE_ROWID, DB_TYPE_TIMESTAMP, DB_TYPE_TIMESTAMP_LTZ, DB_TYPE_TIMESTAMP_TZ, DB_TYPE_UROWID,
    DB_TYPE_VARCHAR, DB_TYPE_VECTOR, DB_TYPE_XMLTYPE, DbType, Lob, Metadata, OracleIntervalDS,
    OracleIntervalYM, OracleNumber, OracleTimestamp, Row,
};

use crate::error::render_into;
use crate::lob::OracleLobStream;

use reldex_db_driver_api::ConnectionId;

/// How one result column is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnPlan {
    /// Exact decimal, via `OracleNumber`'s `Display`.
    Number,
    /// `BINARY_FLOAT`.
    Float,
    /// `BINARY_DOUBLE`.
    Double,
    /// Character data, delivered as UTF-8.
    Text,
    /// `RAW` / `LONG RAW`.
    Bytes,
    /// `DATE` / `TIMESTAMP`: civil fields, no zone.
    Timestamp,
    /// `TIMESTAMP WITH TIME ZONE`: civil fields plus a UTC offset.
    TimestampWithZone,
    /// PL/SQL `BOOLEAN` (and, from 26ai, a column type).
    Boolean,
    /// A large object, fetched as a locator and streamed.
    Lob(LobKind),
    /// A `ROWID`, rendered as text because the contract has no type for it.
    UnsupportedRowid,
    /// An `INTERVAL DAY TO SECOND`, rendered as text.
    UnsupportedIntervalDs,
    /// An `INTERVAL YEAR TO MONTH`, rendered as text.
    UnsupportedIntervalYm,
    /// A `TIMESTAMP WITH LOCAL TIME ZONE`, rendered as text.
    ///
    /// The contract deliberately has no variant for it (ADR-0002 M1): its value
    /// depends on the session time zone, so carrying it as a plain timestamp
    /// would silently change meaning between sessions.
    UnsupportedTimestampLtz,
    /// A type this driver cannot decode at all.
    ///
    /// `oracledb` refuses these during deserialization, so unlike the variants
    /// above they cannot be rendered as text: a `SELECT *` over such a column
    /// fails. See the crate documentation, "Known limitations".
    Unsupported,
}

/// The vendor-neutral type of a column, and how to decode it.
pub(crate) fn plan_for(db_type: &'static DbType) -> (SqlType, ColumnPlan) {
    if db_type == &DB_TYPE_NUMBER {
        (SqlType::Number, ColumnPlan::Number)
    } else if db_type == &DB_TYPE_BINARY_FLOAT {
        (SqlType::BinaryFloat, ColumnPlan::Float)
    } else if db_type == &DB_TYPE_BINARY_DOUBLE {
        (SqlType::BinaryDouble, ColumnPlan::Double)
    } else if db_type == &DB_TYPE_VARCHAR || db_type == &DB_TYPE_LONG {
        (text(false, false), ColumnPlan::Text)
    } else if db_type == &DB_TYPE_CHAR {
        (text(false, true), ColumnPlan::Text)
    } else if db_type == &DB_TYPE_NVARCHAR || db_type == &DB_TYPE_LONG_NVARCHAR {
        (text(true, false), ColumnPlan::Text)
    } else if db_type == &DB_TYPE_NCHAR {
        (text(true, true), ColumnPlan::Text)
    } else if db_type == &DB_TYPE_RAW || db_type == &DB_TYPE_LONG_RAW {
        (SqlType::Raw, ColumnPlan::Bytes)
    } else if db_type == &DB_TYPE_DATE {
        (SqlType::Date, ColumnPlan::Timestamp)
    } else if db_type == &DB_TYPE_TIMESTAMP {
        (SqlType::Timestamp, ColumnPlan::Timestamp)
    } else if db_type == &DB_TYPE_TIMESTAMP_TZ {
        (
            SqlType::TimestampWithTimeZone,
            ColumnPlan::TimestampWithZone,
        )
    } else if db_type == &DB_TYPE_TIMESTAMP_LTZ {
        (SqlType::Unsupported, ColumnPlan::UnsupportedTimestampLtz)
    } else if db_type == &DB_TYPE_CLOB {
        (
            SqlType::CharacterLob { national: false },
            ColumnPlan::Lob(LobKind::Character),
        )
    } else if db_type == &DB_TYPE_NCLOB {
        (
            SqlType::CharacterLob { national: true },
            ColumnPlan::Lob(LobKind::NationalCharacter),
        )
    } else if db_type == &DB_TYPE_BLOB {
        (SqlType::BinaryLob, ColumnPlan::Lob(LobKind::Binary))
    } else if db_type == &DB_TYPE_BOOLEAN {
        (SqlType::Boolean, ColumnPlan::Boolean)
    } else if db_type == &DB_TYPE_CURSOR {
        // Rejected before the fetch starts; see `cursor::describe`.
        (SqlType::Cursor, ColumnPlan::Unsupported)
    } else if db_type == &DB_TYPE_ROWID || db_type == &DB_TYPE_UROWID {
        (SqlType::Unsupported, ColumnPlan::UnsupportedRowid)
    } else if db_type == &DB_TYPE_INTERVAL_DS {
        (SqlType::Unsupported, ColumnPlan::UnsupportedIntervalDs)
    } else if db_type == &DB_TYPE_INTERVAL_YM {
        (SqlType::Unsupported, ColumnPlan::UnsupportedIntervalYm)
    } else {
        (SqlType::Unsupported, ColumnPlan::Unsupported)
    }
}

const fn text(national: bool, fixed_length: bool) -> SqlType {
    SqlType::Text {
        national,
        fixed_length,
    }
}

/// The server's own type name, for diagnostics and for unsupported columns.
///
/// `oracledb`'s `DbType::name()` returns its own constant name
/// (`"DB_TYPE_NUMBER"`) and keeps the Oracle name private, so the mapping is
/// written out here.
pub(crate) fn native_type_name(db_type: &'static DbType) -> &'static str {
    let names: [(&'static DbType, &'static str); 26] = [
        (&DB_TYPE_NUMBER, "NUMBER"),
        (&DB_TYPE_BINARY_FLOAT, "BINARY_FLOAT"),
        (&DB_TYPE_BINARY_DOUBLE, "BINARY_DOUBLE"),
        (&DB_TYPE_VARCHAR, "VARCHAR2"),
        (&DB_TYPE_NVARCHAR, "NVARCHAR2"),
        (&DB_TYPE_CHAR, "CHAR"),
        (&DB_TYPE_NCHAR, "NCHAR"),
        (&DB_TYPE_LONG, "LONG"),
        (&DB_TYPE_LONG_NVARCHAR, "LONG NVARCHAR"),
        (&DB_TYPE_RAW, "RAW"),
        (&DB_TYPE_LONG_RAW, "LONG RAW"),
        (&DB_TYPE_DATE, "DATE"),
        (&DB_TYPE_TIMESTAMP, "TIMESTAMP"),
        (&DB_TYPE_TIMESTAMP_TZ, "TIMESTAMP WITH TIME ZONE"),
        (&DB_TYPE_TIMESTAMP_LTZ, "TIMESTAMP WITH LOCAL TIME ZONE"),
        (&DB_TYPE_CLOB, "CLOB"),
        (&DB_TYPE_NCLOB, "NCLOB"),
        (&DB_TYPE_BLOB, "BLOB"),
        (&DB_TYPE_BFILE, "BFILE"),
        (&DB_TYPE_BOOLEAN, "BOOLEAN"),
        (&DB_TYPE_CURSOR, "CURSOR"),
        (&DB_TYPE_ROWID, "ROWID"),
        (&DB_TYPE_UROWID, "UROWID"),
        (&DB_TYPE_INTERVAL_DS, "INTERVAL DAY TO SECOND"),
        (&DB_TYPE_INTERVAL_YM, "INTERVAL YEAR TO MONTH"),
        (&DB_TYPE_JSON, "JSON"),
    ];
    for (candidate, name) in names {
        if db_type == candidate {
            return name;
        }
    }
    if db_type == &DB_TYPE_VECTOR {
        "VECTOR"
    } else if db_type == &DB_TYPE_XMLTYPE {
        "XMLTYPE"
    } else if db_type == &DB_TYPE_OBJECT {
        "OBJECT"
    } else {
        db_type.name()
    }
}

/// Builds the contract's description of one result column.
///
/// Precision and scale are attached only where they mean something. Oracle
/// reports `precision = 0, scale = 0` for character and binary columns, and
/// passing that through would state a decimal precision of zero rather than
/// "none" — a wrong answer dressed as a known one. For the timestamp family,
/// `scale` carries the declared fractional-seconds precision, which is the
/// contract's documented reading of that slot.
pub(crate) fn column_metadata(meta: &Metadata) -> ColumnMetadata {
    let (sql_type, _) = plan_for(meta.db_type());
    let mut metadata = ColumnMetadata::new(meta.name(), sql_type)
        .with_nullable(meta.nullable())
        .with_native_type_name(native_type_name(meta.db_type()));

    match sql_type {
        SqlType::Number => {
            metadata = metadata.with_precision_scale(meta.precision(), meta.scale());
        }
        SqlType::Timestamp | SqlType::TimestampWithTimeZone => {
            metadata = metadata.with_precision_scale(0, meta.scale());
        }
        _ => {}
    }
    if meta.max_size() > 0 {
        metadata = metadata.with_max_size_bytes(meta.max_size());
    }
    metadata
}

/// Accumulates one column of a batch.
pub(crate) struct ColumnBuilder {
    plan: ColumnPlan,
    data: Buffer,
    null_rows: Vec<usize>,
    rows: usize,
    scratch: String,
}

enum Buffer {
    Number(Vec<Number>),
    Float(Vec<f32>),
    Double(Vec<f64>),
    Text(TextColumn),
    Bytes(BytesColumn),
    Timestamp(Vec<Timestamp>),
    Boolean(Vec<bool>),
    Lob(Vec<Option<LobLocator>>),
    Unsupported(TextColumn),
}

impl ColumnBuilder {
    /// A builder sized for `capacity` rows.
    pub(crate) fn new(plan: ColumnPlan, capacity: usize) -> Self {
        let data = match plan {
            ColumnPlan::Number => Buffer::Number(Vec::with_capacity(capacity)),
            ColumnPlan::Float => Buffer::Float(Vec::with_capacity(capacity)),
            ColumnPlan::Double => Buffer::Double(Vec::with_capacity(capacity)),
            ColumnPlan::Text => Buffer::Text(TextColumn::with_capacity(capacity, capacity * 16)),
            ColumnPlan::Bytes => Buffer::Bytes(BytesColumn::with_capacity(capacity, capacity * 16)),
            ColumnPlan::Timestamp | ColumnPlan::TimestampWithZone => {
                Buffer::Timestamp(Vec::with_capacity(capacity))
            }
            ColumnPlan::Boolean => Buffer::Boolean(Vec::with_capacity(capacity)),
            ColumnPlan::Lob(_) => Buffer::Lob(Vec::with_capacity(capacity)),
            ColumnPlan::UnsupportedRowid
            | ColumnPlan::UnsupportedIntervalDs
            | ColumnPlan::UnsupportedIntervalYm
            | ColumnPlan::UnsupportedTimestampLtz
            | ColumnPlan::Unsupported => {
                Buffer::Unsupported(TextColumn::with_capacity(capacity, capacity * 24))
            }
        };
        Self {
            plan,
            data,
            null_rows: Vec::new(),
            rows: 0,
            scratch: String::new(),
        }
    }

    /// Appends the cell at `index` of `row`.
    pub(crate) fn push(
        &mut self,
        row: &mut Row,
        index: usize,
        connection: ConnectionId,
        closed: &crate::conn::Closed,
    ) -> DbResult<()> {
        let row_number = self.rows;
        self.rows += 1;
        match (&mut self.data, self.plan) {
            (Buffer::Number(values), _) => match get::<OracleNumber>(row, index)? {
                Some(value) => values.push(Number::parse(render_into(&mut self.scratch, &value))?),
                None => {
                    values.push(Number::ZERO);
                    self.null_rows.push(row_number);
                }
            },
            (Buffer::Float(values), _) => match get::<f32>(row, index)? {
                Some(value) => values.push(value),
                None => {
                    values.push(0.0);
                    self.null_rows.push(row_number);
                }
            },
            (Buffer::Double(values), _) => match get::<f64>(row, index)? {
                Some(value) => values.push(value),
                None => {
                    values.push(0.0);
                    self.null_rows.push(row_number);
                }
            },
            (Buffer::Text(values), _) => match get::<String>(row, index)? {
                Some(value) => values.push(&value),
                None => {
                    values.push_null_placeholder();
                    self.null_rows.push(row_number);
                }
            },
            (Buffer::Bytes(values), _) => match get::<Vec<u8>>(row, index)? {
                Some(value) => values.push(&value),
                None => {
                    values.push_null_placeholder();
                    self.null_rows.push(row_number);
                }
            },
            (Buffer::Boolean(values), _) => match get::<bool>(row, index)? {
                Some(value) => values.push(value),
                None => {
                    values.push(false);
                    self.null_rows.push(row_number);
                }
            },
            (Buffer::Timestamp(values), plan) => match get::<OracleTimestamp>(row, index)? {
                Some(value) => {
                    values.push(to_timestamp(&value, plan == ColumnPlan::TimestampWithZone)?);
                }
                None => {
                    // The mask, not this value, records nullness; the contract
                    // consults the mask before it reads a cell. Built through
                    // the fallible constructor so nothing here can panic.
                    values.push(Timestamp::from_source(1970, 1, 1, 0, 0, 0)?);
                    self.null_rows.push(row_number);
                }
            },
            (Buffer::Lob(values), ColumnPlan::Lob(kind)) => {
                match take::<Lob>(row, index)? {
                    Some(lob) => values.push(Some(LobLocator::new(Box::new(
                        OracleLobStream::new(lob, kind, connection, closed.clone()),
                    )))),
                    None => {
                        values.push(None);
                        self.null_rows.push(row_number);
                    }
                }
            }
            (Buffer::Lob(_), _) => {
                return Err(DbError::internal("LOB buffer used for a non-LOB column"));
            }
            (Buffer::Unsupported(values), plan) => {
                let rendered = match plan {
                    ColumnPlan::UnsupportedRowid => {
                        get::<String>(row, index)?.map(|value| render(&mut self.scratch, &value))
                    }
                    ColumnPlan::UnsupportedIntervalDs => get::<OracleIntervalDS>(row, index)?
                        .map(|value| render(&mut self.scratch, &value)),
                    ColumnPlan::UnsupportedIntervalYm => get::<OracleIntervalYM>(row, index)?
                        .map(|value| render(&mut self.scratch, &value)),
                    ColumnPlan::UnsupportedTimestampLtz => get::<OracleTimestamp>(row, index)?
                        .map(|value| render_local_time_zone(&mut self.scratch, &value)),
                    _ => {
                        return Err(DbError::new(
                            ErrorKind::Unsupported,
                            "this driver cannot decode values of this column's type",
                        ));
                    }
                };
                match rendered {
                    Some(()) => values.push(&self.scratch),
                    None => {
                        values.push_null_placeholder();
                        self.null_rows.push(row_number);
                    }
                }
            }
        }
        Ok(())
    }

    /// Finishes the column.
    pub(crate) fn finish(self) -> DbResult<Column> {
        let data = match self.data {
            Buffer::Number(values) => ColumnData::Number(values),
            Buffer::Float(values) => ColumnData::Float(values),
            Buffer::Double(values) => ColumnData::Double(values),
            Buffer::Text(values) => ColumnData::Text(values),
            Buffer::Bytes(values) => ColumnData::Bytes(values),
            Buffer::Timestamp(values) => ColumnData::Timestamp(values),
            Buffer::Boolean(values) => ColumnData::Boolean(values),
            Buffer::Lob(values) => ColumnData::Lob(values),
            Buffer::Unsupported(values) => ColumnData::Unsupported(values),
        };
        let mut nulls = NullMask::new(self.rows);
        for row in self.null_rows {
            nulls.set_null(row)?;
        }
        Column::new(data, nulls)
    }
}

fn render(scratch: &mut String, value: &impl std::fmt::Display) {
    let _ = render_into(scratch, value);
}

/// Renders a `TIMESTAMP WITH LOCAL TIME ZONE` **without claiming a zone**.
///
/// `OracleTimestamp`'s own `Display` writes a trailing `Z` whenever the offset
/// fields are zero, which is exactly how this type arrives: the server
/// normalizes the value to the database time zone and sends no offset at all.
/// Rendering it as `2026-09-19T13:45:30.000000000Z` therefore asserts UTC on no
/// evidence — the driver has not asked for `DBTIMEZONE` and does not know the
/// session's zone either — and a user comparing the cell against `SELECT c FROM
/// t` in any other tool would see a different instant. The fields are written
/// bare instead; the column's `native_type_name` already says
/// `TIMESTAMP WITH LOCAL TIME ZONE`, which is what the value means.
fn render_local_time_zone(scratch: &mut String, value: &OracleTimestamp) {
    use std::fmt::Write as _;
    scratch.clear();
    let _ = write!(
        scratch,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}",
        value.year(),
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
        value.nanoseconds()
    );
}

fn get<'a, T>(row: &'a Row, index: usize) -> DbResult<Option<T>>
where
    Option<T>: oracledb::FromDbValue<'a>,
{
    row.get::<Option<T>>(index)
        .map_err(|e| crate::error::map(&e))
}

fn take<'a, T>(row: &'a mut Row, index: usize) -> DbResult<Option<T>>
where
    Option<T>: oracledb::FromDbValue<'a>,
{
    row.take::<Option<T>>(index)
        .map_err(|e| crate::error::map(&e))
}

/// Converts an Oracle timestamp into the contract's civil representation.
///
/// Decoding uses [`Timestamp::from_source`], the lenient path: a historical row
/// inside the ten days the Gregorian reform skipped is a fact the database
/// holds, and failing an entire fetch over it would serve nobody
/// (ADR-0002 M7).
pub(crate) fn to_timestamp(value: &OracleTimestamp, zoned: bool) -> DbResult<Timestamp> {
    if !zoned {
        return Ok(Timestamp::from_source(
            value.year(),
            value.month(),
            value.day(),
            value.hour(),
            value.minute(),
            value.second(),
        )?
        .with_nanosecond(value.nanoseconds())?);
    }

    // A `TIMESTAMP WITH TIME ZONE` arrives on the wire as **UTC** date and time
    // fields plus the zone's offset. `oracledb` hands both back unchanged, so
    // pairing them as they stand would name a different instant: a value stored
    // as `13:45:30 +07:00` would be reported as `06:45:30 +07:00`. The offset is
    // therefore applied here to recover the local fields the server was given.
    //
    // The shift uses proleptic Gregorian civil arithmetic. That differs from
    // Oracle's mixed Julian/Gregorian calendar only for dates before
    // 1582-10-15, and only if the shift crosses the ten-day gap. `DATE` and
    // `TIMESTAMP` values — which is where historical dates actually occur — are
    // untouched by this path.
    let minutes = i16::from(value.tz_hour_offset()) * 60 + i16::from(value.tz_minute_offset());
    let (year, month, day, hour, minute) = shift_minutes(
        value.year(),
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        i32::from(minutes),
    );
    Ok(
        Timestamp::from_source(year, month, day, hour, minute, value.second())?
            .with_nanosecond(value.nanoseconds())?
            .with_zone(TimeZone::offset(minutes)?),
    )
}

/// Adds `minutes` to a civil date and time, in the proleptic Gregorian
/// calendar.
pub(crate) fn shift_minutes(
    year: i16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    minutes: i32,
) -> (i16, u8, u8, u8, u8) {
    let total = i64::from(days_from_civil(year, month, day)) * 24 * 60
        + i64::from(hour) * 60
        + i64::from(minute)
        + i64::from(minutes);
    let days = total.div_euclid(24 * 60);
    let within = total.rem_euclid(24 * 60);
    let (year, month, day) = civil_from_days(i32::try_from(days).unwrap_or(0));
    (
        year,
        month,
        day,
        u8::try_from(within / 60).unwrap_or(0),
        u8::try_from(within % 60).unwrap_or(0),
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian civil date.
fn days_from_civil(year: i16, month: u8, day: u8) -> i32 {
    let y = i32::from(year) - i32::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i32::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i32::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i32) -> (i16, u8, u8) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (
        i16::try_from(y + i32::from(m <= 2)).unwrap_or(0),
        u8::try_from(m).unwrap_or(1),
        u8::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zone_offset_is_applied_to_the_utc_fields_on_the_wire() {
        // Oracle sends UTC + offset; the contract wants the local fields.
        assert_eq!(
            shift_minutes(2026, 9, 19, 6, 45, 7 * 60),
            (2026, 9, 19, 13, 45)
        );
        // Backwards over midnight, a month end, and a year end.
        assert_eq!(
            shift_minutes(2026, 9, 19, 2, 0, -5 * 60),
            (2026, 9, 18, 21, 0)
        );
        assert_eq!(
            shift_minutes(2026, 10, 1, 1, 30, -150),
            (2026, 9, 30, 23, 0)
        );
        assert_eq!(
            shift_minutes(2027, 1, 1, 0, 15, -30),
            (2026, 12, 31, 23, 45)
        );
        // Forwards across a leap day.
        assert_eq!(shift_minutes(2028, 2, 28, 23, 0, 120), (2028, 2, 29, 1, 0));
        // A half-hour zone, which is why the shift is in minutes.
        assert_eq!(
            shift_minutes(2026, 9, 19, 0, 0, -330),
            (2026, 9, 18, 18, 30)
        );
        // Round trip: applying and undoing the shift is the identity.
        for minutes in [-840, -330, 0, 270, 840] {
            let (y, mo, d, h, mi) = shift_minutes(2026, 3, 1, 0, 5, minutes);
            assert_eq!(
                shift_minutes(y, mo, d, h, mi, -minutes),
                (2026, 3, 1, 0, 5),
                "offset {minutes}"
            );
        }
    }

    #[test]
    fn every_spec_type_family_has_a_plan() {
        // `SPEC.md` §8 "Types": NUMBER, CHAR/VARCHAR2/NVARCHAR2, DATE,
        // TIMESTAMP, TIMESTAMP WITH TIME ZONE, RAW, CLOB/NCLOB/BLOB.
        let expected: [(&'static DbType, SqlType, ColumnPlan); 16] = [
            (&DB_TYPE_NUMBER, SqlType::Number, ColumnPlan::Number),
            (
                &DB_TYPE_BINARY_FLOAT,
                SqlType::BinaryFloat,
                ColumnPlan::Float,
            ),
            (
                &DB_TYPE_BINARY_DOUBLE,
                SqlType::BinaryDouble,
                ColumnPlan::Double,
            ),
            (&DB_TYPE_VARCHAR, text(false, false), ColumnPlan::Text),
            (&DB_TYPE_CHAR, text(false, true), ColumnPlan::Text),
            (&DB_TYPE_NVARCHAR, text(true, false), ColumnPlan::Text),
            (&DB_TYPE_NCHAR, text(true, true), ColumnPlan::Text),
            (&DB_TYPE_RAW, SqlType::Raw, ColumnPlan::Bytes),
            (&DB_TYPE_LONG_RAW, SqlType::Raw, ColumnPlan::Bytes),
            (&DB_TYPE_DATE, SqlType::Date, ColumnPlan::Timestamp),
            (
                &DB_TYPE_TIMESTAMP,
                SqlType::Timestamp,
                ColumnPlan::Timestamp,
            ),
            (
                &DB_TYPE_TIMESTAMP_TZ,
                SqlType::TimestampWithTimeZone,
                ColumnPlan::TimestampWithZone,
            ),
            (
                &DB_TYPE_CLOB,
                SqlType::CharacterLob { national: false },
                ColumnPlan::Lob(LobKind::Character),
            ),
            (
                &DB_TYPE_NCLOB,
                SqlType::CharacterLob { national: true },
                ColumnPlan::Lob(LobKind::NationalCharacter),
            ),
            (
                &DB_TYPE_BLOB,
                SqlType::BinaryLob,
                ColumnPlan::Lob(LobKind::Binary),
            ),
            (&DB_TYPE_BOOLEAN, SqlType::Boolean, ColumnPlan::Boolean),
        ];
        for (db_type, sql_type, plan) in expected {
            assert_eq!(plan_for(db_type), (sql_type, plan), "{}", db_type.name());
        }
    }

    #[test]
    fn types_the_contract_cannot_represent_become_unsupported_text() {
        // ADR-0002 M1: one such column must not hide the whole table.
        for (db_type, plan) in [
            (&DB_TYPE_ROWID, ColumnPlan::UnsupportedRowid),
            (&DB_TYPE_INTERVAL_DS, ColumnPlan::UnsupportedIntervalDs),
            (&DB_TYPE_INTERVAL_YM, ColumnPlan::UnsupportedIntervalYm),
            (&DB_TYPE_TIMESTAMP_LTZ, ColumnPlan::UnsupportedTimestampLtz),
        ] {
            let (sql_type, actual) = plan_for(db_type);
            assert_eq!(sql_type, SqlType::Unsupported, "{}", db_type.name());
            assert_eq!(actual, plan, "{}", db_type.name());
        }
    }

    #[test]
    fn the_servers_own_type_name_is_reported_not_the_drivers() {
        assert_eq!(native_type_name(&DB_TYPE_NUMBER), "NUMBER");
        assert_eq!(native_type_name(&DB_TYPE_NVARCHAR), "NVARCHAR2");
        assert_eq!(
            native_type_name(&DB_TYPE_TIMESTAMP_TZ),
            "TIMESTAMP WITH TIME ZONE"
        );
        assert_eq!(
            native_type_name(&DB_TYPE_INTERVAL_DS),
            "INTERVAL DAY TO SECOND"
        );
        assert_eq!(native_type_name(&DB_TYPE_NCLOB), "NCLOB");
    }

    #[test]
    fn timestamps_carry_their_offset_including_negative_ones() {
        let naive = OracleTimestamp::new_timestamp(2026, 9, 19, 13, 45, 30, 123_456_789);
        let converted = to_timestamp(&naive, false).expect("valid");
        assert_eq!(converted.year(), 2026);
        assert_eq!(converted.nanosecond(), 123_456_789);
        assert_eq!(converted.zone(), TimeZone::Unspecified);

        let east = OracleTimestamp::new_timestamp_tz(2026, 9, 19, 13, 45, 30, 0, 7, 0);
        assert_eq!(
            to_timestamp(&east, true)
                .expect("valid")
                .zone()
                .offset_minutes(),
            Some(420)
        );

        let west = OracleTimestamp::new_timestamp_tz(2026, 9, 19, 13, 45, 30, 0, -5, -30);
        assert_eq!(
            to_timestamp(&west, true)
                .expect("valid")
                .zone()
                .offset_minutes(),
            Some(-330)
        );
    }

    #[test]
    fn a_bc_date_and_a_julian_leap_day_decode_rather_than_fail() {
        let bc = OracleTimestamp::new_date(-44, 3, 15);
        assert_eq!(to_timestamp(&bc, false).expect("valid").year(), -44);

        let julian = OracleTimestamp::new_date(1500, 2, 29);
        assert_eq!(to_timestamp(&julian, false).expect("valid").day(), 29);

        // Inside the Gregorian reform gap: strict construction refuses it, the
        // decode path accepts it (ADR-0002 M7).
        let reform = OracleTimestamp::new_date(1582, 10, 10);
        assert!(to_timestamp(&reform, false).is_ok());
    }

    #[test]
    fn a_decoder_bug_is_still_rejected() {
        let impossible = OracleTimestamp::new_date(2026, 13, 1);
        let error = to_timestamp(&impossible, false).expect_err("month 13 is a decoder bug");
        assert_eq!(error.kind(), ErrorKind::DataConversion);
    }

    #[test]
    fn the_null_placeholder_is_a_valid_timestamp() {
        let placeholder = Timestamp::from_source(1970, 1, 1, 0, 0, 0).expect("valid");
        assert_eq!(placeholder.year(), 1970);
    }
}
