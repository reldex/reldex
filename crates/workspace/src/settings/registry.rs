//! The registry: every setting Reldex has, what it holds, its built-in
//! default, and the levels it may be set at.
//!
//! Owner rule (2026-09-19): every behaviour whose default the lead chose is
//! user-configurable, and each setting says at which level it lives —
//! application, connection profile, or worksheet. This module is where that
//! rule is written down; `every_owner_default_is_registered_with_its_levels`
//! in this module's tests is where it is checked.
//!
//! Settings are identified by [`SettingId`] everywhere a program chooses one,
//! and by the typed handles below ([`CONNECT_TIMEOUT`], …) wherever the value
//! is read or written, so the value's Rust type is fixed at compile time. The
//! only string form is [`SettingId::storage_key`], which exists for the SQLite
//! file and never crosses a layer boundary.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU32;

use super::value::{ByteLimit, SettingType, SettingValue, TimeLimit, ValueKind};

/// A level a setting's value can come from, in increasing precedence.
///
/// `effective = worksheet ?? profile ?? application ?? built-in`: the highest
/// level that holds a value wins, and [`Level::BuiltIn`] always holds one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    /// The default compiled into Reldex. Always present, never stored.
    BuiltIn,
    /// The user's application-wide default.
    Application,
    /// A connection profile's override.
    Profile,
    /// One worksheet's override.
    Worksheet,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BuiltIn => "built-in default",
            Self::Application => "application",
            Self::Profile => "connection profile",
            Self::Worksheet => "worksheet",
        })
    }
}

/// The set of levels a setting may be **set** at. [`Level::BuiltIn`] is
/// implicit: every setting has a built-in default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LevelSet(u8);

impl LevelSet {
    const APPLICATION_BIT: u8 = 1;
    const PROFILE_BIT: u8 = 2;
    const WORKSHEET_BIT: u8 = 4;

    /// The application level only.
    pub const APPLICATION: Self = Self(Self::APPLICATION_BIT);
    /// Application and connection profile.
    pub const APPLICATION_PROFILE: Self = Self(Self::APPLICATION_BIT | Self::PROFILE_BIT);
    /// Application, connection profile and worksheet.
    pub const ALL: Self = Self(Self::APPLICATION_BIT | Self::PROFILE_BIT | Self::WORKSHEET_BIT);

    /// Whether a value may be set at `level`. Always `false` for
    /// [`Level::BuiltIn`], which is not settable.
    #[must_use]
    pub const fn contains(self, level: Level) -> bool {
        let bit = match level {
            Level::BuiltIn => return false,
            Level::Application => Self::APPLICATION_BIT,
            Level::Profile => Self::PROFILE_BIT,
            Level::Worksheet => Self::WORKSHEET_BIT,
        };
        self.0 & bit != 0
    }

    /// The settable levels, lowest precedence first.
    pub fn iter(self) -> impl Iterator<Item = Level> {
        [Level::Application, Level::Profile, Level::Worksheet]
            .into_iter()
            .filter(move |level| self.contains(*level))
    }
}

/// Where a setting is shown and grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SettingGroup {
    /// Opening a connection.
    Connection,
    /// Running statements.
    Execution,
    /// Fetching results.
    Results,
    /// The server output (`DBMS_OUTPUT`) pane.
    ServerOutput,
}

/// When a changed value is first used.
///
/// A UI must say so: a connection option changed while a worksheet is
/// connected does nothing to that worksheet's session, which is the
/// session-stability rule (`SPEC.md` §9), not a bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TakesEffect {
    /// Baked into a connection when it is opened: sessions already open keep
    /// the value they were opened with.
    NextConnection,
    /// Read for each statement (or each result) as it starts.
    NextStatement,
}

/// What choosing "no limit" costs, so a UI can say it plainly before the user
/// commits to it (owner decision 2026-09-19; `SPEC.md` §10). The wording
/// itself is the UI's and is owner-approved (M4.6); this names *which*
/// consequence applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NoLimitConsequence {
    /// A statement that never returns can only be ended by disconnecting its
    /// worksheet, which loses the worksheet's transaction (`SPEC.md` §10).
    StatementEndsOnlyByDisconnecting,
    /// A connect to an address that accepts the connection and then goes
    /// silent waits forever (upstream gaps U-15, U-17).
    ConnectWaitsForever,
    /// The server buffers output without a limit, in the session's own
    /// memory on the server.
    ServerBuffersWithoutLimit,
}

/// Whether a limit-valued setting accepts "no limit".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unlimited {
    /// Only a bounded value is accepted.
    NotAllowed,
    /// "No limit" is accepted, at this cost.
    Allowed(NoLimitConsequence),
}

/// Inclusive bounds on a numeric value: a count, a number of seconds, or a
/// number of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Bounds {
    /// Smallest accepted value.
    pub min: u32,
    /// Largest accepted value.
    pub max: u32,
}

/// Every setting Reldex has.
///
/// `#[non_exhaustive]`: settings are added as features land (display options
/// arrive with the result grid, M5). A setting is never renumbered or
/// renamed; one that is retired stays readable so an older file loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum SettingId {
    /// See [`CONNECT_TIMEOUT`].
    ConnectTimeout,
    /// See [`REWRITE_TRIGGER_DDL`].
    RewriteTriggerDdl,
    /// See [`STATEMENT_TIME_LIMIT`].
    StatementTimeLimit,
    /// See [`FETCH_ROWS`].
    FetchRows,
    /// See [`FETCHES_IN_FLIGHT`].
    FetchesInFlight,
    /// See [`SERVER_OUTPUT_ENABLED`].
    ServerOutputEnabled,
    /// See [`SERVER_OUTPUT_BUFFER`].
    ServerOutputBuffer,
}

impl SettingId {
    /// Every setting, in registry order.
    pub const ALL: [Self; 7] = [
        Self::ConnectTimeout,
        Self::RewriteTriggerDdl,
        Self::StatementTimeLimit,
        Self::FetchRows,
        Self::FetchesInFlight,
        Self::ServerOutputEnabled,
        Self::ServerOutputBuffer,
    ];

    /// This setting's descriptor.
    #[must_use]
    pub const fn descriptor(self) -> &'static SettingDescriptor {
        match self {
            Self::ConnectTimeout => &DESCRIPTORS[0],
            Self::RewriteTriggerDdl => &DESCRIPTORS[1],
            Self::StatementTimeLimit => &DESCRIPTORS[2],
            Self::FetchRows => &DESCRIPTORS[3],
            Self::FetchesInFlight => &DESCRIPTORS[4],
            Self::ServerOutputEnabled => &DESCRIPTORS[5],
            Self::ServerOutputBuffer => &DESCRIPTORS[6],
        }
    }

    /// The stable key this setting is stored under in the SQLite file.
    ///
    /// Storage only: it never crosses a layer boundary, and nothing may parse
    /// meaning out of it. Never changed once released.
    #[must_use]
    pub const fn storage_key(self) -> &'static str {
        self.descriptor().storage_key
    }

    /// The setting stored under `key`, if this build knows it. A key this
    /// build does not know may belong to a newer Reldex, and is reported and
    /// kept rather than deleted.
    #[must_use]
    pub fn from_storage_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.storage_key() == key)
    }
}

impl fmt::Display for SettingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.storage_key())
    }
}

/// Why a value was refused for a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SettingError {
    /// The setting may not be set at this level (for example a worksheet
    /// value for a setting that is fixed when a connection opens).
    LevelNotAllowed {
        /// The setting.
        setting: SettingId,
        /// The level that was asked for.
        level: Level,
    },
    /// The value is of another kind than the setting holds.
    KindMismatch {
        /// The setting.
        setting: SettingId,
        /// The kind it holds.
        expected: ValueKind,
        /// The kind that was offered.
        found: ValueKind,
    },
    /// The number is outside the setting's bounds.
    OutOfBounds {
        /// The setting.
        setting: SettingId,
        /// The number offered.
        value: u32,
        /// The accepted range.
        bounds: Bounds,
    },
    /// "No limit" was offered for a setting that requires a limit.
    UnlimitedNotAllowed {
        /// The setting.
        setting: SettingId,
    },
}

impl fmt::Display for SettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LevelNotAllowed { setting, level } => {
                write!(f, "setting {setting} cannot be set at the {level} level")
            }
            Self::KindMismatch {
                setting,
                expected,
                found,
            } => write!(
                f,
                "setting {setting} holds a {expected:?} value, not a {found:?} value"
            ),
            Self::OutOfBounds {
                setting,
                value,
                bounds,
            } => write!(
                f,
                "setting {setting} accepts {}..={}, not {value}",
                bounds.min, bounds.max
            ),
            Self::UnlimitedNotAllowed { setting } => {
                write!(f, "setting {setting} requires a limit")
            }
        }
    }
}

impl std::error::Error for SettingError {}

/// Everything known about one setting.
#[derive(Debug)]
pub struct SettingDescriptor {
    id: SettingId,
    storage_key: &'static str,
    group: SettingGroup,
    kind: ValueKind,
    default: SettingValue,
    levels: LevelSet,
    bounds: Option<Bounds>,
    unlimited: Unlimited,
    takes_effect: TakesEffect,
    summary: &'static str,
}

impl SettingDescriptor {
    /// The setting.
    #[must_use]
    pub const fn id(&self) -> SettingId {
        self.id
    }

    /// Where it is grouped.
    #[must_use]
    pub const fn group(&self) -> SettingGroup {
        self.group
    }

    /// The kind of value it holds.
    #[must_use]
    pub const fn kind(&self) -> ValueKind {
        self.kind
    }

    /// The built-in default.
    #[must_use]
    pub const fn default_value(&self) -> SettingValue {
        self.default
    }

    /// The levels it may be set at.
    #[must_use]
    pub const fn levels(&self) -> LevelSet {
        self.levels
    }

    /// Bounds on its number, for counts, time limits and byte limits.
    #[must_use]
    pub const fn bounds(&self) -> Option<Bounds> {
        self.bounds
    }

    /// Whether it accepts "no limit", and at what cost.
    #[must_use]
    pub const fn unlimited(&self) -> Unlimited {
        self.unlimited
    }

    /// When a changed value is first used.
    #[must_use]
    pub const fn takes_effect(&self) -> TakesEffect {
        self.takes_effect
    }

    /// A one-line description for developers and logs. Not user-facing text:
    /// the UI owns its wording and its translations.
    #[must_use]
    pub const fn summary(&self) -> &'static str {
        self.summary
    }

    /// Checks `value` against this setting's kind and bounds, for `level`.
    ///
    /// # Errors
    ///
    /// [`SettingError`] naming the first rule the value breaks.
    pub fn check(&self, level: Level, value: SettingValue) -> Result<(), SettingError> {
        if !self.levels.contains(level) {
            return Err(SettingError::LevelNotAllowed {
                setting: self.id,
                level,
            });
        }
        self.check_value(value)
    }

    /// Checks `value` against this setting's kind and bounds, at any level.
    ///
    /// # Errors
    ///
    /// [`SettingError`] naming the first rule the value breaks.
    pub fn check_value(&self, value: SettingValue) -> Result<(), SettingError> {
        if value.kind() != self.kind {
            return Err(SettingError::KindMismatch {
                setting: self.id,
                expected: self.kind,
                found: value.kind(),
            });
        }
        let number = match value {
            SettingValue::Bool(_) => None,
            SettingValue::Count(count) => Some(count),
            SettingValue::TimeLimit(TimeLimit::Seconds(seconds)) => Some(seconds.get()),
            SettingValue::ByteLimit(ByteLimit::Bytes(bytes)) => Some(bytes.get()),
            SettingValue::TimeLimit(TimeLimit::NoLimit)
            | SettingValue::ByteLimit(ByteLimit::Unlimited) => {
                return match self.unlimited {
                    Unlimited::Allowed(_) => Ok(()),
                    Unlimited::NotAllowed => {
                        Err(SettingError::UnlimitedNotAllowed { setting: self.id })
                    }
                };
            }
        };
        if let (Some(number), Some(bounds)) = (number, self.bounds)
            && (number < bounds.min || number > bounds.max)
        {
            return Err(SettingError::OutOfBounds {
                setting: self.id,
                value: number,
                bounds,
            });
        }
        Ok(())
    }
}

/// A typed handle on one setting: the setting, and the Rust type of its
/// value.
///
/// Only this module creates them, and `Setting::new` refuses — at compile
/// time — a handle whose type does not match the setting's kind.
pub struct Setting<T> {
    id: SettingId,
    value_type: PhantomData<fn() -> T>,
}

impl<T: SettingType> Setting<T> {
    const fn new(id: SettingId) -> Self {
        assert!(
            T::KIND.same_as(id.descriptor().kind),
            "a typed setting handle must match its setting's kind"
        );
        Self {
            id,
            value_type: PhantomData,
        }
    }

    /// The setting.
    #[must_use]
    pub const fn id(self) -> SettingId {
        self.id
    }

    /// Its descriptor.
    #[must_use]
    pub const fn descriptor(self) -> &'static SettingDescriptor {
        self.id.descriptor()
    }

    /// The built-in default, typed.
    #[must_use]
    pub fn default_value(self) -> T {
        match T::from_value(self.descriptor().default) {
            Some(value) => value,
            // Unreachable: `new` checked the kind, and
            // `every_default_satisfies_its_own_descriptor` checks the
            // default's kind against the descriptor's.
            None => unreachable!("a setting's default has the setting's kind"),
        }
    }
}

impl<T> Clone for Setting<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Setting<T> {}

impl<T> fmt::Debug for Setting<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Setting").field(&self.id).finish()
    }
}

impl<T> PartialEq for Setting<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for Setting<T> {}

const fn nz(value: u32) -> NonZeroU32 {
    match NonZeroU32::new(value) {
        Some(value) => value,
        None => panic!("a registry constant must be non-zero"),
    }
}

/// How long opening a connection may take. Default **15 s**, per profile,
/// "no limit" allowed (owner decision 2026-09-19, `SPEC.md` §8; the driver
/// honours it on a helper thread, M2.1).
///
/// Bounded to one hour because that is the most the Oracle thin driver will
/// wait for a caller-set limit (`MAX_CONNECT_TIMEOUT`); a larger value would
/// be silently capped there, and a setting must not claim what it cannot do.
pub const CONNECT_TIMEOUT: Setting<TimeLimit> = Setting::new(SettingId::ConnectTimeout);

/// Whether trigger DDL whose body the driver would otherwise misread as bind
/// placeholders (`:NEW`/`:OLD`) is rewritten so it runs, with a warning that
/// shows the text actually sent. Default **on**, with a per-connection off
/// switch (owner decision 2026-09-19; upstream gap U-18; M2.2).
pub const REWRITE_TRIGGER_DDL: Setting<bool> = Setting::new(SettingId::RewriteTriggerDdl);

/// The per-statement time limit armed before a worksheet statement runs.
/// Default **600 s**; application, profile and worksheet levels; "no limit"
/// allowed with its consequence stated (owner decision 2026-09-19,
/// `SPEC.md` §10). It is never Cancel and must never be presented as Cancel.
///
/// Bounded to 24 hours: a longer bound is "no limit" in practice, and saying
/// so is more honest than offering a number nobody will wait for.
pub const STATEMENT_TIME_LIMIT: Setting<TimeLimit> = Setting::new(SettingId::StatementTimeLimit);

/// Rows fetched per round trip. Default **1,000** — the fastest-or-tied
/// setting in spike S15's sweep, which measured zero network latency; the
/// owner's sign-off on the number is still pending (`phase-1.md` §C.3 item
/// 10, M5.6), and it is a user setting either way.
pub const FETCH_ROWS: Setting<u32> = Setting::new(SettingId::FetchRows);

/// How many fetches a result view keeps requested ahead of the one it is
/// consuming. Default **2** (spike S15: more buys nothing at zero latency; to
/// be re-measured on a real network in M5.7).
///
/// Application level only: it tunes the one result pipeline the process has,
/// not a connection. Widening its levels later is a compatible change.
pub const FETCHES_IN_FLIGHT: Setting<u32> = Setting::new(SettingId::FetchesInFlight);

/// Whether the server output (`DBMS_OUTPUT`) pane collects output. Default
/// **off**, because collecting it costs a round trip after every statement
/// (M2.7, ADR-0002 amendment T).
pub const SERVER_OUTPUT_ENABLED: Setting<bool> = Setting::new(SettingId::ServerOutputEnabled);

/// How much output the server may buffer for a session while the pane is on.
/// Default **1,000,000 bytes**.
///
/// Bounded by default rather than unlimited so that one runaway loop cannot
/// make a statement's output drain unbounded while the drain itself has no
/// bound yet (M2.13); "unlimited", which is what SQL*Plus uses, is one
/// setting away. A driver may adjust the number to what its server accepts
/// and reports the size actually in force (ADR-0002 T1).
pub const SERVER_OUTPUT_BUFFER: Setting<ByteLimit> = Setting::new(SettingId::ServerOutputBuffer);

const DESCRIPTORS: [SettingDescriptor; 7] = [
    SettingDescriptor {
        id: SettingId::ConnectTimeout,
        storage_key: "connection.connect_timeout",
        group: SettingGroup::Connection,
        kind: ValueKind::TimeLimit,
        default: SettingValue::TimeLimit(TimeLimit::Seconds(nz(15))),
        levels: LevelSet::APPLICATION_PROFILE,
        bounds: Some(Bounds { min: 1, max: 3_600 }),
        unlimited: Unlimited::Allowed(NoLimitConsequence::ConnectWaitsForever),
        takes_effect: TakesEffect::NextConnection,
        summary: "How long opening a connection may take, in seconds, or no limit.",
    },
    SettingDescriptor {
        id: SettingId::RewriteTriggerDdl,
        storage_key: "connection.rewrite_trigger_ddl",
        group: SettingGroup::Connection,
        kind: ValueKind::Bool,
        default: SettingValue::Bool(true),
        levels: LevelSet::APPLICATION_PROFILE,
        bounds: None,
        unlimited: Unlimited::NotAllowed,
        takes_effect: TakesEffect::NextConnection,
        summary: "Rewrite trigger DDL whose body the driver would misread as binds, and report it.",
    },
    SettingDescriptor {
        id: SettingId::StatementTimeLimit,
        storage_key: "execution.statement_time_limit",
        group: SettingGroup::Execution,
        kind: ValueKind::TimeLimit,
        default: SettingValue::TimeLimit(TimeLimit::Seconds(nz(600))),
        levels: LevelSet::ALL,
        bounds: Some(Bounds {
            min: 1,
            max: 86_400,
        }),
        unlimited: Unlimited::Allowed(NoLimitConsequence::StatementEndsOnlyByDisconnecting),
        takes_effect: TakesEffect::NextStatement,
        summary: "Time limit armed before each worksheet statement, in seconds, or no limit.",
    },
    SettingDescriptor {
        id: SettingId::FetchRows,
        storage_key: "results.fetch_rows",
        group: SettingGroup::Results,
        kind: ValueKind::Count,
        default: SettingValue::Count(1_000),
        levels: LevelSet::ALL,
        bounds: Some(Bounds {
            min: 1,
            max: 100_000,
        }),
        unlimited: Unlimited::NotAllowed,
        takes_effect: TakesEffect::NextStatement,
        summary: "Rows fetched per round trip.",
    },
    SettingDescriptor {
        id: SettingId::FetchesInFlight,
        storage_key: "results.fetches_in_flight",
        group: SettingGroup::Results,
        kind: ValueKind::Count,
        default: SettingValue::Count(2),
        levels: LevelSet::APPLICATION,
        bounds: Some(Bounds { min: 1, max: 8 }),
        unlimited: Unlimited::NotAllowed,
        takes_effect: TakesEffect::NextStatement,
        summary: "Fetches kept requested ahead of the one a result view is consuming.",
    },
    SettingDescriptor {
        id: SettingId::ServerOutputEnabled,
        storage_key: "server_output.enabled",
        group: SettingGroup::ServerOutput,
        kind: ValueKind::Bool,
        default: SettingValue::Bool(false),
        levels: LevelSet::ALL,
        bounds: None,
        unlimited: Unlimited::NotAllowed,
        takes_effect: TakesEffect::NextStatement,
        summary: "Collect server output after every statement (one extra round trip each).",
    },
    SettingDescriptor {
        id: SettingId::ServerOutputBuffer,
        storage_key: "server_output.buffer",
        group: SettingGroup::ServerOutput,
        kind: ValueKind::ByteLimit,
        default: SettingValue::ByteLimit(ByteLimit::Bytes(nz(1_000_000))),
        levels: LevelSet::ALL,
        bounds: Some(Bounds {
            min: 1,
            max: 1 << 30,
        }),
        unlimited: Unlimited::Allowed(NoLimitConsequence::ServerBuffersWithoutLimit),
        takes_effect: TakesEffect::NextStatement,
        summary: "Bytes of server output the server may buffer per session, or unlimited.",
    },
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn descriptors_are_indexed_by_their_own_id() {
        for id in SettingId::ALL {
            assert_eq!(id.descriptor().id(), id);
        }
        assert_eq!(DESCRIPTORS.len(), SettingId::ALL.len());
    }

    #[test]
    fn storage_keys_are_unique_lowercase_and_round_trip() {
        let mut seen = HashSet::new();
        for id in SettingId::ALL {
            let key = id.storage_key();
            assert!(seen.insert(key), "duplicate key {key}");
            assert!(
                key.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'.'),
                "{key}"
            );
            assert_eq!(SettingId::from_storage_key(key), Some(id));
        }
        assert_eq!(SettingId::from_storage_key("no.such_setting"), None);
    }

    #[test]
    fn every_default_satisfies_its_own_descriptor() {
        for id in SettingId::ALL {
            let descriptor = id.descriptor();
            assert_eq!(descriptor.default_value().kind(), descriptor.kind(), "{id}");
            assert_eq!(
                descriptor.check_value(descriptor.default_value()),
                Ok(()),
                "{id}"
            );
            assert!(
                descriptor.levels().iter().next().is_some(),
                "{id} must be settable somewhere: every default is user-configurable"
            );
            let numeric = matches!(
                descriptor.kind(),
                ValueKind::Count | ValueKind::TimeLimit | ValueKind::ByteLimit
            );
            assert_eq!(descriptor.bounds().is_some(), numeric, "{id}");
            if let Some(bounds) = descriptor.bounds() {
                assert!(bounds.min >= 1 && bounds.min <= bounds.max, "{id}");
            }
        }
    }

    /// The owner's decisions of 2026-09-19/20, one line each: the default the
    /// lead chose and the levels the owner's rule puts it at.
    #[test]
    fn every_owner_default_is_registered_with_its_levels() {
        assert_eq!(CONNECT_TIMEOUT.default_value(), TimeLimit::Seconds(nz(15)));
        assert_eq!(
            CONNECT_TIMEOUT.descriptor().levels(),
            LevelSet::APPLICATION_PROFILE
        );
        assert!(matches!(
            CONNECT_TIMEOUT.descriptor().unlimited(),
            Unlimited::Allowed(NoLimitConsequence::ConnectWaitsForever)
        ));

        assert_eq!(
            STATEMENT_TIME_LIMIT.default_value(),
            TimeLimit::Seconds(nz(600))
        );
        assert_eq!(STATEMENT_TIME_LIMIT.descriptor().levels(), LevelSet::ALL);
        assert!(matches!(
            STATEMENT_TIME_LIMIT.descriptor().unlimited(),
            Unlimited::Allowed(NoLimitConsequence::StatementEndsOnlyByDisconnecting)
        ));

        assert!(REWRITE_TRIGGER_DDL.default_value());
        assert!(
            REWRITE_TRIGGER_DDL
                .descriptor()
                .levels()
                .contains(Level::Profile)
        );

        assert!(!SERVER_OUTPUT_ENABLED.default_value());
        assert_eq!(SERVER_OUTPUT_ENABLED.descriptor().levels(), LevelSet::ALL);
        assert_eq!(
            SERVER_OUTPUT_BUFFER.default_value(),
            ByteLimit::Bytes(nz(1_000_000))
        );

        assert_eq!(FETCH_ROWS.default_value(), 1_000);
        assert_eq!(FETCHES_IN_FLIGHT.default_value(), 2);
        assert_eq!(
            FETCHES_IN_FLIGHT.descriptor().levels(),
            LevelSet::APPLICATION
        );
    }

    #[test]
    fn the_connect_timeout_bound_matches_what_the_driver_honours() {
        // oracle-thin's MAX_CONNECT_TIMEOUT is one hour; a larger setting
        // would be capped there without saying so.
        assert_eq!(
            CONNECT_TIMEOUT.descriptor().bounds().map(|b| b.max),
            Some(3_600)
        );
    }

    #[test]
    fn check_names_the_rule_a_value_breaks() {
        let limit = STATEMENT_TIME_LIMIT.descriptor();
        assert_eq!(
            limit.check(Level::Worksheet, SettingValue::Bool(true)),
            Err(SettingError::KindMismatch {
                setting: SettingId::StatementTimeLimit,
                expected: ValueKind::TimeLimit,
                found: ValueKind::Bool,
            })
        );
        assert!(matches!(
            limit.check(
                Level::Profile,
                SettingValue::TimeLimit(TimeLimit::Seconds(nz(86_401)))
            ),
            Err(SettingError::OutOfBounds { value: 86_401, .. })
        ));
        assert_eq!(
            limit.check(
                Level::Worksheet,
                SettingValue::TimeLimit(TimeLimit::NoLimit)
            ),
            Ok(())
        );
        assert_eq!(
            limit.check(Level::BuiltIn, SettingValue::TimeLimit(TimeLimit::NoLimit)),
            Err(SettingError::LevelNotAllowed {
                setting: SettingId::StatementTimeLimit,
                level: Level::BuiltIn,
            })
        );

        let connect = CONNECT_TIMEOUT.descriptor();
        assert_eq!(
            connect.check(
                Level::Worksheet,
                SettingValue::TimeLimit(TimeLimit::Seconds(nz(5)))
            ),
            Err(SettingError::LevelNotAllowed {
                setting: SettingId::ConnectTimeout,
                level: Level::Worksheet,
            })
        );

        let fetch = FETCH_ROWS.descriptor();
        assert!(matches!(
            fetch.check(Level::Application, SettingValue::Count(0)),
            Err(SettingError::OutOfBounds { value: 0, .. })
        ));
        assert!(matches!(
            FETCHES_IN_FLIGHT
                .descriptor()
                .check(Level::Profile, SettingValue::Count(2)),
            Err(SettingError::LevelNotAllowed { .. })
        ));
    }

    #[test]
    fn a_limit_that_requires_a_number_refuses_unlimited() {
        // No registered setting refuses "no limit" today; the rule is still
        // enforced, so build a descriptor that does.
        let strict = SettingDescriptor {
            unlimited: Unlimited::NotAllowed,
            ..DESCRIPTORS[2]
        };
        assert_eq!(
            strict.check_value(SettingValue::TimeLimit(TimeLimit::NoLimit)),
            Err(SettingError::UnlimitedNotAllowed {
                setting: SettingId::StatementTimeLimit
            })
        );
    }

    #[test]
    fn level_sets_contain_what_they_say() {
        assert!(LevelSet::ALL.contains(Level::Worksheet));
        assert!(!LevelSet::APPLICATION_PROFILE.contains(Level::Worksheet));
        assert!(!LevelSet::APPLICATION.contains(Level::Profile));
        assert!(!LevelSet::ALL.contains(Level::BuiltIn));
        assert_eq!(
            LevelSet::APPLICATION_PROFILE.iter().collect::<Vec<_>>(),
            [Level::Application, Level::Profile]
        );
        assert!(Level::Worksheet > Level::Profile);
        assert!(Level::Profile > Level::Application);
        assert!(Level::Application > Level::BuiltIn);
    }
}
