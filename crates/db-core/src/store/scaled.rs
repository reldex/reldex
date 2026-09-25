//! `NUMBER` as a scaled 64-bit integer, and a cell that is either that or the
//! lossless [`Number`] it came from (ADR-0004 RS1).
//!
//! A segment column whose every non-NULL value is exactly `m × 10⁻ˢ`, with
//! `s ≤ 18` and `m` in `i64`, is kept as one `i64` per row plus one scale for
//! the column: 8 bytes a cell instead of 44. The encoding is chosen per
//! segment column, so the same value can be `15 × 10⁻¹` in one segment and
//! `150 × 10⁻²` in the next. **Nothing a reader sees may depend on that**:
//! [`ScaledNumber`]'s `Display` is byte-identical to [`Number`]'s canonical
//! `Display` for the value it holds, whatever the scale, and
//! [`ScaledNumber::to_number`] gives back exactly the `Number` that was
//! encoded.

use std::fmt;
use std::fmt::Write as _;

use reldex_db_driver_api::Number;

/// A decimal value held as `mantissa × 10^-scale`, exactly.
///
/// Built only from a [`Number`] that it represents exactly
/// ([`ScaledNumber::from_number`]) or from its parts
/// ([`ScaledNumber::new`]); `Display` and [`ScaledNumber::to_number`] never
/// round. Equality compares the representation, so `15 × 10⁻¹` and
/// `150 × 10⁻²` are *different* `ScaledNumber`s that display the same text
/// and convert to the same `Number` — compare [`ScaledNumber::to_number`]
/// results to compare values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScaledNumber {
    mantissa: i64,
    scale: u8,
}

impl ScaledNumber {
    /// The largest scale a scaled column uses. 10¹⁸ is the largest power of
    /// ten an `i64` holds, so every scale up to it can represent at least
    /// the value one.
    pub const MAX_SCALE: u8 = 18;

    /// `mantissa × 10^-scale`, or `None` when `scale` exceeds
    /// [`ScaledNumber::MAX_SCALE`].
    #[must_use]
    pub const fn new(mantissa: i64, scale: u8) -> Option<Self> {
        if scale > Self::MAX_SCALE {
            return None;
        }
        Some(Self { mantissa, scale })
    }

    /// `value` at `scale`, if that is exact: `None` when `value` has more
    /// fraction digits than `scale`, when the mantissa does not fit in
    /// `i64`, or when `scale` exceeds [`ScaledNumber::MAX_SCALE`].
    #[must_use]
    pub fn from_number(value: &Number, scale: u8) -> Option<Self> {
        if scale > Self::MAX_SCALE {
            return None;
        }
        if value.is_zero() {
            return Some(Self { mantissa: 0, scale });
        }
        let digits = value.digits();
        // value = ±0.d₁…dₙ × 10^e = ±(d₁…dₙ) × 10^(e − n); at scale s the
        // mantissa is (d₁…dₙ) × 10^(e − n + s).
        let count = i32::try_from(digits.len()).ok()?;
        let shift = i32::from(value.exponent()) - count + i32::from(scale);
        if shift < 0 {
            return None;
        }
        // The mantissa has `e + s` digits; more than 19 never fits an i64.
        if count + shift > 19 {
            return None;
        }
        let mut magnitude: i128 = 0;
        for digit in digits {
            magnitude = magnitude * 10 + i128::from(*digit);
        }
        for _ in 0..shift {
            magnitude *= 10;
        }
        let signed = if value.is_negative() {
            -magnitude
        } else {
            magnitude
        };
        Some(Self {
            mantissa: i64::try_from(signed).ok()?,
            scale,
        })
    }

    /// The smallest scale at which `value` is exact, or `None` when it needs
    /// more than [`ScaledNumber::MAX_SCALE`] fraction digits. Zero needs
    /// scale 0; so does any integer.
    #[must_use]
    pub fn required_scale(value: &Number) -> Option<u8> {
        if value.is_zero() {
            return Some(0);
        }
        let count = i32::try_from(value.digit_count()).ok()?;
        let fraction_digits = (count - i32::from(value.exponent())).max(0);
        u8::try_from(fraction_digits)
            .ok()
            .filter(|scale| *scale <= Self::MAX_SCALE)
    }

    /// The mantissa.
    #[must_use]
    pub const fn mantissa(self) -> i64 {
        self.mantissa
    }

    /// The scale: how many decimal digits of the mantissa are fraction.
    #[must_use]
    pub const fn scale(self) -> u8 {
        self.scale
    }

    /// The exact [`Number`] this holds.
    #[must_use]
    pub fn to_number(self) -> Number {
        let (digits, len) = self.magnitude_digits();
        let scale = i16::from(self.scale);
        let exponent = i16::try_from(len).unwrap_or(i16::MAX) - scale;
        // Unreachable failure: at most 20 digits and an exponent within
        // −18..=20 are always representable.
        Number::from_digits(self.mantissa < 0, &digits[..len], exponent).unwrap_or(Number::ZERO)
    }

    /// The decimal digits of `|mantissa|`, most significant first, and how
    /// many there are. `u64` holds `|i64::MIN|`, which has 19 digits.
    fn magnitude_digits(self) -> ([u8; 20], usize) {
        let mut digits = [0_u8; 20];
        let mut rest = self.mantissa.unsigned_abs();
        let mut len = 0_usize;
        while rest > 0 {
            digits[len] = u8::try_from(rest % 10).unwrap_or(0);
            rest /= 10;
            len += 1;
        }
        digits[..len].reverse();
        (digits, len)
    }
}

impl fmt::Display for ScaledNumber {
    /// Writes exactly what [`Number`]'s `Display` writes for the same value:
    /// plain positional decimal, no exponent, no separators, no trailing
    /// fraction zeros, `0.` before a value below one and `-` before a
    /// negative one. Allocation-free.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mantissa == 0 {
            return f.write_str("0");
        }
        let (digits, len) = self.magnitude_digits();
        // Trailing fraction zeros are not part of the canonical form:
        // 150 × 10⁻² is "1.5", as `Number` normalizes it.
        let mut scale = usize::from(self.scale);
        let mut len = len;
        while scale > 0 && digits[len - 1] == 0 {
            len -= 1;
            scale -= 1;
        }
        let digits = &digits[..len];
        if self.mantissa < 0 {
            f.write_char('-')?;
        }
        let ascii = |digit: &u8| char::from(b'0' + *digit);
        if scale == 0 {
            for digit in digits {
                f.write_char(ascii(digit))?;
            }
            return Ok(());
        }
        if len > scale {
            let split = len - scale;
            for digit in &digits[..split] {
                f.write_char(ascii(digit))?;
            }
            f.write_char('.')?;
            for digit in &digits[split..] {
                f.write_char(ascii(digit))?;
            }
            return Ok(());
        }
        f.write_str("0.")?;
        for _ in 0..(scale - len) {
            f.write_char('0')?;
        }
        for digit in digits {
            f.write_char(ascii(digit))?;
        }
        Ok(())
    }
}

/// One `NUMBER` cell as the store holds it: scaled, or the lossless
/// [`Number`] a column falls back to when a value does not scale.
///
/// Both forms display byte-identically to [`Number`]'s canonical `Display`
/// and convert to the same `Number`; which one a cell is says only how its
/// segment column is stored. `#[non_exhaustive]`: a later encoding (packed
/// BCD for the fallback, ADR-0004 "Alternatives") is an addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NumberValue<'a> {
    /// Held as a scaled `i64`.
    Scaled(ScaledNumber),
    /// Held as the driver delivered it.
    Decimal(&'a Number),
}

impl NumberValue<'_> {
    /// The exact value.
    #[must_use]
    pub fn to_number(self) -> Number {
        match self {
            Self::Scaled(value) => value.to_number(),
            Self::Decimal(value) => *value,
        }
    }
}

impl fmt::Display for NumberValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scaled(value) => fmt::Display::fmt(value, f),
            Self::Decimal(value) => fmt::Display::fmt(value, f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NumberValue, ScaledNumber};
    use reldex_db_driver_api::Number;

    fn number(text: &str) -> Number {
        text.parse().expect("valid decimal")
    }

    /// A small deterministic generator, so the property test needs no
    /// dependency and every failure reproduces.
    struct Xorshift(u64);

    impl Xorshift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// A random `Number` of up to 19 significant digits with up to 18
    /// fraction digits — the domain a scaled column can hold — or, one time
    /// in eight, one that cannot be scaled at all.
    fn random_number(rng: &mut Xorshift) -> Number {
        let digits = 1 + rng.below(19);
        let mut text = String::new();
        if rng.below(2) == 0 {
            text.push('-');
        }
        for index in 0..digits {
            let digit = if index == 0 {
                1 + rng.below(9)
            } else {
                rng.below(10)
            };
            text.push(char::from(b'0' + u8::try_from(digit).expect("digit")));
        }
        let fraction = if rng.below(8) == 0 {
            // Outside what scales: 19–40 fraction digits.
            19 + rng.below(22)
        } else {
            rng.below(19)
        };
        let fraction = usize::try_from(fraction).expect("small");
        let exponent = if fraction >= text.trim_start_matches('-').len() {
            format!("E-{fraction}")
        } else {
            let split = text.len() - fraction;
            text.insert(split, '.');
            String::new()
        };
        number(&format!("{text}{exponent}"))
    }

    #[test]
    fn display_is_byte_identical_to_the_canonical_number_form() {
        for (text, scale) in [
            ("0", 0),
            ("0", 18),
            ("1.5", 1),
            ("1.5", 2),
            ("1.5", 18),
            ("1500", 0),
            ("1500", 3),
            ("-0.005", 3),
            ("-0.005", 9),
            ("123.45", 2),
            ("0.000000000000000001", 18),
            ("9223372036854775807", 0),
            ("-9223372036854775808", 0),
            ("-9.223372036854775808", 18),
            ("922337203.6854775807", 10),
        ] {
            let original = number(text);
            let scaled = ScaledNumber::from_number(&original, scale)
                .unwrap_or_else(|| panic!("{text} scales at {scale}"));
            assert_eq!(scaled.to_string(), original.to_string(), "{text} @ {scale}");
            assert_eq!(scaled.to_number(), original, "{text} @ {scale}");
        }
    }

    #[test]
    fn one_value_at_two_scales_reads_the_same() {
        // The case the ADR names: a column scaled 1 in one segment and 2 in
        // the next must not show "1.5" and then "1.50".
        let value = number("1.5");
        let one = ScaledNumber::from_number(&value, 1).expect("scale 1");
        let two = ScaledNumber::from_number(&value, 2).expect("scale 2");
        assert_ne!(one, two, "the representations differ");
        assert_eq!(one.mantissa(), 15);
        assert_eq!(two.mantissa(), 150);
        assert_eq!(one.to_string(), "1.5");
        assert_eq!(two.to_string(), "1.5");
        assert_eq!(one.to_number(), two.to_number());
        assert_eq!(
            NumberValue::Scaled(two).to_string(),
            NumberValue::Decimal(&value).to_string()
        );
    }

    #[test]
    fn values_that_do_not_scale_are_refused_never_rounded() {
        assert_eq!(ScaledNumber::from_number(&number("1.25"), 1), None);
        assert_eq!(
            ScaledNumber::from_number(&number("9223372036854775808"), 0),
            None,
            "one past i64::MAX"
        );
        assert_eq!(ScaledNumber::from_number(&number("1"), 19), None);
        assert_eq!(ScaledNumber::from_number(&number("1E+30"), 0), None);
        let quotient = number("0.1428571428571428571428571428571428571429");
        assert_eq!(ScaledNumber::required_scale(&quotient), None);
        assert_eq!(ScaledNumber::required_scale(&number("1.25")), Some(2));
        assert_eq!(ScaledNumber::required_scale(&number("1E+30")), Some(0));
        assert_eq!(ScaledNumber::required_scale(&Number::ZERO), Some(0));
        assert_eq!(ScaledNumber::new(1, 19), None);
        assert_eq!(ScaledNumber::new(-7, 18).map(ScaledNumber::scale), Some(18));
    }

    /// The property ADR-0004 asks M5.2 for: over random `Number`s and every
    /// scale 0–18, a value that scales is re-encoded losslessly and displays
    /// byte-identically, whichever scale its segment happened to choose; one
    /// that does not scale is refused.
    #[test]
    fn random_numbers_round_trip_and_display_identically_at_every_scale() {
        let mut rng = Xorshift(0x5EED_1234_ABCD_0001);
        let mut scaled_cells = 0_u32;
        for _ in 0..40_000 {
            let original = random_number(&mut rng);
            let canonical = original.to_string();
            let required = ScaledNumber::required_scale(&original);
            for scale in 0..=ScaledNumber::MAX_SCALE {
                match ScaledNumber::from_number(&original, scale) {
                    Some(scaled) => {
                        scaled_cells += 1;
                        assert!(required.is_some_and(|needed| needed <= scale));
                        assert_eq!(scaled.to_number(), original, "{canonical} @ {scale}");
                        assert_eq!(scaled.to_string(), canonical, "{canonical} @ {scale}");
                    }
                    None => {
                        // Refused only for a reason: too many fraction
                        // digits for this scale, or a mantissa outside i64.
                        if required.is_some_and(|needed| needed <= scale) {
                            let mantissa = number(&format!("{canonical}E{scale}")).to_i128();
                            assert!(
                                mantissa.is_none_or(|m| i64::try_from(m).is_err()),
                                "{canonical} @ {scale} was refused but fits"
                            );
                        }
                    }
                }
            }
        }
        assert!(scaled_cells > 100_000, "the generator must mostly scale");
    }
}
