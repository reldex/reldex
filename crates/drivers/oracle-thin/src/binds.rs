//! Bind parameters in both directions (ADR-0002 D5, D6).
//!
//! `oracledb` takes binds as `&[&dyn ToDbValue]` (or name/value pairs), and
//! declares a **pure OUT** bind by binding the `&'static DbType` itself — there
//! is no separate "declare an output of type T, size N" call. Two consequences
//! the wrapper cannot paper over:
//!
//! - [`OutBindSpec::max_size_bytes`] above the driver's own default for the type
//!   (4000 bytes for `VARCHAR2`) cannot be requested, so it is refused rather
//!   than silently truncated.
//! - An IN OUT bind is bound exactly like an IN bind; the direction comes back
//!   from the server's describe of the PL/SQL call.

use reldex_db_driver_api::{
    Bind, BindValue, DbError, DbResult, ErrorKind, Number, OutBindSpec, SqlType, TimeZone,
    Timestamp,
};

use oracledb::{
    DB_TYPE_BINARY_DOUBLE, DB_TYPE_BINARY_FLOAT, DB_TYPE_BLOB, DB_TYPE_BOOLEAN, DB_TYPE_CLOB,
    DB_TYPE_CURSOR, DB_TYPE_DATE, DB_TYPE_NCLOB, DB_TYPE_NUMBER, DB_TYPE_NVARCHAR, DB_TYPE_RAW,
    DB_TYPE_TIMESTAMP, DB_TYPE_TIMESTAMP_TZ, DB_TYPE_VARCHAR, DbType, OracleNumber,
    OracleTimestamp, ToDbValue,
};

/// The largest OUT bind this driver can request, because `oracledb` sizes an
/// output placeholder from the type's own default and offers no override.
pub(crate) const MAX_OUT_BIND_BYTES: u32 = 4000;

/// One bind value, owned for the duration of the call.
pub(crate) enum OwnedBind {
    /// SQL NULL, typed as `VARCHAR2` because a bind must carry some type.
    Null(Option<String>),
    Text(String),
    Bytes(Vec<u8>),
    Number(OracleNumber),
    Float(f32),
    Double(f64),
    Boolean(bool),
    Timestamp(OracleTimestamp),
    /// A pure OUT placeholder: the type itself is the bind value.
    Output(&'static DbType),
}

impl OwnedBind {
    /// Borrows the value as the trait object `oracledb` expects.
    pub(crate) fn as_dyn(&self) -> &dyn ToDbValue {
        match self {
            Self::Null(value) => value,
            Self::Text(value) => value,
            Self::Bytes(value) => value,
            Self::Number(value) => value,
            Self::Float(value) => value,
            Self::Double(value) => value,
            Self::Boolean(value) => value,
            Self::Timestamp(value) => value,
            Self::Output(value) => value,
        }
    }
}

/// Converts one contract bind into the value `oracledb` will send.
pub(crate) fn to_owned_bind(bind: &Bind) -> DbResult<OwnedBind> {
    match bind {
        Bind::In(value) | Bind::InOut { value, .. } => from_bind_value(value),
        Bind::Out(spec) => Ok(OwnedBind::Output(out_bind_type(*spec)?)),
        _ => Err(DbError::new(
            ErrorKind::Unsupported,
            "this driver does not understand this bind direction",
        )),
    }
}

fn from_bind_value(value: &BindValue) -> DbResult<OwnedBind> {
    let bind = match value {
        BindValue::Null => OwnedBind::Null(None),
        BindValue::Boolean(value) => OwnedBind::Boolean(*value),
        BindValue::Number(value) => OwnedBind::Number(to_oracle_number(*value)?),
        BindValue::Float(value) => OwnedBind::Float(*value),
        BindValue::Double(value) => OwnedBind::Double(*value),
        BindValue::Text(value) | BindValue::Json(value) => OwnedBind::Text(value.clone()),
        BindValue::Bytes(value) => OwnedBind::Bytes(value.clone()),
        BindValue::Timestamp(value) => OwnedBind::Timestamp(to_oracle_timestamp(*value)),
        _ => {
            return Err(DbError::new(
                ErrorKind::Unsupported,
                format!(
                    "this driver cannot bind a value of type {}",
                    value.type_name()
                ),
            ));
        }
    };
    Ok(bind)
}

/// Converts the contract's exact decimal into `OracleNumber`.
///
/// `OracleNumber`'s fields are private, so the only route in is its `FromStr`,
/// fed from the contract's canonical plain-decimal `Display`. Both sides carry
/// 40 significant digits, so this is lossless; it is one allocation per bound
/// number, which an upstream constructor from digits and exponent would remove.
fn to_oracle_number(value: Number) -> DbResult<OracleNumber> {
    let text = value.to_string();
    let (digits, decimal_point_index) = upstream_shape(&text);

    // Two defects in `oracledb` 26.0.0-beta.3's `OracleNumber` encoder are
    // guarded here. Both were found in spike S2, against the live database.
    //
    // 1. **Silent factor-of-ten corruption.** `to_buf` decides whether the
    //    base-100 digit pairs need a leading zero with
    //    `decimal_point_index % 2 == 1` (`src/ora_type/number.rs:314`). In Rust
    //    that is `-1`, not `1`, for a negative odd index, so the test is false
    //    and the pairs are written one place out. Binding `0.05` stores `0.5`;
    //    `0.0005` stores `0.005`; `1E-130` stores `1E-129`. Nothing reports an
    //    error — the wrong number is simply committed.
    // 2. **Process abort on large magnitudes.** `from_str` folds trailing zeros
    //    into `num_digits` without bounding it to the 40-byte `digits` array
    //    (`src/ora_type/number.rs:280-283`) and `to_buf` then indexes past its
    //    end (line 347). The resulting panic unwinds while the client mutex is
    //    held, and `impl Drop for StatementHolder`'s `.lock().unwrap()` on the
    //    poisoned mutex turns it into a process abort.
    //
    // Refusing is a real limitation and is documented as one. Corrupting a
    // number or killing the application is not a limitation but a defect, and a
    // database tool that does either is worse than one that says no.
    if decimal_point_index < 0 && decimal_point_index % 2 != 0 {
        return Err(DbError::new(
            ErrorKind::Unsupported,
            format!(
                "this driver version cannot bind {value}: the underlying Oracle crate \
                 encodes a value with an odd number of leading zeros after the decimal \
                 point ten times too large, and would store it silently wrong. Write it \
                 as a literal in the statement text instead"
            ),
        ));
    }
    if digits > MAX_BOUND_NUMBER_DIGITS {
        return Err(DbError::new(
            ErrorKind::Unsupported,
            format!(
                "this driver version cannot bind a NUMBER of magnitude \
                 1E{MAX_BOUND_NUMBER_DIGITS} or larger: the underlying Oracle crate panics \
                 while encoding it. Write it as a literal in the statement text instead"
            ),
        ));
    }
    text.parse::<OracleNumber>().map_err(|error| {
        DbError::new(
            ErrorKind::DataConversion,
            format!("{value} is not a value this database can hold: {error}"),
        )
    })
}

/// The size of the upstream digit array, and so the largest digit count its
/// encoder can index safely.
const MAX_BOUND_NUMBER_DIGITS: usize = 40;

/// Reproduces the `num_digits` and `decimal_point_index` that
/// `OracleNumber::from_str` derives from this text.
///
/// Mirrored rather than inferred: both upstream defects depend on these two
/// numbers exactly as that function computes them, trailing zeros and all.
fn upstream_shape(text: &str) -> (usize, i32) {
    let mut digits = 0_usize;
    let mut zeros = 0_usize;
    let mut point = false;
    let mut index = 0_i32;
    for ch in text.chars() {
        if ch == '.' && !point {
            point = true;
            if digits > 0 {
                digits += zeros;
                index = i32::try_from(digits).unwrap_or(i32::MAX);
            }
            zeros = 0;
        } else if let Some(value) = ch.to_digit(10) {
            if value == 0 {
                zeros += 1;
            } else {
                if digits > 0 {
                    digits += zeros;
                } else if point {
                    index = -i32::try_from(zeros).unwrap_or(i32::MAX);
                }
                zeros = 0;
                digits += 1;
            }
        }
    }
    if !point && digits > 0 {
        digits += zeros;
        index = i32::try_from(digits).unwrap_or(i32::MAX);
    }
    (digits, index)
}

fn to_oracle_timestamp(value: Timestamp) -> OracleTimestamp {
    match value.zone() {
        TimeZone::Offset { minutes } => {
            let (year, month, day, hour, minute) = crate::value::shift_minutes(
                value.year(),
                value.month(),
                value.day(),
                value.hour(),
                value.minute(),
                -i32::from(minutes),
            );
            OracleTimestamp::new_timestamp_tz(
                year,
                month,
                day,
                hour,
                minute,
                value.second(),
                value.nanosecond(),
                i8::try_from(minutes / 60).unwrap_or(0),
                i8::try_from(minutes % 60).unwrap_or(0),
            )
        }
        _ => OracleTimestamp::new_timestamp(
            value.year(),
            value.month(),
            value.day(),
            value.hour(),
            value.minute(),
            value.second(),
            value.nanosecond(),
        ),
    }
}

/// The database type used to declare an OUT placeholder.
pub(crate) fn out_bind_type(spec: OutBindSpec) -> DbResult<&'static DbType> {
    if let Some(requested) = spec.max_size_bytes()
        && requested > MAX_OUT_BIND_BYTES
    {
        return Err(DbError::new(
            ErrorKind::Unsupported,
            format!(
                "an output bind of {requested} bytes was requested, but this driver \
                 cannot size an output placeholder above {MAX_OUT_BIND_BYTES} bytes"
            ),
        ));
    }
    let db_type: &'static DbType = match spec.sql_type() {
        SqlType::Number => &DB_TYPE_NUMBER,
        SqlType::BinaryFloat => &DB_TYPE_BINARY_FLOAT,
        SqlType::BinaryDouble => &DB_TYPE_BINARY_DOUBLE,
        SqlType::Boolean => &DB_TYPE_BOOLEAN,
        SqlType::Text { national: true, .. } => &DB_TYPE_NVARCHAR,
        SqlType::Text { .. } | SqlType::Json => &DB_TYPE_VARCHAR,
        SqlType::Date => &DB_TYPE_DATE,
        SqlType::Timestamp => &DB_TYPE_TIMESTAMP,
        SqlType::TimestampWithTimeZone => &DB_TYPE_TIMESTAMP_TZ,
        SqlType::Raw => &DB_TYPE_RAW,
        SqlType::CharacterLob { national: false } => &DB_TYPE_CLOB,
        SqlType::CharacterLob { national: true } => &DB_TYPE_NCLOB,
        SqlType::BinaryLob => &DB_TYPE_BLOB,
        SqlType::Cursor => &DB_TYPE_CURSOR,
        other => {
            return Err(DbError::new(
                ErrorKind::Unsupported,
                format!("an output bind of type {other} is not supported"),
            ));
        }
    };
    Ok(db_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reldex_db_driver_api::BindValue;

    #[test]
    fn a_forty_digit_number_survives_the_string_round_trip() {
        // The only route into `OracleNumber` is `FromStr`, so this is the point
        // where silent precision loss would happen if it happened anywhere.
        let text = "1234567890123456789012345678901234567891";
        let number = Number::parse(text).expect("40 digits");
        let converted = to_oracle_number(number).expect("convertible");
        assert_eq!(converted.to_string(), text);
    }

    #[test]
    fn a_number_the_upstream_encoder_would_abort_on_is_refused_cleanly() {
        // `9.99..E125` is a legal Oracle NUMBER, but binding it panics inside
        // `oracledb` and the panic is turned into a process abort by a
        // `.lock().unwrap()` in a `Drop`. A refusal is the only safe answer
        // this wrapper can give (spike S2).
        let value = Number::parse("9.9999999999999999999999999999999999999E125")
            .expect("a legal Oracle NUMBER");
        let error = to_oracle_number(value).expect_err("must be refused, not attempted");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.message().contains("literal"), "{error}");

        // The boundary itself: 40 digits is fine, 41 is not.
        assert_eq!(
            upstream_shape("1234567890123456789012345678901234567890").0,
            40
        );
        assert_eq!(
            upstream_shape("12345678901234567890123456789012345678901").0,
            41
        );
        // Trailing zeros count, which is the whole of that defect; the
        // contract's canonical `Display` is plain decimal, never exponential.
        assert_eq!(
            upstream_shape(&Number::parse("1E41").expect("valid").to_string()).0,
            42
        );
    }

    #[test]
    fn a_number_the_upstream_encoder_would_scale_by_ten_is_refused_cleanly() {
        // An odd number of leading zeros after the decimal point is encoded one
        // base-100 place out by `oracledb` 26.0.0-beta.3: `0.05` is stored as
        // `0.5`, with no error anywhere. Refusing is the only honest answer a
        // wrapper can give (spike S2).
        for wrong in [
            "0.05", "0.0005", "0.0123", "0.000123", "1E-4", "1E-130", "-0.05",
        ] {
            let value = Number::parse(wrong).expect("valid");
            let error = to_oracle_number(value)
                .err()
                .unwrap_or_else(|| panic!("{wrong} must be refused, not silently changed"));
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{wrong}");
        }
        // The even-zero-count neighbours are unaffected and must still work.
        for fine in [
            "0.5", "0.005", "0.00005", "0.123", "0.00123", "1E-3", "1E-129", "-0.005",
        ] {
            assert!(
                to_oracle_number(Number::parse(fine).expect("valid")).is_ok(),
                "{fine} was refused although it encodes correctly"
            );
        }
        // The decimal point index is what decides, so check it directly.
        assert_eq!(upstream_shape("0.05").1, -1);
        assert_eq!(upstream_shape("0.005").1, -2);
        assert_eq!(upstream_shape("0.5").1, 0);
        assert_eq!(upstream_shape("123.45").1, 3);
        assert_eq!(upstream_shape("42").1, 2);
    }

    #[test]
    fn small_and_awkward_numbers_survive_too() {
        for text in ["0", "-1", "0.5", "-0.5", "1500", "123.45"] {
            let number = Number::parse(text).expect(text);
            let converted = to_oracle_number(number).expect(text);
            assert_eq!(
                Number::parse(&converted.to_string()).expect("reparsed"),
                number,
                "{text}"
            );
        }
    }

    #[test]
    fn a_zoned_timestamp_keeps_its_offset_through_the_bind() {
        let value = Timestamp::new(2026, 9, 19, 13, 45, 30)
            .expect("valid")
            .with_zone(TimeZone::offset(7 * 60).expect("valid"));
        let bound = to_oracle_timestamp(value);
        assert_eq!(bound.tz_hour_offset(), 7);
        assert_eq!(bound.tz_minute_offset(), 0);
        // The wire carries UTC, which is 13:45:30 +07:00 minus seven hours.
        assert_eq!(bound.hour(), 6);
        assert_eq!(bound.minute(), 45);

        let west = Timestamp::new(2026, 9, 19, 13, 45, 30)
            .expect("valid")
            .with_zone(TimeZone::offset(-330).expect("valid"));
        let bound = to_oracle_timestamp(west);
        assert_eq!(bound.tz_hour_offset(), -5);
        assert_eq!(bound.tz_minute_offset(), -30);
        assert_eq!((bound.hour(), bound.minute()), (19, 15));
    }

    #[test]
    fn a_null_bind_is_typed_rather_than_untyped() {
        let bind = from_bind_value(&BindValue::Null).expect("null is bindable");
        assert!(matches!(bind, OwnedBind::Null(None)));
        assert_eq!(bind.as_dyn().db_type(), &DB_TYPE_VARCHAR);
    }

    #[test]
    fn out_bind_types_cover_the_spec_matrix() {
        for (sql_type, expected) in [
            (SqlType::Number, &DB_TYPE_NUMBER),
            (SqlType::VARCHAR, &DB_TYPE_VARCHAR),
            (
                SqlType::Text {
                    national: true,
                    fixed_length: false,
                },
                &DB_TYPE_NVARCHAR,
            ),
            (SqlType::Date, &DB_TYPE_DATE),
            (SqlType::Timestamp, &DB_TYPE_TIMESTAMP),
            (SqlType::TimestampWithTimeZone, &DB_TYPE_TIMESTAMP_TZ),
            (SqlType::Raw, &DB_TYPE_RAW),
            (SqlType::Cursor, &DB_TYPE_CURSOR),
            (SqlType::BinaryLob, &DB_TYPE_BLOB),
        ] {
            let actual = out_bind_type(OutBindSpec::new(sql_type)).expect("supported");
            // By value, not by pointer: `DB_TYPE_*` are `const`, not `static`,
            // so every `&DB_TYPE_NUMBER` may be a distinct promoted temporary.
            // The driver's own type dispatch compares the same way.
            assert_eq!(actual, expected, "{sql_type}");
        }
    }

    #[test]
    fn an_out_bind_larger_than_the_driver_can_size_is_refused_not_truncated() {
        let spec = OutBindSpec::new(SqlType::VARCHAR).with_max_size_bytes(32_767);
        let error = out_bind_type(spec).expect_err("cannot be sized");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.message().contains("4000"), "{error}");

        // Anything within the driver's own default is fine.
        assert!(
            out_bind_type(OutBindSpec::new(SqlType::VARCHAR).with_max_size_bytes(4000)).is_ok()
        );
    }

    #[test]
    fn an_unsupported_out_bind_type_is_reported() {
        let error =
            out_bind_type(OutBindSpec::new(SqlType::Unsupported)).expect_err("not supported");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }
}
