//! Wall-clock timestamps for provenance ("created", "modified", "set at").

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, UTC.
///
/// Stored as an SQLite `INTEGER`. These are provenance for a person reading
/// them ("modified yesterday"), never an ordering the store relies on: a wall
/// clock can step backwards, and nothing here assumes it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixTimeMs(i64);

impl UnixTimeMs {
    /// The current wall-clock time. A clock set before 1970 yields a negative
    /// value rather than a panic; one past the year 292 million saturates.
    #[must_use]
    pub fn now() -> Self {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since) => Self(i64::try_from(since.as_millis()).unwrap_or(i64::MAX)),
            Err(before) => Self(
                i64::try_from(before.duration().as_millis())
                    .map(|ms| -ms)
                    .unwrap_or(i64::MIN),
            ),
        }
    }

    /// A timestamp from a millisecond count.
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// The millisecond count.
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_after_2026_and_round_trips() {
        // 2026-01-01T00:00:00Z. Not a timing bound: a lower bound on a clock
        // that this repository could only have been built after.
        let now = UnixTimeMs::now();
        assert!(now.as_millis() > 1_767_225_600_000, "{now:?}");
        assert_eq!(UnixTimeMs::from_millis(now.as_millis()), now);
    }
}
