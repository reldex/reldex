//! Resolution: `effective = worksheet ?? profile ?? application ?? built-in`,
//! with the level the value came from.

use super::layer::{ApplicationScope, ProfileScope, ScopeLevel, SettingsLayer, WorksheetScope};
use super::registry::{Level, Setting, SettingId};
use super::value::{SettingType, SettingValue};

/// An effective value and the level it came from — what a UI needs to say
/// "10 min (from profile)" or "inherited from application".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Resolved<T> {
    /// The effective value.
    pub value: T,
    /// The level that supplied it.
    pub source: Level,
}

/// The layers in force for one lookup. Any of them may be missing: a
/// connection that is not yet a worksheet has no worksheet layer, and a
/// fresh install has no application layer at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveContext<'a> {
    application: Option<&'a SettingsLayer<ApplicationScope>>,
    profile: Option<&'a SettingsLayer<ProfileScope>>,
    worksheet: Option<&'a SettingsLayer<WorksheetScope>>,
}

impl<'a> ResolveContext<'a> {
    /// No layers: everything resolves to its built-in default.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            application: None,
            profile: None,
            worksheet: None,
        }
    }

    /// Adds (or replaces) the application layer.
    #[must_use]
    pub const fn with_application(mut self, layer: &'a SettingsLayer<ApplicationScope>) -> Self {
        self.application = Some(layer);
        self
    }

    /// Adds (or replaces) the profile layer.
    #[must_use]
    pub const fn with_profile(mut self, layer: &'a SettingsLayer<ProfileScope>) -> Self {
        self.profile = Some(layer);
        self
    }

    /// Adds (or replaces) the worksheet layer.
    #[must_use]
    pub const fn with_worksheet(mut self, layer: &'a SettingsLayer<WorksheetScope>) -> Self {
        self.worksheet = Some(layer);
        self
    }

    /// The effective value of a setting, and where it came from.
    #[must_use]
    pub fn resolve<T: SettingType>(&self, setting: Setting<T>) -> Resolved<T> {
        let resolved = self.resolve_value(setting.id());
        match T::from_value(resolved.value) {
            Some(value) => Resolved {
                value,
                source: resolved.source,
            },
            // Every layer checks the kind on the way in, so a value of another
            // kind cannot be there; the built-in default is the safe answer if
            // one somehow is.
            None => Resolved {
                value: setting.default_value(),
                source: Level::BuiltIn,
            },
        }
    }

    /// The effective value of a setting chosen at run time.
    ///
    /// A level the setting does not allow is skipped even if its layer holds
    /// a value: the layers already refuse such values, and resolution does
    /// not rely on that.
    #[must_use]
    pub fn resolve_value(&self, id: SettingId) -> Resolved<SettingValue> {
        let descriptor = id.descriptor();
        let allowed = descriptor.levels();
        let candidates = [
            layer_value(self.worksheet, id),
            layer_value(self.profile, id),
            layer_value(self.application, id),
        ];
        for (level, value) in candidates.into_iter().flatten() {
            if allowed.contains(level) && descriptor.check_value(value).is_ok() {
                return Resolved {
                    value,
                    source: level,
                };
            }
        }
        Resolved {
            value: descriptor.default_value(),
            source: Level::BuiltIn,
        }
    }
}

fn layer_value<S: ScopeLevel>(
    layer: Option<&SettingsLayer<S>>,
    id: SettingId,
) -> Option<(Level, SettingValue)> {
    layer.and_then(|layer| layer.get_value(id).map(|value| (S::LEVEL, value)))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::settings::registry::{CONNECT_TIMEOUT, FETCH_ROWS, STATEMENT_TIME_LIMIT};
    use crate::settings::value::TimeLimit;

    fn secs(n: u32) -> TimeLimit {
        TimeLimit::Seconds(NonZeroU32::new(n).expect("non-zero"))
    }

    /// One level's state in the truth table: four states, three levels,
    /// 4³ = 64 combinations.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum State {
        /// The context has no layer for this level at all.
        NoLayer,
        /// The layer exists and does not set the setting.
        Unset,
        /// The layer sets a value unique to this level.
        SetDistinct,
        /// The layer sets a value equal to the built-in default. It must
        /// still be reported as this level's: provenance is where the value
        /// came from, not whether it differs.
        SetToDefault,
    }

    const STATES: [State; 4] = [
        State::NoLayer,
        State::Unset,
        State::SetDistinct,
        State::SetToDefault,
    ];

    fn value_for(level: Level, state: State, default: TimeLimit) -> Option<TimeLimit> {
        match state {
            State::NoLayer | State::Unset => None,
            State::SetToDefault => Some(default),
            State::SetDistinct => Some(match level {
                Level::Application => secs(111),
                Level::Profile => secs(222),
                Level::Worksheet => TimeLimit::NoLimit,
                Level::BuiltIn => unreachable!("built-in is not a layer"),
            }),
        }
    }

    /// Runs all 64 combinations for one time-limit setting and checks each
    /// against the rule written out independently of the resolver.
    fn truth_table(setting: Setting<TimeLimit>) -> usize {
        let default = setting.default_value();
        let allowed = setting.descriptor().levels();
        let mut rows = 0;
        for app_state in STATES {
            for profile_state in STATES {
                for worksheet_state in STATES {
                    let mut app = SettingsLayer::<ApplicationScope>::new();
                    let mut profile = SettingsLayer::<ProfileScope>::new();
                    let mut worksheet = SettingsLayer::<WorksheetScope>::new();
                    let states = [
                        (Level::Worksheet, worksheet_state),
                        (Level::Profile, profile_state),
                        (Level::Application, app_state),
                    ];
                    for (level, state) in states {
                        let Some(value) = value_for(level, state, default) else {
                            continue;
                        };
                        let value = SettingValue::TimeLimit(value);
                        // A level the setting does not allow is forced in, to
                        // prove resolution skips it rather than trusting the
                        // layer's own check.
                        match level {
                            Level::Application => app.insert_unchecked(setting.id(), value),
                            Level::Profile => profile.insert_unchecked(setting.id(), value),
                            Level::Worksheet => worksheet.insert_unchecked(setting.id(), value),
                            Level::BuiltIn => unreachable!(),
                        }
                    }
                    let mut context = ResolveContext::new();
                    if app_state != State::NoLayer {
                        context = context.with_application(&app);
                    }
                    if profile_state != State::NoLayer {
                        context = context.with_profile(&profile);
                    }
                    if worksheet_state != State::NoLayer {
                        context = context.with_worksheet(&worksheet);
                    }

                    // The rule, stated directly: the highest allowed level
                    // whose state is "set" supplies the value.
                    let expected = states
                        .iter()
                        .find_map(|(level, state)| {
                            let value = value_for(*level, *state, default)?;
                            allowed.contains(*level).then_some(Resolved {
                                value,
                                source: *level,
                            })
                        })
                        .unwrap_or(Resolved {
                            value: default,
                            source: Level::BuiltIn,
                        });

                    assert_eq!(
                        context.resolve(setting),
                        expected,
                        "{setting:?}: application={app_state:?} profile={profile_state:?} \
                         worksheet={worksheet_state:?}"
                    );
                    assert_eq!(
                        context.resolve_value(setting.id()),
                        Resolved {
                            value: SettingValue::TimeLimit(expected.value),
                            source: expected.source,
                        }
                    );
                    rows += 1;
                }
            }
        }
        rows
    }

    #[test]
    fn truth_table_for_a_setting_allowed_at_every_level() {
        assert_eq!(truth_table(STATEMENT_TIME_LIMIT), 64);
    }

    #[test]
    fn truth_table_for_a_setting_that_skips_the_worksheet_level() {
        assert_eq!(truth_table(CONNECT_TIMEOUT), 64);
    }

    #[test]
    fn provenance_names_the_level_even_when_the_value_equals_the_default() {
        let mut profile = SettingsLayer::<ProfileScope>::new();
        profile
            .set(FETCH_ROWS, FETCH_ROWS.default_value())
            .expect("allowed");
        let resolved = ResolveContext::new()
            .with_profile(&profile)
            .resolve(FETCH_ROWS);
        assert_eq!(resolved.value, 1_000);
        assert_eq!(resolved.source, Level::Profile);
    }

    #[test]
    fn nothing_set_anywhere_is_the_built_in_default() {
        for id in SettingId::ALL {
            let resolved = ResolveContext::new().resolve_value(id);
            assert_eq!(resolved.source, Level::BuiltIn);
            assert_eq!(resolved.value, id.descriptor().default_value());
        }
    }
}
