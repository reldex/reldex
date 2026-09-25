//! One level's worth of setting values: the application defaults, one
//! profile's overrides, or one worksheet's overrides.
//!
//! The level is part of the type ([`SettingsLayer<ProfileScope>`] and so on),
//! so a worksheet's layer cannot be passed where a profile's is expected and
//! resolution never has to trust a run-time tag.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use super::registry::{Level, Setting, SettingError, SettingId};
use super::value::{SettingType, SettingValue};

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::ApplicationScope {}
    impl Sealed for super::ProfileScope {}
    impl Sealed for super::WorksheetScope {}
}

/// A level a [`SettingsLayer`] belongs to. Sealed: the three levels are the
/// architecture (`SPEC.md` §10, owner decision 2026-09-19).
pub trait ScopeLevel: sealed::Sealed {
    /// The level.
    const LEVEL: Level;
}

/// Marker: the application level.
#[derive(Debug)]
pub enum ApplicationScope {}

/// Marker: the connection-profile level.
#[derive(Debug)]
pub enum ProfileScope {}

/// Marker: the worksheet level.
#[derive(Debug)]
pub enum WorksheetScope {}

impl ScopeLevel for ApplicationScope {
    const LEVEL: Level = Level::Application;
}

impl ScopeLevel for ProfileScope {
    const LEVEL: Level = Level::Profile;
}

impl ScopeLevel for WorksheetScope {
    const LEVEL: Level = Level::Worksheet;
}

/// The values set at one level. A setting that is absent here is inherited
/// from the level below.
///
/// Every value in a layer has passed its descriptor's
/// [`check`](super::SettingDescriptor::check) for this layer's level: `set`
/// refuses anything else, and the store rejects (and reports) a stored row
/// that no longer passes.
pub struct SettingsLayer<S: ScopeLevel> {
    values: BTreeMap<SettingId, SettingValue>,
    scope: PhantomData<fn() -> S>,
}

impl<S: ScopeLevel> SettingsLayer<S> {
    /// An empty layer: everything inherited.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: BTreeMap::new(),
            scope: PhantomData,
        }
    }

    /// The level this layer belongs to.
    #[must_use]
    pub const fn level(&self) -> Level {
        S::LEVEL
    }

    /// The value set here, if any.
    #[must_use]
    pub fn get<T: SettingType>(&self, setting: Setting<T>) -> Option<T> {
        self.values
            .get(&setting.id())
            .copied()
            .and_then(T::from_value)
    }

    /// The value set here for `id`, if any.
    #[must_use]
    pub fn get_value(&self, id: SettingId) -> Option<SettingValue> {
        self.values.get(&id).copied()
    }

    /// Sets a value at this level, returning the one it replaced.
    ///
    /// # Errors
    ///
    /// [`SettingError::LevelNotAllowed`] if the setting cannot be set at this
    /// level, or [`SettingError::OutOfBounds`] /
    /// [`SettingError::UnlimitedNotAllowed`] if the value is not accepted.
    pub fn set<T: SettingType>(
        &mut self,
        setting: Setting<T>,
        value: T,
    ) -> Result<Option<T>, SettingError> {
        self.set_value(setting.id(), value.into_value())
            .map(|previous| previous.and_then(T::from_value))
    }

    /// Sets a value chosen at run time — the store's and the FFI's shape.
    ///
    /// # Errors
    ///
    /// As [`SettingsLayer::set`], plus [`SettingError::KindMismatch`].
    pub fn set_value(
        &mut self,
        id: SettingId,
        value: SettingValue,
    ) -> Result<Option<SettingValue>, SettingError> {
        id.descriptor().check(S::LEVEL, value)?;
        Ok(self.values.insert(id, value))
    }

    /// Removes this level's value, so the setting is inherited again. Returns
    /// the value removed.
    pub fn clear(&mut self, id: SettingId) -> Option<SettingValue> {
        self.values.remove(&id)
    }

    /// Whether nothing is set at this level.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// How many settings are set at this level.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// The values set here, in registry order.
    pub fn iter(&self) -> impl Iterator<Item = (SettingId, SettingValue)> + '_ {
        self.values.iter().map(|(id, value)| (*id, *value))
    }

    /// Inserts without the level check. Only for tests that must prove
    /// resolution ignores a disallowed level even if one got in.
    #[cfg(test)]
    pub(crate) fn insert_unchecked(&mut self, id: SettingId, value: SettingValue) {
        self.values.insert(id, value);
    }
}

impl<S: ScopeLevel> Default for SettingsLayer<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: ScopeLevel> Clone for SettingsLayer<S> {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            scope: PhantomData,
        }
    }
}

impl<S: ScopeLevel> PartialEq for SettingsLayer<S> {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl<S: ScopeLevel> Eq for SettingsLayer<S> {}

impl<S: ScopeLevel> fmt::Debug for SettingsLayer<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsLayer")
            .field("level", &S::LEVEL)
            .field("values", &self.values)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::registry::{
        CONNECT_TIMEOUT, FETCH_ROWS, FETCHES_IN_FLIGHT, STATEMENT_TIME_LIMIT,
    };
    use crate::settings::value::TimeLimit;

    #[test]
    fn set_get_clear_round_trip() {
        let mut layer = SettingsLayer::<ProfileScope>::new();
        assert!(layer.is_empty());
        assert_eq!(layer.set(FETCH_ROWS, 500), Ok(None));
        assert_eq!(layer.set(FETCH_ROWS, 250), Ok(Some(500)));
        assert_eq!(layer.get(FETCH_ROWS), Some(250));
        assert_eq!(layer.len(), 1);
        assert_eq!(
            layer.clear(SettingId::FetchRows),
            Some(SettingValue::Count(250))
        );
        assert_eq!(layer.get(FETCH_ROWS), None);
        assert_eq!(layer.level(), Level::Profile);
    }

    #[test]
    fn a_layer_refuses_a_setting_its_level_does_not_allow() {
        let mut worksheet = SettingsLayer::<WorksheetScope>::new();
        assert!(matches!(
            worksheet.set(CONNECT_TIMEOUT, TimeLimit::NoLimit),
            Err(SettingError::LevelNotAllowed {
                level: Level::Worksheet,
                ..
            })
        ));
        let mut profile = SettingsLayer::<ProfileScope>::new();
        assert!(matches!(
            profile.set(FETCHES_IN_FLIGHT, 3),
            Err(SettingError::LevelNotAllowed { .. })
        ));
        assert!(worksheet.is_empty() && profile.is_empty());
    }

    #[test]
    fn set_value_refuses_the_wrong_kind() {
        let mut layer = SettingsLayer::<ApplicationScope>::new();
        assert!(matches!(
            layer.set_value(SettingId::StatementTimeLimit, SettingValue::Count(5)),
            Err(SettingError::KindMismatch { .. })
        ));
        assert_eq!(
            layer.set(STATEMENT_TIME_LIMIT, TimeLimit::NoLimit),
            Ok(None)
        );
    }
}
