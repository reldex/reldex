//! Date and time representation without a calendar dependency (ADR-0002 D5).
//!
//! One [`Timestamp`] type covers `DATE`, `TIMESTAMP` and
//! `TIMESTAMP WITH TIME ZONE`; which of them a column actually is comes from the
//! declared [`crate::types::SqlType`], not from the value.
//!
//! # Which calendar
//!
//! Validation follows **the calendar rules of the source database**, which for
//! the SQL servers this contract targets means the historical (mixed
//! Julian/Gregorian) calendar, not the proleptic Gregorian calendar that most
//! date libraries implement:
//!
//! - Dates before the Gregorian reform of 15 October 1582 use the **Julian**
//!   leap rule — every fourth year, with no century exception. `1500-02-29` is a
//!   real, storable date on such a server even though proleptic Gregorian says
//!   1500 was not a leap year.
//! - There is **no year zero**: year `-1` is 1 BC, `-2` is 2 BC. Leap years
//!   before the common era therefore fall on `-1`, `-5`, `-9` …, which is offset
//!   by one from naive "divisible by four" arithmetic on the negative number.
//! - The ten days `1582-10-05` … `1582-10-14` were skipped by the reform and do
//!   not exist. See [`Timestamp::new`] and [`Timestamp::from_source`] for the
//!   deliberately different treatment on the two paths.
//!
//! Known gap (ADR-0002 D5): a named time-zone region is normalized to a fixed
//! offset. Preserving region names would mean either a heap-allocated name on
//! every timestamp or a time-zone database in this crate; both are deferred.

use std::fmt;

/// Why a temporal value was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemporalError {
    /// The year was outside `-4712..=9999`, or was zero.
    YearOutOfRange,
    /// The month was outside `1..=12`.
    MonthOutOfRange,
    /// The day was outside the valid range for that month and year.
    DayOutOfRange,
    /// The date falls in the ten days the Gregorian reform skipped
    /// (`1582-10-05` … `1582-10-14`), which never existed.
    ///
    /// Only strict construction reports this; the decode path accepts such a
    /// date rather than failing a whole fetch (see [`Timestamp::from_source`]).
    NonexistentCalendarDate,
    /// Hour, minute or second was outside its valid range.
    TimeOutOfRange,
    /// The nanosecond component was `1_000_000_000` or greater.
    NanosecondOutOfRange,
    /// The UTC offset was outside `-1439..=1439` minutes.
    OffsetOutOfRange,
}

impl fmt::Display for TemporalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::YearOutOfRange => "year outside -4712..=9999 (and non-zero; there is no year 0)",
            Self::MonthOutOfRange => "month outside 1..=12",
            Self::DayOutOfRange => "day outside the valid range for that month",
            Self::NonexistentCalendarDate => {
                "date falls in the ten days skipped by the Gregorian reform (1582-10-05..1582-10-14)"
            }
            Self::TimeOutOfRange => "hour, minute or second out of range",
            Self::NanosecondOutOfRange => "nanosecond outside 0..1_000_000_000",
            Self::OffsetOutOfRange => "UTC offset outside -1439..=1439 minutes",
        };
        f.write_str(text)
    }
}

impl std::error::Error for TemporalError {}

/// Time-zone information attached to a [`Timestamp`].
///
/// `#[non_exhaustive]`: a `Region` case for named IANA zones is a planned
/// addition (ADR-0002 D5, lead decision 2), and adding it must not be a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TimeZone {
    /// No zone information: a wall-clock value (`DATE`, `TIMESTAMP`).
    #[default]
    Unspecified,
    /// A fixed offset from UTC, in minutes.
    Offset {
        /// Minutes east of UTC, `-1439..=1439`.
        minutes: i16,
    },
}

impl TimeZone {
    /// Builds a fixed UTC offset.
    ///
    /// # Errors
    ///
    /// Returns [`TemporalError::OffsetOutOfRange`] outside `-1439..=1439`.
    pub fn offset(minutes: i16) -> Result<Self, TemporalError> {
        if !(-1439..=1439).contains(&minutes) {
            return Err(TemporalError::OffsetOutOfRange);
        }
        Ok(Self::Offset { minutes })
    }

    /// The offset in minutes east of UTC, if any.
    #[must_use]
    pub const fn offset_minutes(self) -> Option<i16> {
        match self {
            Self::Unspecified => None,
            Self::Offset { minutes } => Some(minutes),
        }
    }
}

impl fmt::Display for TimeZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unspecified => Ok(()),
            Self::Offset { minutes } => {
                let sign = if *minutes < 0 { '-' } else { '+' };
                let absolute = minutes.unsigned_abs();
                write!(f, "{sign}{:02}:{:02}", absolute / 60, absolute % 60)
            }
        }
    }
}

/// A validated calendar date and time with optional zone information.
///
/// Ordering is deliberately not implemented: comparing a zoned and an unzoned
/// value, or two values in different offsets, needs a policy that belongs in
/// `db-core`, not in the transport contract.
///
/// ```
/// use reldex_db_driver_api::{TimeZone, Timestamp};
///
/// let ts = Timestamp::new(2026, 9, 19, 13, 45, 30)
///     .expect("valid")
///     .with_nanosecond(123_456_000)
///     .expect("valid")
///     .with_zone(TimeZone::offset(7 * 60).expect("valid"));
/// assert_eq!(ts.to_string(), "2026-09-19T13:45:30.123456000+07:00");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timestamp {
    year: i16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    nanosecond: u32,
    zone: TimeZone,
}

impl Timestamp {
    /// Builds a date at midnight with no zone information, validating strictly.
    ///
    /// # Errors
    ///
    /// See [`Timestamp::new`].
    pub fn date(year: i16, month: u8, day: u8) -> Result<Self, TemporalError> {
        Self::new(year, month, day, 0, 0, 0)
    }

    /// Builds a date and time with no sub-second part and no zone information,
    /// validating strictly.
    ///
    /// This is the constructor for values Reldex or a user *invents* — a bind
    /// parameter, a literal typed into a filter. It rejects every date the
    /// source calendar says never existed, including the ten days the Gregorian
    /// reform skipped, because sending one to a server would only produce a
    /// server-side error later, with worse diagnostics.
    ///
    /// Drivers decoding a value the *server produced* use
    /// [`Timestamp::from_source`] instead.
    ///
    /// Leap years follow the calendar of the source database, so `1500-02-29`
    /// (Julian) is accepted and `1900-02-29` (Gregorian) is not.
    ///
    /// # Errors
    ///
    /// See [`TemporalError`]; in particular
    /// [`TemporalError::NonexistentCalendarDate`] for the reform gap.
    pub fn new(
        year: i16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<Self, TemporalError> {
        let timestamp = Self::from_source(year, month, day, hour, minute, second)?;
        if is_in_reform_gap(year, month, day) {
            return Err(TemporalError::NonexistentCalendarDate);
        }
        Ok(timestamp)
    }

    /// Builds a date and time from values a **server produced**, validating
    /// leniently.
    ///
    /// Ranges are still checked — a month of 13 or a 30th of February is a
    /// decoding bug and must not reach the core — but a date inside the ten days
    /// the Gregorian reform skipped is *accepted*. A server that stores such a
    /// value (or a wire decoder that reconstructs one) is reporting a fact, and
    /// `SPEC.md` §2's correctness ranking is not served by failing an entire
    /// fetch of a large table over one historical row. Reldex shows what the
    /// database holds; it does not audit the database's own history.
    ///
    /// Use [`Timestamp::new`] for values Reldex constructs itself.
    ///
    /// # Errors
    ///
    /// [`TemporalError::YearOutOfRange`], [`TemporalError::MonthOutOfRange`],
    /// [`TemporalError::DayOutOfRange`] or [`TemporalError::TimeOutOfRange`].
    /// Never [`TemporalError::NonexistentCalendarDate`].
    pub fn from_source(
        year: i16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<Self, TemporalError> {
        if !(-4712..=9999).contains(&year) || year == 0 {
            return Err(TemporalError::YearOutOfRange);
        }
        if !(1..=12).contains(&month) {
            return Err(TemporalError::MonthOutOfRange);
        }
        if day == 0 || day > days_in_month(year, month) {
            return Err(TemporalError::DayOutOfRange);
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Err(TemporalError::TimeOutOfRange);
        }
        Ok(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            nanosecond: 0,
            zone: TimeZone::Unspecified,
        })
    }

    /// Adds a sub-second component.
    ///
    /// # Errors
    ///
    /// Returns [`TemporalError::NanosecondOutOfRange`] for a value of
    /// `1_000_000_000` or more.
    pub const fn with_nanosecond(mut self, nanosecond: u32) -> Result<Self, TemporalError> {
        if nanosecond >= 1_000_000_000 {
            return Err(TemporalError::NanosecondOutOfRange);
        }
        self.nanosecond = nanosecond;
        Ok(self)
    }

    /// Attaches zone information.
    #[must_use]
    pub const fn with_zone(mut self, zone: TimeZone) -> Self {
        self.zone = zone;
        self
    }

    /// The year; negative values are BCE.
    #[must_use]
    pub const fn year(self) -> i16 {
        self.year
    }

    /// The month, `1..=12`.
    #[must_use]
    pub const fn month(self) -> u8 {
        self.month
    }

    /// The day of month, `1..=31`.
    #[must_use]
    pub const fn day(self) -> u8 {
        self.day
    }

    /// The hour, `0..=23`.
    #[must_use]
    pub const fn hour(self) -> u8 {
        self.hour
    }

    /// The minute, `0..=59`.
    #[must_use]
    pub const fn minute(self) -> u8 {
        self.minute
    }

    /// The second, `0..=59`.
    #[must_use]
    pub const fn second(self) -> u8 {
        self.second
    }

    /// The sub-second component, `0..1_000_000_000`.
    #[must_use]
    pub const fn nanosecond(self) -> u32 {
        self.nanosecond
    }

    /// The zone information, if any.
    #[must_use]
    pub const fn zone(self) -> TimeZone {
        self.zone
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.year < 0 {
            write!(f, "-{:04}", self.year.unsigned_abs())?;
        } else {
            write!(f, "{:04}", self.year)?;
        }
        write!(
            f,
            "-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.month, self.day, self.hour, self.minute, self.second
        )?;
        if self.nanosecond != 0 {
            write!(f, ".{:09}", self.nanosecond)?;
        }
        write!(f, "{}", self.zone)
    }
}

/// The year the Gregorian reform took effect; dates before it are Julian.
const GREGORIAN_REFORM_YEAR: i16 = 1582;

/// Days in `month` of `year`, under the calendar rules of the source database.
const fn days_in_month(year: i16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Whether `year` is a leap year in the historical (mixed) calendar.
///
/// Two corrections over the usual proleptic-Gregorian one-liner:
///
/// - Years before the 1582 reform use the **Julian** rule (every fourth year,
///   no century exception), so 1500 and 1300 are leap years.
/// - The era numbering has **no year zero**, so a negative `year` is shifted by
///   one before the divisibility test: `-1` is 1 BC, which was a leap year.
///
/// The reform year itself is treated as Gregorian, which is unambiguous:
/// February 1582 predates the October cut-over, but 1582 is not a leap year
/// under either rule, so the two readings agree.
const fn is_leap_year(year: i16) -> bool {
    // Astronomical numbering: 1 BC becomes 0, 2 BC becomes -1, and so on, so
    // that "divisible by four" means what it says on both sides of the era
    // boundary.
    let astronomical = if year < 0 { year + 1 } else { year };
    if astronomical % 4 != 0 {
        return false;
    }
    if year < GREGORIAN_REFORM_YEAR {
        return true;
    }
    astronomical % 100 != 0 || astronomical % 400 == 0
}

/// Whether the date falls in the ten days the Gregorian reform skipped.
///
/// 4 October 1582 (Julian) was followed directly by 15 October 1582
/// (Gregorian); the dates in between never occurred.
const fn is_in_reform_gap(year: i16, month: u8, day: u8) -> bool {
    year == GREGORIAN_REFORM_YEAR && month == 10 && day >= 5 && day <= 14
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_dates_and_times() {
        assert!(Timestamp::date(2026, 9, 19).is_ok());
        assert!(Timestamp::new(2026, 12, 31, 23, 59, 59).is_ok());
        assert!(Timestamp::date(2024, 2, 29).is_ok(), "2024 is a leap year");
        assert!(Timestamp::date(2000, 2, 29).is_ok(), "2000 is a leap year");
        assert!(Timestamp::date(-4712, 1, 1).is_ok());
    }

    #[test]
    fn julian_leap_years_before_the_reform_are_accepted() {
        // Proleptic Gregorian says 1500 and 1300 are not leap years, because
        // they are divisible by 100 but not 400. The source calendar is Julian
        // before 1582, so `DATE '1500-02-29'` is a real, storable value and
        // rejecting it would make Reldex unable to display existing rows.
        assert!(
            Timestamp::date(1500, 2, 29).is_ok(),
            "1500 is a Julian leap year"
        );
        assert!(
            Timestamp::date(1300, 2, 29).is_ok(),
            "1300 is a Julian leap year"
        );
        assert!(Timestamp::date(1200, 2, 29).is_ok());
        // Still not a leap year under either rule.
        assert_eq!(
            Timestamp::date(1501, 2, 29),
            Err(TemporalError::DayOutOfRange)
        );
        // After the reform the Gregorian century rule applies again.
        assert_eq!(
            Timestamp::date(1700, 2, 29),
            Err(TemporalError::DayOutOfRange)
        );
        assert!(
            Timestamp::date(1600, 2, 29).is_ok(),
            "1600 is divisible by 400"
        );
    }

    #[test]
    fn bc_leap_years_account_for_the_missing_year_zero() {
        // Year -1 is 1 BC, which was a leap year; naive `year % 4` would say no.
        assert!(Timestamp::date(-1, 2, 29).is_ok(), "1 BC is a leap year");
        assert!(Timestamp::date(-5, 2, 29).is_ok(), "5 BC is a leap year");
        assert!(Timestamp::date(-4709, 2, 29).is_ok());
        for year in [-2_i16, -3, -4] {
            assert_eq!(
                Timestamp::date(year, 2, 29),
                Err(TemporalError::DayOutOfRange),
                "{year} is not a leap year"
            );
        }
        // There is no year zero at all.
        assert_eq!(
            Timestamp::date(0, 2, 28),
            Err(TemporalError::YearOutOfRange)
        );
    }

    #[test]
    fn the_gregorian_reform_gap_is_strict_on_input_and_lenient_on_decode() {
        // The ten days never happened, so a value Reldex invents is refused...
        for day in 5..=14 {
            assert_eq!(
                Timestamp::date(1582, 10, day),
                Err(TemporalError::NonexistentCalendarDate),
                "1582-10-{day} never existed"
            );
        }
        // ...but a value the server produced is accepted rather than failing the
        // whole fetch.
        for day in 5..=14 {
            let decoded = Timestamp::from_source(1582, 10, day, 12, 0, 0)
                .unwrap_or_else(|error| panic!("decode of 1582-10-{day} must succeed: {error}"));
            assert_eq!(decoded.day(), day);
        }
        // The days on either side of the gap are ordinary.
        assert!(Timestamp::date(1582, 10, 4).is_ok());
        assert!(Timestamp::date(1582, 10, 15).is_ok());

        // Leniency is limited to the gap: a decoder bug is still a bug.
        assert_eq!(
            Timestamp::from_source(1582, 13, 1, 0, 0, 0),
            Err(TemporalError::MonthOutOfRange)
        );
        assert_eq!(
            Timestamp::from_source(1900, 2, 29, 0, 0, 0),
            Err(TemporalError::DayOutOfRange)
        );
        assert_eq!(
            Timestamp::from_source(2026, 1, 1, 24, 0, 0),
            Err(TemporalError::TimeOutOfRange)
        );
    }

    #[test]
    fn rejects_impossible_calendar_values() {
        assert_eq!(
            Timestamp::date(2023, 2, 29),
            Err(TemporalError::DayOutOfRange)
        );
        assert_eq!(
            Timestamp::date(1900, 2, 29),
            Err(TemporalError::DayOutOfRange)
        );
        assert_eq!(
            Timestamp::date(2026, 4, 31),
            Err(TemporalError::DayOutOfRange)
        );
        assert_eq!(
            Timestamp::date(2026, 0, 1),
            Err(TemporalError::MonthOutOfRange)
        );
        assert_eq!(
            Timestamp::date(2026, 13, 1),
            Err(TemporalError::MonthOutOfRange)
        );
        assert_eq!(Timestamp::date(0, 1, 1), Err(TemporalError::YearOutOfRange));
        assert_eq!(
            Timestamp::date(-4713, 1, 1),
            Err(TemporalError::YearOutOfRange)
        );
        assert_eq!(
            Timestamp::new(2026, 1, 1, 24, 0, 0),
            Err(TemporalError::TimeOutOfRange)
        );
        assert_eq!(
            Timestamp::new(2026, 1, 1, 0, 0, 60),
            Err(TemporalError::TimeOutOfRange)
        );
    }

    #[test]
    fn rejects_out_of_range_nanoseconds_and_offsets() {
        let base = Timestamp::date(2026, 1, 1).expect("valid");
        assert_eq!(
            base.with_nanosecond(1_000_000_000),
            Err(TemporalError::NanosecondOutOfRange)
        );
        assert!(base.with_nanosecond(999_999_999).is_ok());
        assert_eq!(TimeZone::offset(1440), Err(TemporalError::OffsetOutOfRange));
        assert_eq!(
            TimeZone::offset(-1440),
            Err(TemporalError::OffsetOutOfRange)
        );
        assert!(TimeZone::offset(1439).is_ok());
    }

    #[test]
    fn renders_iso_like_text() {
        let naive = Timestamp::new(2026, 9, 19, 13, 45, 30).expect("valid");
        assert_eq!(naive.to_string(), "2026-09-19T13:45:30");

        let fractional = naive.with_nanosecond(5_000_000).expect("valid");
        assert_eq!(fractional.to_string(), "2026-09-19T13:45:30.005000000");

        let zoned = naive.with_zone(TimeZone::offset(-330).expect("valid"));
        assert_eq!(zoned.to_string(), "2026-09-19T13:45:30-05:30");

        let bce = Timestamp::date(-44, 3, 15).expect("valid");
        assert_eq!(bce.to_string(), "-0044-03-15T00:00:00");
    }

    #[test]
    fn zone_accessors_are_explicit() {
        assert_eq!(TimeZone::default(), TimeZone::Unspecified);
        assert_eq!(TimeZone::Unspecified.offset_minutes(), None);
        assert_eq!(
            TimeZone::offset(420).expect("valid").offset_minutes(),
            Some(420)
        );
        assert_eq!(TimeZone::offset(0).expect("valid").to_string(), "+00:00");
    }

    #[test]
    fn stays_compact() {
        assert!(
            size_of::<Timestamp>() <= 16,
            "Timestamp grew to {} bytes",
            size_of::<Timestamp>()
        );
    }
}
