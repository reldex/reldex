//! The caps and fetch settings one result store runs under (ADR-0004 RS2,
//! RS3), and the declared row width the first round trip is sized by.
//!
//! The values come from the user's settings (`results.*`, ADR-0006 and its
//! amendment "Result caps"), resolved by the composition root, which is the
//! only layer that can see both the settings store and a session. `db-core`
//! must not depend on the settings crate, so the provenance a setting's value
//! carries is mirrored here as [`CapSource`], with one level the settings do
//! not have: [`CapSource::FetchMore`], for a cap raised on one result only.

use std::num::NonZeroUsize;

use reldex_db_driver_api::{ColumnMetadata, DEFAULT_FETCH_ROWS, Number, SqlType, Timestamp};

use crate::store::segment::LOB_CELL_BYTES;

/// The most rows a result can show whatever its row cap says: the grid's own
/// ceiling, Qt's `int` (ADR-0004 RS3, "No limit").
pub const GRID_ROW_CEILING: usize = 2_147_483_647;

/// The bytes one round trip is sized to carry until M5.6 measures the budget
/// and the owner signs it off (ADR-0004 RS2, owner-review point (a)): a
/// **placeholder**, about 22 ms on the Oracle 19c curve of ADR-0004 Table 3a.
pub const DEFAULT_ROUND_TRIP_BYTES: NonZeroUsize = match NonZeroUsize::new(256 * 1024) {
    Some(bytes) => bytes,
    None => unreachable!(),
};

/// How many fetches one result keeps outstanding at most, when nothing says
/// otherwise: `results.fetches_in_flight`'s built-in default.
pub const DEFAULT_FETCHES_IN_FLIGHT: NonZeroUsize = match NonZeroUsize::new(2) {
    Some(count) => count,
    None => unreachable!(),
};

/// The declared width assumed for a variable-length column whose driver did
/// not report one: the common `VARCHAR` limit. It only sizes the first round
/// trip, and it errs toward a smaller one.
const UNKNOWN_VARIABLE_WIDTH: usize = 4_000;

/// The smallest width assumed for a column the driver renders as text
/// because the contract cannot represent it: its native size (a `ROWID` is
/// 10 bytes) says little about the length of the rendering.
const UNSUPPORTED_MIN_WIDTH: usize = 64;

/// A row or byte cap: a bound, or none at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cap {
    /// At most this many rows, or bytes.
    At(NonZeroUsize),
    /// No limit. For rows the grid's ceiling ([`GRID_ROW_CEILING`]) still
    /// applies; for bytes nothing does, and what that costs is stated where
    /// the user chooses it (ADR-0004 RS3).
    Unlimited,
}

impl Cap {
    /// The bound, or `None` for [`Cap::Unlimited`].
    #[must_use]
    pub const fn limit(self) -> Option<usize> {
        match self {
            Self::At(value) => Some(value.get()),
            Self::Unlimited => None,
        }
    }

    /// This cap raised by `step`: `Unlimited` when either is.
    const fn raised_by(self, step: Self) -> Self {
        match (self, step) {
            (Self::At(value), Self::At(step)) => Self::At(value.saturating_add(step.get())),
            _ => Self::Unlimited,
        }
    }
}

/// Where a cap in force came from, so the UI can say it (ADR-0004 RS2, "every
/// state carries the caps in force with their provenance").
///
/// The first four mirror the settings levels (`reldex_workspace::Level`);
/// the composition root maps one onto the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CapSource {
    /// The built-in default.
    BuiltIn,
    /// The user's application-wide setting.
    Application,
    /// The connection profile's setting.
    Profile,
    /// The worksheet's setting.
    Worksheet,
    /// Raised for this one result by "Fetch more" ([`crate::ResultStore::fetch_more`]).
    FetchMore,
}

/// A value and where it came from — the core-side twin of the settings'
/// `Resolved<T>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sourced<T> {
    /// The value in force.
    pub value: T,
    /// Where it came from.
    pub source: CapSource,
}

impl<T> Sourced<T> {
    /// A value from `source`.
    #[must_use]
    pub const fn new(value: T, source: CapSource) -> Self {
        Self { value, source }
    }
}

/// The caps one result runs under (ADR-0004 RS3): `results.max_rows`,
/// `results.max_bytes` and `results.close_cursor_at_limit`, each with its
/// provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResultCaps {
    max_rows: Sourced<Cap>,
    max_bytes: Sourced<Cap>,
    close_cursor_at_limit: Sourced<bool>,
}

impl ResultCaps {
    /// Row and byte caps, keeping the cursor open at the cap (the built-in
    /// default of `results.close_cursor_at_limit`).
    #[must_use]
    pub const fn new(max_rows: Sourced<Cap>, max_bytes: Sourced<Cap>) -> Self {
        Self {
            max_rows,
            max_bytes,
            close_cursor_at_limit: Sourced::new(false, CapSource::BuiltIn),
        }
    }

    /// Whether the cursor is closed when a cap is reached.
    #[must_use]
    pub const fn with_close_cursor_at_limit(mut self, close: Sourced<bool>) -> Self {
        self.close_cursor_at_limit = close;
        self
    }

    /// The row cap in force.
    #[must_use]
    pub const fn max_rows(&self) -> Sourced<Cap> {
        self.max_rows
    }

    /// The byte cap in force.
    #[must_use]
    pub const fn max_bytes(&self) -> Sourced<Cap> {
        self.max_bytes
    }

    /// Whether the cursor is closed at the cap, and where that came from.
    #[must_use]
    pub const fn close_cursor_at_limit(&self) -> Sourced<bool> {
        self.close_cursor_at_limit
    }

    /// Both caps raised by the caps in `steps`, from [`CapSource::FetchMore`].
    pub(crate) const fn raised_by(mut self, steps: &Self) -> Self {
        self.max_rows = Sourced::new(
            self.max_rows.value.raised_by(steps.max_rows.value),
            CapSource::FetchMore,
        );
        self.max_bytes = Sourced::new(
            self.max_bytes.value.raised_by(steps.max_bytes.value),
            CapSource::FetchMore,
        );
        self
    }

    /// Both caps lifted, from [`CapSource::FetchMore`].
    pub(crate) const fn lifted(mut self) -> Self {
        self.max_rows = Sourced::new(Cap::Unlimited, CapSource::FetchMore);
        self.max_bytes = Sourced::new(Cap::Unlimited, CapSource::FetchMore);
        self
    }
}

/// Everything one [`crate::ResultStore`] runs under: its caps and its fetch
/// settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResultPolicy {
    caps: ResultCaps,
    fetch_rows: NonZeroUsize,
    fetches_in_flight: NonZeroUsize,
    round_trip_bytes: NonZeroUsize,
}

impl ResultPolicy {
    /// `caps`, with `results.fetch_rows` and `results.fetches_in_flight` at
    /// their built-in defaults and the placeholder round-trip budget
    /// ([`DEFAULT_ROUND_TRIP_BYTES`]).
    #[must_use]
    pub const fn new(caps: ResultCaps) -> Self {
        Self {
            caps,
            fetch_rows: DEFAULT_FETCH_ROWS,
            fetches_in_flight: DEFAULT_FETCHES_IN_FLIGHT,
            round_trip_bytes: DEFAULT_ROUND_TRIP_BYTES,
        }
    }

    /// The most rows one round trip may ask for: `results.fetch_rows`, which
    /// ADR-0004 RS2 turns from the size of every round trip into its upper
    /// bound.
    #[must_use]
    pub const fn with_fetch_rows(mut self, rows: NonZeroUsize) -> Self {
        self.fetch_rows = rows;
        self
    }

    /// The most fetches this result keeps outstanding:
    /// `results.fetches_in_flight`, per result (ADR-0004 RS3).
    #[must_use]
    pub const fn with_fetches_in_flight(mut self, fetches: NonZeroUsize) -> Self {
        self.fetches_in_flight = fetches;
        self
    }

    /// The bytes one round trip is sized to carry (ADR-0004 RS2).
    #[must_use]
    pub const fn with_round_trip_bytes(mut self, bytes: NonZeroUsize) -> Self {
        self.round_trip_bytes = bytes;
        self
    }

    /// The caps.
    #[must_use]
    pub const fn caps(&self) -> ResultCaps {
        self.caps
    }

    /// The most rows one round trip may ask for.
    #[must_use]
    pub const fn fetch_rows(&self) -> NonZeroUsize {
        self.fetch_rows
    }

    /// The most fetches outstanding for one result.
    #[must_use]
    pub const fn fetches_in_flight(&self) -> NonZeroUsize {
        self.fetches_in_flight
    }

    /// The bytes one round trip is sized to carry.
    #[must_use]
    pub const fn round_trip_bytes(&self) -> NonZeroUsize {
        self.round_trip_bytes
    }
}

/// The bytes one row of this select list can occupy in the store, from the
/// describe alone: what the first round trip is sized by, before any row has
/// been seen (ADR-0004 RS2, RS3).
///
/// Counted the way a segment accounts its bytes, so the byte cap and the
/// round-trip budget compare like with like: a fixed width for `NUMBER` (its
/// uncompacted 44 bytes — the scaled form is never larger), dates and
/// timestamps; the declared maximum plus one offset for text and bytes; the
/// nominal LOB charge for a large object; one NULL bit per column.
pub(crate) fn declared_row_width(columns: &[ColumnMetadata]) -> usize {
    let offset = size_of::<usize>();
    let variable = |column: &ColumnMetadata| {
        column
            .max_size_bytes()
            .map_or(UNKNOWN_VARIABLE_WIDTH, |bytes| {
                usize::try_from(bytes).unwrap_or(usize::MAX)
            })
            .saturating_add(offset)
    };
    let cells = columns.iter().fold(0_usize, |width, column| {
        let cell = match column.sql_type() {
            SqlType::Boolean => size_of::<bool>(),
            SqlType::Number => size_of::<Number>(),
            SqlType::BinaryFloat => size_of::<f32>(),
            SqlType::BinaryDouble => size_of::<f64>(),
            SqlType::Date | SqlType::Timestamp | SqlType::TimestampWithTimeZone => {
                size_of::<Timestamp>()
            }
            SqlType::CharacterLob { .. } | SqlType::BinaryLob => LOB_CELL_BYTES,
            SqlType::Unsupported => variable(column).max(UNSUPPORTED_MIN_WIDTH + offset),
            // Text, JSON, RAW, and any family this core does not know yet:
            // the declared maximum is the best a describe can say.
            _ => variable(column),
        };
        width.saturating_add(cell)
    });
    cells.saturating_add(columns.len().div_ceil(8)).max(1)
}

#[cfg(test)]
mod tests {
    use super::{Cap, CapSource, ResultCaps, Sourced, declared_row_width};
    use reldex_db_driver_api::{ColumnMetadata, SqlType};
    use std::num::NonZeroUsize;

    fn at(value: usize) -> Cap {
        Cap::At(NonZeroUsize::new(value).expect("non-zero"))
    }

    #[test]
    fn declared_widths_follow_the_describe() {
        let s14 = [
            ColumnMetadata::new("ID", SqlType::Number),
            ColumnMetadata::new("NAME", SqlType::VARCHAR).with_max_size_bytes(160),
            ColumnMetadata::new("CREATED", SqlType::Date),
        ];
        // 44 + (160 + 8) + 16 + one NULL byte.
        assert_eq!(declared_row_width(&s14), 229);

        let wide: Vec<ColumnMetadata> = (0..4)
            .map(|i| {
                ColumnMetadata::new(format!("C{i}"), SqlType::VARCHAR).with_max_size_bytes(4000)
            })
            .collect();
        assert_eq!(declared_row_width(&wide), 4 * 4008 + 1);

        // Nothing declared: the common VARCHAR limit, not zero.
        let unknown = [ColumnMetadata::new("X", SqlType::VARCHAR)];
        assert_eq!(declared_row_width(&unknown), 4008 + 1);
        let lob = [ColumnMetadata::new(
            "DOC",
            SqlType::CharacterLob { national: false },
        )];
        assert_eq!(declared_row_width(&lob), 320 + 1);
        assert_eq!(declared_row_width(&[]), 1);
    }

    #[test]
    fn fetch_more_raises_both_caps_by_their_own_step_and_says_so() {
        let caps = ResultCaps::new(
            Sourced::new(at(1_000), CapSource::Worksheet),
            Sourced::new(at(1 << 20), CapSource::BuiltIn),
        );
        let raised = caps.raised_by(&caps);
        assert_eq!(
            raised.max_rows(),
            Sourced::new(at(2_000), CapSource::FetchMore)
        );
        assert_eq!(
            raised.max_bytes(),
            Sourced::new(at(2 << 20), CapSource::FetchMore)
        );
        assert_eq!(raised.raised_by(&caps).max_rows().value, at(3_000));
        assert_eq!(raised.close_cursor_at_limit(), caps.close_cursor_at_limit());
        let lifted = caps.lifted();
        assert_eq!(lifted.max_rows().value, Cap::Unlimited);
        assert_eq!(lifted.max_bytes().value, Cap::Unlimited);
        assert_eq!(Cap::Unlimited.limit(), None);
        assert_eq!(at(7).limit(), Some(7));
    }
}
