//! Typed settings with three-level resolution and provenance.
//!
//! - [`registry`] — every setting, its value kind, built-in default, bounds
//!   and the levels it may be set at.
//! - [`SettingsLayer`] — the values one level sets.
//! - [`ResolveContext`] — `effective = worksheet ?? profile ?? application ??
//!   built-in`, reported as a [`Resolved`] with the [`Level`] it came from.

mod layer;
pub mod registry;
mod resolve;
mod value;

pub use layer::{ApplicationScope, ProfileScope, ScopeLevel, SettingsLayer, WorksheetScope};
pub use registry::{
    Bounds, CONNECT_TIMEOUT, FETCH_ROWS, FETCHES_IN_FLIGHT, HISTORY_MAX_ENTRIES_PER_PROFILE,
    Level, LevelSet, NoLimitConsequence, REWRITE_TRIGGER_DDL, SERVER_OUTPUT_BUFFER,
    SERVER_OUTPUT_ENABLED, STATEMENT_TIME_LIMIT, Setting, SettingDescriptor, SettingError,
    SettingGroup, SettingId, TakesEffect, Unlimited,
};
pub use resolve::{ResolveContext, Resolved};
pub use value::{ByteLimit, EntryLimit, SettingType, SettingValue, TimeLimit, ValueKind};
