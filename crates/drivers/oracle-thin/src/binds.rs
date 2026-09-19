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
///
/// Values the upstream encoder would mishandle are refused **before** the parse,
/// because the parse is the last point at which this wrapper is still in
/// control: see [`encoder_defect`].
fn to_oracle_number(value: Number) -> DbResult<OracleNumber> {
    let (digits, decimal_point_index) = upstream_shape(value);
    if let Some(defect) = encoder_defect(digits, decimal_point_index) {
        return Err(DbError::new(ErrorKind::Unsupported, defect.refusal(value)));
    }
    value.to_string().parse::<OracleNumber>().map_err(|error| {
        DbError::new(
            ErrorKind::DataConversion,
            format!("{value} is not a value this database can hold: {error}"),
        )
    })
}

/// The size of the upstream digit array (`constants::ORA_NUM_MAX_DIGITS`), and
/// so the largest digit position its encoder can index safely.
const MAX_BOUND_NUMBER_DIGITS: usize = 40;

/// A way `oracledb` 26.0.0-beta.3's `OracleNumber` encoder gets a value wrong.
///
/// Both were found in spike S2 against the live database, and both are derived
/// here from the upstream source rather than from the examples that exposed
/// them — see [`encoder_defect`] for the derivation and
/// `binds::tests::the_refusal_predicate_covers_every_shape_the_encoder_mishandles`
/// for the proof that the predicate below is exactly right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncoderDefect {
    /// `to_buf` writes the base-100 digit pairs one place out and the server
    /// stores a value ten times too large, with no error anywhere (U-1).
    ScaledByTen,
    /// `to_buf` reads past the end of its 40-byte `digits` array; the panic
    /// unwinds while the client mutex is held and the **process aborts**
    /// (U-2 compounded by U-4).
    ReadsPastItsDigitBuffer,
}

impl EncoderDefect {
    /// The message a refused bind carries. It names the upstream defect, so a
    /// user who sees it can tell a Reldex limitation from a deliberate policy.
    fn refusal(self, value: Number) -> String {
        match self {
            Self::ScaledByTen => format!(
                "this driver version cannot bind {value}: its decimal exponent is odd \
                 and negative, and oracledb 26.0.0-beta.3's NUMBER encoder (upstream \
                 defect U-1) then writes the base-100 digit pairs one place out, so the \
                 server would silently store a value ten times too large. Write it as a \
                 literal in the statement text instead"
            ),
            Self::ReadsPastItsDigitBuffer => format!(
                "this driver version cannot bind {value}: oracledb 26.0.0-beta.3's NUMBER \
                 encoder (upstream defect U-2) would read past the end of its \
                 {MAX_BOUND_NUMBER_DIGITS}-digit buffer while encoding it, and the \
                 resulting panic aborts the whole process (U-4). Write it as a literal \
                 in the statement text instead"
            ),
        }
    }
}

/// Whether `OracleNumber::to_buf` mishandles a value of this shape, derived
/// from the upstream encoder rather than from examples.
///
/// `to_buf` (`src/ora_type/number.rs:293-357`) works on `num_digits` (`n`
/// below) significant digit positions and `decimal_point_index` (`d`), and does
/// exactly this:
///
/// ```text
/// prepend_zero = (d % 2 == 1)      // Rust `%`: true only for a POSITIVE odd d
/// if prepend_zero { n += 1; d += 1 }
/// if n % 2 == 1   { n += 1 }
/// for i in 0..n/2 {
///     read digits[pos];  if i == 0 && prepend_zero { pos += 1 }
///                        else { read digits[pos + 1]; pos += 2 }
/// }
/// ```
///
/// Two consequences follow, and they are the whole of the refusal set:
///
/// - **Alignment.** A base-100 pair must start on an even digit position, so a
///   zero has to be prepended exactly when `d` is **odd**, either sign. Rust's
///   `%` yields `-1` for a negative odd `d`, so upstream skips the prepend and
///   the pairs land one place out — [`EncoderDefect::ScaledByTen`].
/// - **Bounds.** With the prepend, the last index read is `n` when `n` is even
///   and `n - 1` when odd; without it, `n - 1` when `n` is even and `n` when
///   odd. Against a 40-byte array that is out of bounds for `n >= 41`
///   regardless, and for `n == 40` exactly when the prepend happens — which is
///   why "magnitude ≥ 1E40" is *not* the criterion. `9.99E39` is larger than
///   anything refused here and encodes correctly, while
///   `1.234567890123456789012345678901234567891` — a value between one and two,
///   `n == 40`, `d == 1` — aborts the process
///   ([`EncoderDefect::ReadsPastItsDigitBuffer`]).
///
/// The server never produces this shape itself: an Oracle NUMBER holds 20
/// base-100 pairs, and an odd `d` spends one position on the alignment zero, so
/// 40 significant digits and an odd `d` cannot arrive together from a query
/// (`SELECT 10/3 FROM dual` returns 39 digits). It is reachable from a value a
/// user typed or Reldex computed, which is the path that matters here.
const fn encoder_defect(digits: usize, decimal_point_index: i32) -> Option<EncoderDefect> {
    if digits == 0 {
        // Zero: `to_buf` writes the two-byte constant and returns before it
        // looks at `digits` or the index at all.
        return None;
    }
    let odd = decimal_point_index.rem_euclid(2) == 1;
    // Upstream prepends only for a positive odd index, which is both defects.
    let prepends_zero = odd && decimal_point_index > 0;
    if digits > MAX_BOUND_NUMBER_DIGITS || (prepends_zero && digits >= MAX_BOUND_NUMBER_DIGITS) {
        return Some(EncoderDefect::ReadsPastItsDigitBuffer);
    }
    if odd && !prepends_zero {
        return Some(EncoderDefect::ScaledByTen);
    }
    None
}

/// The `num_digits` and `decimal_point_index` that `OracleNumber::from_str`
/// derives from this value's canonical plain-decimal rendering.
///
/// Closed form rather than a second string walk, and exact for every [`Number`]:
/// the contract normalizes to `±0.d₁…dₙ × 10^exponent` with `d₁ != 0` and
/// `dₙ != 0`, and `Display` writes that as plain decimal. So `from_str`'s
/// `decimal_point_index` *is* the exponent, and its `num_digits` is the number
/// of digit positions from the first significant digit to the last — which is
/// the digit count, except for an integer padded with trailing zeros, where it
/// is the exponent. `from_str` folds those trailing zeros in without a bounds
/// check, which is exactly how a 40-byte array comes to be indexed at 41.
///
/// `tests::the_closed_form_shape_matches_a_walk_of_the_upstream_parser` checks
/// this against a transcription of `from_str` itself.
fn upstream_shape(value: Number) -> (usize, i32) {
    let exponent = i32::from(value.exponent());
    let digits = value
        .digit_count()
        .max(usize::try_from(exponent).unwrap_or(0));
    (digits, exponent)
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

    /// Transcribes `OracleNumber::from_str`'s `num_digits` and
    /// `decimal_point_index` arithmetic, character by character, so the closed
    /// form in [`upstream_shape`] can be checked rather than believed.
    ///
    /// Returns `None` where `from_str` itself returns `Err`.
    fn from_str_shape(text: &str) -> Option<(usize, i32)> {
        let mut num_digits = 0_usize;
        let mut num_zeros = 0_usize;
        let mut point = false;
        let mut index = 0_i32;
        let mut first = true;
        for ch in text.chars() {
            if first {
                first = false;
                if ch == '-' || ch == '+' {
                    continue;
                }
            }
            if ch == '.' && !point {
                point = true;
                if num_digits > 0 {
                    num_digits += num_zeros;
                    index = i32::try_from(num_digits).expect("fits");
                }
                num_zeros = 0;
            } else {
                // `from_str` fails on anything that is not a digit here.
                let digit = ch.to_digit(10)?;
                if digit == 0 {
                    num_zeros += 1;
                } else {
                    if num_digits > 0 {
                        if num_digits + num_zeros + 1 > MAX_BOUND_NUMBER_DIGITS {
                            return None; // `Error::invalid_oracle_number`
                        }
                        num_digits += num_zeros;
                    } else if point {
                        index = -i32::try_from(num_zeros).expect("fits");
                    }
                    num_zeros = 0;
                    num_digits += 1;
                }
            }
        }
        if num_digits == 0 && num_zeros == 0 {
            return None;
        }
        if num_digits == 0 {
            index = 0;
        } else if !point {
            num_digits += num_zeros;
            index = i32::try_from(num_digits).expect("fits");
        }
        Some((num_digits, index))
    }

    /// Transcribes `OracleNumber::to_buf`'s index arithmetic: the largest
    /// `digits` index it reads, and whether it prepended the alignment zero.
    fn to_buf_probe(num_digits: usize, decimal_point_index: i32) -> (Option<usize>, bool) {
        if num_digits == 0 {
            return (None, false); // upstream's "the value is zero" fast path
        }
        let mut digits = num_digits;
        let mut index = decimal_point_index;
        let mut prepend_zero = false;
        if index % 2 == 1 {
            prepend_zero = true;
            digits += 1;
            index += 1;
        }
        if digits % 2 == 1 {
            digits += 1;
        }
        let _ = index;
        let mut position = 0_usize;
        let mut highest = 0_usize;
        for pair in 0..(digits / 2) {
            highest = highest.max(position);
            if pair == 0 && prepend_zero {
                position += 1;
            } else {
                highest = highest.max(position + 1);
                position += 2;
            }
        }
        (Some(highest), prepend_zero)
    }

    #[test]
    fn the_refusal_predicate_covers_every_shape_the_encoder_mishandles() {
        // The proof the reviewer asked for, and the reason the old
        // "magnitude >= 1E40" criterion was wrong: walk **every** shape a
        // `Number` can present to the upstream encoder — 1..=40 significant
        // digits (plus the over-long counts its own `from_str` can produce)
        // across the whole legal exponent range — and check the guard against a
        // transcription of `to_buf`'s own index arithmetic.
        let mut refused_out_of_bounds = 0_usize;
        let mut refused_scaled = 0_usize;
        for digits in 0..=130_usize {
            for index in -129_i32..=126 {
                let (highest, prepends) = to_buf_probe(digits, index);
                let out_of_bounds = highest.is_some_and(|i| i >= MAX_BOUND_NUMBER_DIGITS);
                // A base-100 pair must start on an even digit position, so the
                // alignment zero is required for every odd index, either sign.
                let needs_prepend = index.rem_euclid(2) == 1;
                let mis_encodes = digits > 0 && needs_prepend != prepends;

                match encoder_defect(digits, index) {
                    Some(EncoderDefect::ReadsPastItsDigitBuffer) => {
                        assert!(out_of_bounds, "{digits} digits, index {index}");
                        refused_out_of_bounds += 1;
                    }
                    Some(EncoderDefect::ScaledByTen) => {
                        assert!(
                            mis_encodes && !out_of_bounds,
                            "{digits} digits, index {index}"
                        );
                        refused_scaled += 1;
                    }
                    None => assert!(
                        !out_of_bounds && !mis_encodes,
                        "{digits} digits, index {index}: the encoder is not safe here \
                         (out of bounds: {out_of_bounds}, mis-encodes: {mis_encodes})"
                    ),
                }
            }
        }
        assert!(refused_out_of_bounds > 0 && refused_scaled > 0);

        // The two boundaries the old guard missed, spelled out.
        assert_eq!(
            encoder_defect(40, 2),
            None,
            "40 digits, even index, is fine"
        );
        assert_eq!(
            encoder_defect(40, 1),
            Some(EncoderDefect::ReadsPastItsDigitBuffer),
            "40 digits with an odd positive index reads digits[40]"
        );
        assert_eq!(encoder_defect(39, 1), None, "39 digits is always in bounds");
    }

    #[test]
    fn the_closed_form_shape_matches_a_walk_of_the_upstream_parser() {
        for text in [
            "0",
            "1",
            "-1",
            "0.5",
            "-0.5",
            "0.05",
            "0.0005",
            "1500",
            "123.45",
            "1E-129",
            "1E-130",
            "1E39",
            "1E40",
            "1E41",
            "9.99E125",
            "3.333333333333333333333333333333333333339",
            "1.234567890123456789012345678901234567891",
            "123.4567890123456789012345678901234567891",
            "-123.4567890123456789012345678901234567891",
            "1234567890123456789012345678901234567890",
            "0.1234567890123456789012345678901234567891",
        ] {
            let value = Number::parse(text).unwrap_or_else(|_| panic!("{text}"));
            let rendered = value.to_string();
            assert_eq!(
                Some(upstream_shape(value)),
                from_str_shape(&rendered),
                "{text} rendered as {rendered}"
            );
        }
    }

    #[test]
    fn a_number_the_upstream_encoder_would_abort_on_is_refused_cleanly() {
        // Every one of these reads `digits[40]` inside `to_buf` and takes the
        // process down with it (U-2 with U-4). The last five are the ones the
        // old "magnitude >= 1E40" guard let through, and they are the size of
        // one, three or a hundred — not of 1E40.
        for text in [
            "9.9999999999999999999999999999999999999E125",
            "1E40",
            "1E41",
            "3.333333333333333333333333333333333333339",
            "1.234567890123456789012345678901234567891",
            "123.4567890123456789012345678901234567891",
            "-3.333333333333333333333333333333333333339",
            "-123.4567890123456789012345678901234567891",
        ] {
            let value = Number::parse(text).unwrap_or_else(|_| panic!("{text} is a legal NUMBER"));
            let error = to_oracle_number(value)
                .err()
                .unwrap_or_else(|| panic!("{text} must be refused, not attempted"));
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{text}");
            assert!(error.message().contains("U-2"), "{text}: {error}");
            assert!(error.message().contains("literal"), "{text}: {error}");
        }

        // Their in-bounds neighbours must still bind, or the refusal set has
        // grown into a functional regression.
        for text in [
            // 40 digit positions with an **even** decimal-point index: the
            // magnitude is above 1E39 and the encoder handles it exactly, which
            // is why the criterion is the index, not the magnitude.
            "1E39",
            "9.99E39",
            "9.99E38",
            "1234567890123456789012345678901234567890",
            "12.34567890123456789012345678901234567891",
            "0.1234567890123456789012345678901234567891",
        ] {
            assert!(
                to_oracle_number(Number::parse(text).expect("valid")).is_ok(),
                "{text} was refused although the encoder handles it"
            );
        }
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
            assert!(error.message().contains("U-1"), "{wrong}: {error}");
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
        let shape = |text: &str| upstream_shape(Number::parse(text).expect("valid"));
        assert_eq!(shape("0.05").1, -1);
        assert_eq!(shape("0.005").1, -2);
        assert_eq!(shape("0.5").1, 0);
        assert_eq!(shape("123.45").1, 3);
        assert_eq!(shape("42").1, 2);
        assert_eq!(shape("1E39"), (40, 40));
        assert_eq!(shape("1E40"), (41, 41));
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
