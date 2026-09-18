//! Lossless decimal representation for database `NUMBER` values (ADR-0002 D5).
//!
//! A [`Number`] is sign + up to [`MAX_SIGNIFICANT_DIGITS`] significant decimal
//! digits + a decimal exponent, which covers the domain of an Oracle Database
//! `NUMBER`. It is `Copy` and allocation-free, so a numeric result cell costs no
//! heap traffic, and nothing converts through `f64` implicitly:
//! [`Number::to_f64_lossy`] is named for what it does.
//!
//! # Display is canonical, not pretty
//!
//! [`Number`]'s `Display` writes one form — plain positional decimal, with no
//! exponent and no thousands separators — and [`Number::parse`] reads it back
//! exactly. That makes `Display` a *serialization*, safe for round-tripping,
//! diffing and tests. It is deliberately **not** a presentation format: choosing
//! when a number becomes `1.23E+100`, how many decimals to show, or what the
//! group separator is depends on locale and column width, and belongs to the UI
//! (`SPEC.md` §13), not to a transport contract. `Display` therefore carries no
//! tunable thresholds at all.

use std::cmp::Ordering;
use std::fmt;
use std::fmt::Write as _;
use std::str::FromStr;

/// Maximum number of significant decimal digits a [`Number`] can hold.
///
/// Oracle Database documents `NUMBER` as 38 digits of *declarable* precision,
/// but the stored form is 20 base-100 mantissa bytes, which can carry up to 40
/// decimal digits — and computed values (notably division) reach that. The
/// primary driver's own decimal buffer is 40 digits for the same reason
/// (ADR-0001, `oracledb` review). The contract matches the storage, not the
/// documentation, so a value the server sends can never fail to be represented.
pub const MAX_SIGNIFICANT_DIGITS: usize = 40;

/// Smallest decimal exponent a [`Number`] can hold.
///
/// With the normalized form `±0.d₁…dₙ × 10^exponent`, this bounds the smallest
/// representable magnitude at `1 × 10^-130`, which is `NUMBER`'s documented
/// lower limit.
pub const MIN_EXPONENT: i16 = -129;

/// Largest decimal exponent a [`Number`] can hold.
///
/// This bounds the largest representable magnitude just below `1 × 10^126` —
/// `NUMBER`'s documented upper limit is `9.99…9 × 10^125`, which normalizes to
/// `0.999…9 × 10^126`.
pub const MAX_EXPONENT: i16 = 126;

/// Why a decimal value could not be represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumberError {
    /// The input was empty or contained only whitespace.
    Empty,
    /// The input was not a valid decimal literal.
    InvalidSyntax,
    /// A digit was outside `0..=9`.
    InvalidDigit,
    /// The value needs more than [`MAX_SIGNIFICANT_DIGITS`] significant digits.
    ///
    /// The contract refuses to round silently: silent precision loss is a
    /// correctness failure under `SPEC.md` §2.
    TooManyDigits,
    /// The magnitude is outside `[10^-130, 10^126)`.
    ExponentOutOfRange,
}

impl fmt::Display for NumberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "empty decimal value",
            Self::InvalidSyntax => "not a valid decimal literal",
            Self::InvalidDigit => "decimal digit outside 0..=9",
            Self::TooManyDigits => "more than 40 significant decimal digits",
            Self::ExponentOutOfRange => "decimal exponent outside the supported range",
        };
        f.write_str(text)
    }
}

impl std::error::Error for NumberError {}

/// An exact decimal value.
///
/// The value is `±0.d₁d₂…dₙ × 10^exponent` where `d₁ != 0`, `dₙ != 0` and
/// `n <= 40`. Zero is the unique canonical value with `n == 0`, exponent `0` and
/// a non-negative sign, so equality and hashing can be derived.
///
/// ```
/// use reldex_db_driver_api::Number;
///
/// let n: Number = "123.450".parse().expect("valid");
/// assert_eq!(n.to_string(), "123.45");
/// assert_eq!(n.digit_count(), 5);
/// assert_eq!(Number::from(1_500_i64).to_string(), "1500");
///
/// // Scientific notation is accepted on input and normalized away on output;
/// // `Display` is a canonical serialization, not a presentation format.
/// assert_eq!("1.5E+3".parse::<Number>().expect("valid").to_string(), "1500");
/// assert_eq!("-.5".parse::<Number>().expect("valid").to_string(), "-0.5");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Number {
    /// Significant digits, most significant first, values `0..=9`. Positions at
    /// or beyond `len` are always zero so equality/hashing can be derived.
    digits: [u8; MAX_SIGNIFICANT_DIGITS],
    len: u8,
    exponent: i16,
    negative: bool,
}

impl Number {
    /// The canonical zero.
    pub const ZERO: Self = Self {
        digits: [0; MAX_SIGNIFICANT_DIGITS],
        len: 0,
        exponent: 0,
        negative: false,
    };

    /// Builds a number from its normalized parts.
    ///
    /// `digits` are decimal digit *values* (`0..=9`), most significant first,
    /// interpreted as `±0.d₁…dₙ × 10^exponent`. Leading and trailing zeros are
    /// removed and the exponent adjusted, so callers may pass an unnormalized
    /// digit run straight from a wire decoder.
    ///
    /// # Errors
    ///
    /// Returns [`NumberError::InvalidDigit`] for a digit above 9,
    /// [`NumberError::TooManyDigits`] if more than [`MAX_SIGNIFICANT_DIGITS`]
    /// significant digits remain, and [`NumberError::ExponentOutOfRange`] if the
    /// normalized exponent leaves the supported range.
    pub fn from_digits(negative: bool, digits: &[u8], exponent: i16) -> Result<Self, NumberError> {
        if digits.iter().any(|d| *d > 9) {
            return Err(NumberError::InvalidDigit);
        }
        let leading = digits.iter().take_while(|d| **d == 0).count();
        let significant = &digits[leading..];
        let trailing = significant.iter().rev().take_while(|d| **d == 0).count();
        let significant = &significant[..significant.len() - trailing];
        if significant.is_empty() {
            return Ok(Self::ZERO);
        }
        if significant.len() > MAX_SIGNIFICANT_DIGITS {
            return Err(NumberError::TooManyDigits);
        }
        let exponent = i32::from(exponent) - i32::try_from(leading).unwrap_or(i32::MAX);
        let exponent = Self::check_exponent(exponent)?;

        let mut buf = [0_u8; MAX_SIGNIFICANT_DIGITS];
        buf[..significant.len()].copy_from_slice(significant);
        Ok(Self {
            digits: buf,
            len: u8::try_from(significant.len()).unwrap_or(0),
            exponent,
            negative,
        })
    }

    /// Parses a decimal literal.
    ///
    /// Accepts an optional sign, digits with an optional decimal point, and an
    /// optional `e`/`E` exponent; surrounding ASCII whitespace is ignored.
    ///
    /// # Errors
    ///
    /// See [`NumberError`]. In particular, a literal with more than 38
    /// significant digits is rejected rather than rounded.
    pub fn parse(text: &str) -> Result<Self, NumberError> {
        let bytes = text.trim().as_bytes();
        if bytes.is_empty() {
            return Err(NumberError::Empty);
        }

        let mut i = 0_usize;
        let negative = match bytes[0] {
            b'-' => {
                i = 1;
                true
            }
            b'+' => {
                i = 1;
                false
            }
            _ => false,
        };

        let int_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let int_end = i;

        let (frac_start, frac_end) = if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            (start, i)
        } else {
            (i, i)
        };

        if int_start == int_end && frac_start == frac_end {
            return Err(NumberError::InvalidSyntax);
        }

        let mut exponent_part = 0_i32;
        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
            i += 1;
            let negative_exponent = match bytes.get(i) {
                Some(b'-') => {
                    i += 1;
                    true
                }
                Some(b'+') => {
                    i += 1;
                    false
                }
                _ => false,
            };
            let digits_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // A six-digit cap keeps the accumulation inside `i32` and is far
            // wider than the representable exponent range.
            if i == digits_start || i - digits_start > 6 {
                return Err(NumberError::InvalidSyntax);
            }
            for byte in &bytes[digits_start..i] {
                exponent_part = exponent_part * 10 + i32::from(byte - b'0');
            }
            if negative_exponent {
                exponent_part = -exponent_part;
            }
        }

        if i != bytes.len() {
            return Err(NumberError::InvalidSyntax);
        }

        let mut buf = [0_u8; MAX_SIGNIFICANT_DIGITS];
        let mut len = 0_usize;
        let mut leading_zeros = 0_i32;
        let mut started = false;
        for byte in bytes[int_start..int_end]
            .iter()
            .chain(bytes[frac_start..frac_end].iter())
        {
            let digit = byte - b'0';
            if !started {
                if digit == 0 {
                    leading_zeros += 1;
                    continue;
                }
                started = true;
            }
            if len < MAX_SIGNIFICANT_DIGITS {
                buf[len] = digit;
                len += 1;
            } else if digit != 0 {
                return Err(NumberError::TooManyDigits);
            }
        }
        while len > 0 && buf[len - 1] == 0 {
            len -= 1;
        }
        if len == 0 {
            return Ok(Self::ZERO);
        }

        let int_digits = i32::try_from(int_end - int_start).unwrap_or(i32::MAX);
        let exponent = Self::check_exponent(int_digits - leading_zeros + exponent_part)?;
        Ok(Self {
            digits: buf,
            len: u8::try_from(len).unwrap_or(0),
            exponent,
            negative,
        })
    }

    /// Whether the value is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.len == 0
    }

    /// Whether the value is strictly negative.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.negative
    }

    /// Number of significant decimal digits (`0` for zero).
    #[must_use]
    pub const fn digit_count(self) -> usize {
        self.len as usize
    }

    /// The significant digits, most significant first, as values `0..=9`.
    #[must_use]
    pub fn digits(&self) -> &[u8] {
        &self.digits[..self.len as usize]
    }

    /// The decimal exponent in the normalized form `±0.d₁…dₙ × 10^exponent`.
    #[must_use]
    pub const fn exponent(self) -> i16 {
        self.exponent
    }

    /// Exact conversion to `i128`, or `None` if the value is not an integer or
    /// does not fit.
    #[must_use]
    pub fn to_i128(self) -> Option<i128> {
        if self.len == 0 {
            return Some(0);
        }
        let digit_count = i32::from(self.len);
        let exponent = i32::from(self.exponent);
        if exponent < digit_count {
            return None;
        }
        let mut value: i128 = 0;
        for digit in self.digits() {
            value = value.checked_mul(10)?.checked_add(i128::from(*digit))?;
        }
        for _ in 0..(exponent - digit_count) {
            value = value.checked_mul(10)?;
        }
        if self.negative {
            Some(-value)
        } else {
            Some(value)
        }
    }

    /// Exact conversion to `i64`, or `None` if the value is not an integer or
    /// does not fit.
    #[must_use]
    pub fn to_i64(self) -> Option<i64> {
        self.to_i128().and_then(|value| i64::try_from(value).ok())
    }

    /// Lossy conversion to `f64`, for charting and other display-only uses.
    ///
    /// Never call this on a path where the exact value matters; `NUMBER` values
    /// with more than 15–17 significant digits cannot survive the round trip.
    #[must_use]
    pub fn to_f64_lossy(self) -> f64 {
        self.to_string().parse::<f64>().unwrap_or(f64::NAN)
    }

    fn check_exponent(exponent: i32) -> Result<i16, NumberError> {
        if !(i32::from(MIN_EXPONENT)..=i32::from(MAX_EXPONENT)).contains(&exponent) {
            return Err(NumberError::ExponentOutOfRange);
        }
        i16::try_from(exponent).map_err(|_| NumberError::ExponentOutOfRange)
    }

    fn from_magnitude(magnitude: u128, negative: bool) -> Result<Self, NumberError> {
        if magnitude == 0 {
            return Ok(Self::ZERO);
        }
        // Least significant digit first; u128::MAX has 39 digits.
        let mut reversed = [0_u8; 39];
        let mut count = 0_usize;
        let mut rest = magnitude;
        while rest > 0 {
            reversed[count] = u8::try_from(rest % 10).unwrap_or(0);
            rest /= 10;
            count += 1;
        }
        let trailing_zeros = reversed[..count].iter().take_while(|d| **d == 0).count();
        let significant = count - trailing_zeros;
        if significant > MAX_SIGNIFICANT_DIGITS {
            return Err(NumberError::TooManyDigits);
        }
        let mut digits = [0_u8; MAX_SIGNIFICANT_DIGITS];
        for (target, source) in digits
            .iter_mut()
            .zip(reversed[trailing_zeros..count].iter().rev())
        {
            *target = *source;
        }
        Ok(Self {
            digits,
            len: u8::try_from(significant).unwrap_or(0),
            exponent: i16::try_from(count).unwrap_or(MAX_EXPONENT),
            negative,
        })
    }
}

impl fmt::Display for Number {
    /// Writes the one canonical form: plain positional decimal, never
    /// scientific, with no separators. See the module documentation for why this
    /// carries no formatting policy.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.len == 0 {
            return f.write_str("0");
        }
        let digits = self.digits();
        let digit_count = i32::try_from(digits.len()).unwrap_or(i32::MAX);
        let exponent = i32::from(self.exponent);

        if self.negative {
            f.write_char('-')?;
        }

        if exponent >= digit_count {
            // An integer, possibly with trailing zeros: 123 x 10^2 -> "12300".
            for digit in digits {
                f.write_char(char::from(b'0' + *digit))?;
            }
            for _ in 0..(exponent - digit_count) {
                f.write_char('0')?;
            }
            return Ok(());
        }

        if exponent > 0 {
            // The decimal point falls inside the digit run: "123.45".
            let split = usize::try_from(exponent).unwrap_or(0);
            for digit in &digits[..split] {
                f.write_char(char::from(b'0' + *digit))?;
            }
            f.write_char('.')?;
            for digit in &digits[split..] {
                f.write_char(char::from(b'0' + *digit))?;
            }
            return Ok(());
        }

        // Magnitude below one: "0." then the leading zeros the exponent implies.
        f.write_str("0.")?;
        for _ in 0..(-exponent) {
            f.write_char('0')?;
        }
        for digit in digits {
            f.write_char(char::from(b'0' + *digit))?;
        }
        Ok(())
    }
}

impl fmt::Debug for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Number({self})")
    }
}

impl FromStr for Number {
    type Err = NumberError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (negative, _) => {
                let magnitude = match (self.len, other.len) {
                    (0, 0) => Ordering::Equal,
                    (0, _) => Ordering::Less,
                    (_, 0) => Ordering::Greater,
                    _ => self
                        .exponent
                        .cmp(&other.exponent)
                        .then_with(|| self.digits().cmp(other.digits())),
                };
                if negative {
                    magnitude.reverse()
                } else {
                    magnitude
                }
            }
        }
    }
}

macro_rules! from_signed {
    ($($t:ty),*) => {$(
        impl From<$t> for Number {
            fn from(value: $t) -> Self {
                let magnitude = i128::from(value).unsigned_abs();
                // Every value of these types fits in 38 significant digits.
                Self::from_magnitude(magnitude, value < 0).unwrap_or(Self::ZERO)
            }
        }
    )*};
}

macro_rules! from_unsigned {
    ($($t:ty),*) => {$(
        impl From<$t> for Number {
            fn from(value: $t) -> Self {
                Self::from_magnitude(u128::from(value), false).unwrap_or(Self::ZERO)
            }
        }
    )*};
}

from_signed!(i8, i16, i32, i64);
from_unsigned!(u8, u16, u32, u64);

impl TryFrom<i128> for Number {
    type Error = NumberError;

    fn try_from(value: i128) -> Result<Self, Self::Error> {
        Self::from_magnitude(value.unsigned_abs(), value < 0)
    }
}

impl TryFrom<u128> for Number {
    type Error = NumberError;

    fn try_from(value: u128) -> Result<Self, Self::Error> {
        Self::from_magnitude(value, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str) -> String {
        Number::parse(text)
            .unwrap_or_else(|error| panic!("{text} should parse: {error}"))
            .to_string()
    }

    #[test]
    fn parses_and_renders_plain_decimals() {
        assert_eq!(round_trip("0"), "0");
        assert_eq!(round_trip("-0"), "0");
        assert_eq!(round_trip("0.0"), "0");
        assert_eq!(round_trip("1"), "1");
        assert_eq!(round_trip("-1"), "-1");
        assert_eq!(round_trip("123.45"), "123.45");
        assert_eq!(round_trip("123.450"), "123.45");
        assert_eq!(round_trip("000123.45"), "123.45");
        assert_eq!(round_trip("0.00123"), "0.00123");
        assert_eq!(round_trip("1500"), "1500");
        assert_eq!(round_trip("  42  "), "42");
    }

    #[test]
    fn parses_the_literal_shapes_drivers_actually_emit() {
        // A driver that has to render its native decimal through `Display` and
        // re-parse it here (ADR-0002, notes for driver implementers) produces
        // exactly these shapes. None of them may be rejected.
        assert_eq!(round_trip(".5"), "0.5");
        assert_eq!(round_trip("-.5"), "-0.5");
        assert_eq!(round_trip("+.5"), "0.5");
        assert_eq!(round_trip("1E+2"), "100");
        assert_eq!(round_trip("1e+2"), "100");
        assert_eq!(round_trip("1.5E-130"), format!("0.{}15", "0".repeat(129)));
        assert_eq!(round_trip(".0"), "0");
        assert_eq!(round_trip("-0.0"), "0");
    }

    #[test]
    fn parses_exponent_forms() {
        assert_eq!(round_trip("1.5e3"), "1500");
        assert_eq!(round_trip("1.5E+3"), "1500");
        assert_eq!(round_trip("15e-1"), "1.5");
        assert_eq!(round_trip("1e-130"), format!("0.{}1", "0".repeat(129)));
        assert_eq!(round_trip("1.23e100"), format!("123{}", "0".repeat(98)));
        assert_eq!(round_trip("0e99"), "0");
    }

    #[test]
    fn display_is_canonical_plain_decimal_with_no_policy() {
        // One form, always. No threshold decides when a value "becomes"
        // scientific, because that decision is the UI's (`SPEC.md` §13).
        for text in ["1e-130", "1e60", "1.23e100", "9.99e125", "1e-40"] {
            let rendered = Number::parse(text).expect("valid").to_string();
            assert!(
                !rendered.contains('E') && !rendered.contains('e'),
                "{text} rendered as {rendered}, which is not the canonical form"
            );
        }
    }

    #[test]
    fn display_round_trips_through_parse() {
        for text in [
            "0",
            "1",
            "-1",
            "123.45",
            "-0.00123",
            "1500",
            "1E-130",
            "1.5E-130",
            "1.23E+100",
            "9.99E+125",
            "9999999999999999999999999999999999999999",
            "-0.9999999999999999999999999999999999999999",
        ] {
            let parsed = Number::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            let rendered = parsed.to_string();
            let reparsed = Number::parse(&rendered).unwrap_or_else(|e| panic!("{rendered}: {e}"));
            assert_eq!(
                parsed, reparsed,
                "round trip broke for {text} -> {rendered}"
            );
        }
    }

    #[test]
    fn keeps_all_forty_digits_a_server_can_produce() {
        // 38 is the declarable precision; 40 is what the 20-byte base-100
        // mantissa can carry, and what division results actually reach. A value
        // the server sends must never fail to be represented.
        assert_eq!(MAX_SIGNIFICANT_DIGITS, 40);
        // Ends in a non-zero digit: a trailing zero is not significant, so it
        // would not exercise the limit.
        let text = "1234567890123456789012345678901234567891";
        assert_eq!(text.len(), MAX_SIGNIFICANT_DIGITS);
        let parsed = Number::parse(text).expect("40 digits must be accepted");
        assert_eq!(parsed.digit_count(), MAX_SIGNIFICANT_DIGITS);
        assert_eq!(parsed.to_string(), text);

        // The shape a division produces: 40 significant digits after the point.
        let quotient = format!("0.{}", "3".repeat(40));
        let parsed = Number::parse(&quotient).expect("40 fractional digits must be accepted");
        assert_eq!(parsed.digit_count(), 40);
        assert_eq!(parsed.to_string(), quotient);
    }

    #[test]
    fn rejects_more_than_forty_significant_digits() {
        let text = "1".repeat(MAX_SIGNIFICANT_DIGITS + 1);
        assert_eq!(Number::parse(&text), Err(NumberError::TooManyDigits));
        // Trailing zeros past the limit are not significant, so they are fine as
        // long as the exponent still fits.
        let padded = format!("1{}", "0".repeat(60));
        let parsed = Number::parse(&padded).expect("1e60 has one significant digit");
        assert_eq!(parsed.digit_count(), 1);
        assert_eq!(parsed.to_string(), padded);
        // Same shape, but now out of the NUMBER exponent range.
        assert_eq!(
            Number::parse(&format!("1{}", "0".repeat(130))),
            Err(NumberError::ExponentOutOfRange)
        );
    }

    #[test]
    fn rejects_out_of_range_exponents() {
        assert_eq!(
            Number::parse("1e126"),
            Err(NumberError::ExponentOutOfRange),
            "1e126 is outside the NUMBER range"
        );
        assert!(Number::parse("9.99e125").is_ok());
        assert_eq!(
            Number::parse("1e-131"),
            Err(NumberError::ExponentOutOfRange)
        );
        assert!(Number::parse("1e-130").is_ok());
    }

    #[test]
    fn rejects_garbage() {
        for text in [
            "",
            "   ",
            "abc",
            ".",
            "-",
            "+",
            "1.2.3",
            "1e",
            "1e+",
            "1 2",
            "1,5",
            "0x10",
            "1e1e1",
            "--1",
            "1-",
            "๑๒๓",
        ] {
            assert!(
                Number::parse(text).is_err(),
                "{text:?} should have been rejected"
            );
        }
    }

    #[test]
    fn integer_conversions_are_exact() {
        assert_eq!(Number::from(0_i64).to_string(), "0");
        assert_eq!(Number::from(i64::MIN).to_i64(), Some(i64::MIN));
        assert_eq!(Number::from(u64::MAX).to_i128(), Some(i128::from(u64::MAX)));
        assert_eq!(Number::from(-1500_i64).to_string(), "-1500");
        assert_eq!(Number::parse("123.45").expect("valid").to_i64(), None);
        assert_eq!(Number::parse("1500").expect("valid").to_i64(), Some(1500));
        assert_eq!(
            Number::parse("1e30").expect("valid").to_i128(),
            Some(10_i128.pow(30))
        );
    }

    #[test]
    fn from_digits_normalizes() {
        let n = Number::from_digits(false, &[0, 1, 2, 0, 0], 3).expect("valid");
        // 0.01200 x 10^3 == 0.12 x 10^2 == 12
        assert_eq!(n.to_string(), "12");
        assert_eq!(n.digits(), &[1, 2]);
        assert_eq!(n.exponent(), 2);

        assert_eq!(
            Number::from_digits(false, &[10], 0),
            Err(NumberError::InvalidDigit)
        );
        assert_eq!(Number::from_digits(true, &[0, 0], 5), Ok(Number::ZERO));
        assert!(
            Number::from_digits(false, &[1; MAX_SIGNIFICANT_DIGITS], 0).is_ok(),
            "a full 40-digit mantissa from a wire decoder must be accepted"
        );
        assert_eq!(
            Number::from_digits(false, &[1; MAX_SIGNIFICANT_DIGITS + 1], 0),
            Err(NumberError::TooManyDigits)
        );
    }

    #[test]
    fn ordering_follows_numeric_value() {
        let mut values = [
            Number::parse("1").expect("valid"),
            Number::parse("-1").expect("valid"),
            Number::ZERO,
            Number::parse("0.5").expect("valid"),
            Number::parse("-0.5").expect("valid"),
            Number::parse("1000").expect("valid"),
            Number::parse("-1000").expect("valid"),
        ];
        values.sort_unstable();
        let rendered: Vec<String> = values.iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered,
            vec!["-1000", "-1", "-0.5", "0", "0.5", "1", "1000"]
        );
    }

    #[test]
    fn zero_is_canonical() {
        assert_eq!(Number::parse("-0.000").expect("valid"), Number::ZERO);
        assert_eq!(Number::parse("0e5").expect("valid"), Number::ZERO);
        assert!(!Number::ZERO.is_negative());
        assert!(Number::ZERO.is_zero());
        assert_eq!(Number::ZERO.digits(), &[] as &[u8]);
    }

    #[test]
    fn the_only_f64_conversion_is_the_explicitly_lossy_one_out() {
        // There is no `from_f64`: nothing in the fetch path has an `f64` to
        // start from, and offering one would invite silent precision loss into
        // a contract whose whole point is not having any.
        let exact = Number::parse("123456789012345678901234567890").expect("valid");
        assert!((exact.to_f64_lossy() - 1.234_567_890_123_456_8e29).abs() < 1e14);
        assert_eq!(Number::ZERO.to_f64_lossy(), 0.0);
        assert_eq!(Number::parse("1.5").expect("valid").to_f64_lossy(), 1.5);
    }

    #[test]
    fn stays_allocation_free_and_compact() {
        // 40 digits + length + exponent + sign. Widening from 38 to 40 digits
        // cost two bytes; `Value`'s size is asserted separately.
        assert!(
            size_of::<Number>() <= 48,
            "Number grew to {} bytes",
            size_of::<Number>()
        );
    }
}
