//! The value types a setting can hold.
//!
//! Every setting has exactly one [`ValueKind`], and the typed handles in
//! [`super::registry`] tie a Rust type to it at compile time, so a caller
//! never parses or formats a setting's value: a time limit is a
//! [`TimeLimit`], not the string `"600"` or `"none"`.

use std::num::NonZeroU32;
use std::time::Duration;

use reldex_db_driver_api::ServerOutputBuffer;

/// A limit in whole seconds, or explicitly no limit at all.
///
/// "No limit" is a value in its own right, never an absent one: an unset
/// setting inherits from the level below it, while `NoLimit` is a choice the
/// user made and must be shown as such, together with its consequence
/// ([`super::Unlimited::Allowed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeLimit {
    /// At most this many seconds.
    Seconds(NonZeroU32),
    /// No limit.
    NoLimit,
}

impl TimeLimit {
    /// A limit of `seconds`; `None` for zero, which is not a limit.
    #[must_use]
    pub const fn seconds(seconds: u32) -> Option<Self> {
        match NonZeroU32::new(seconds) {
            Some(seconds) => Some(Self::Seconds(seconds)),
            None => None,
        }
    }

    /// The limit as a [`Duration`]; `None` for [`TimeLimit::NoLimit`].
    #[must_use]
    pub const fn as_duration(self) -> Option<Duration> {
        match self {
            Self::Seconds(seconds) => Some(Duration::from_secs(seconds.get() as u64)),
            Self::NoLimit => None,
        }
    }
}

/// A budget in bytes, or explicitly unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ByteLimit {
    /// At most this many bytes.
    Bytes(NonZeroU32),
    /// No limit.
    Unlimited,
}

impl ByteLimit {
    /// A limit of `bytes`; `None` for zero.
    #[must_use]
    pub const fn bytes(bytes: u32) -> Option<Self> {
        match NonZeroU32::new(bytes) {
            Some(bytes) => Some(Self::Bytes(bytes)),
            None => None,
        }
    }
}

impl From<ByteLimit> for ServerOutputBuffer {
    fn from(limit: ByteLimit) -> Self {
        match limit {
            ByteLimit::Bytes(bytes) => Self::Bytes(bytes),
            ByteLimit::Unlimited => Self::Unlimited,
        }
    }
}

/// Which of the value types a setting holds.
///
/// `#[repr(u8)]` so the typed handles can compare kinds in a `const`
/// context and refuse, at compile time, a handle whose Rust type does not
/// match its setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum ValueKind {
    /// On or off.
    Bool = 1,
    /// A [`TimeLimit`].
    TimeLimit = 2,
    /// A whole number, within the setting's bounds.
    Count = 3,
    /// A [`ByteLimit`].
    ByteLimit = 4,
}

impl ValueKind {
    /// Kind equality usable in a `const` context.
    pub(crate) const fn same_as(self, other: Self) -> bool {
        self as u8 == other as u8
    }
}

/// A setting's value, of any kind.
///
/// Used where the setting is chosen at run time — the store, and the FFI
/// surface M2.11 builds — and checked against the setting's descriptor on
/// every write. Code that knows which setting it means uses the typed handles
/// in [`super::registry`] and never sees this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SettingValue {
    /// A [`ValueKind::Bool`] value.
    Bool(bool),
    /// A [`ValueKind::TimeLimit`] value.
    TimeLimit(TimeLimit),
    /// A [`ValueKind::Count`] value.
    Count(u32),
    /// A [`ValueKind::ByteLimit`] value.
    ByteLimit(ByteLimit),
}

impl SettingValue {
    /// The kind of this value.
    #[must_use]
    pub const fn kind(self) -> ValueKind {
        match self {
            Self::Bool(_) => ValueKind::Bool,
            Self::TimeLimit(_) => ValueKind::TimeLimit,
            Self::Count(_) => ValueKind::Count,
            Self::ByteLimit(_) => ValueKind::ByteLimit,
        }
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for bool {}
    impl Sealed for u32 {}
    impl Sealed for super::TimeLimit {}
    impl Sealed for super::ByteLimit {}
}

/// A Rust type that is the value of some setting kind.
///
/// Sealed: the set of kinds is this crate's schema, and a kind added outside
/// it would have no storage encoding.
pub trait SettingType: Copy + sealed::Sealed {
    /// The kind this type is the value of.
    const KIND: ValueKind;

    /// Wraps the value.
    fn into_value(self) -> SettingValue;

    /// Unwraps a value of this kind; `None` for any other kind.
    fn from_value(value: SettingValue) -> Option<Self>;
}

impl SettingType for bool {
    const KIND: ValueKind = ValueKind::Bool;

    fn into_value(self) -> SettingValue {
        SettingValue::Bool(self)
    }

    fn from_value(value: SettingValue) -> Option<Self> {
        match value {
            SettingValue::Bool(value) => Some(value),
            _ => None,
        }
    }
}

impl SettingType for TimeLimit {
    const KIND: ValueKind = ValueKind::TimeLimit;

    fn into_value(self) -> SettingValue {
        SettingValue::TimeLimit(self)
    }

    fn from_value(value: SettingValue) -> Option<Self> {
        match value {
            SettingValue::TimeLimit(value) => Some(value),
            _ => None,
        }
    }
}

impl SettingType for u32 {
    const KIND: ValueKind = ValueKind::Count;

    fn into_value(self) -> SettingValue {
        SettingValue::Count(self)
    }

    fn from_value(value: SettingValue) -> Option<Self> {
        match value {
            SettingValue::Count(value) => Some(value),
            _ => None,
        }
    }
}

impl SettingType for ByteLimit {
    const KIND: ValueKind = ValueKind::ByteLimit;

    fn into_value(self) -> SettingValue {
        SettingValue::ByteLimit(self)
    }

    fn from_value(value: SettingValue) -> Option<Self> {
        match value {
            SettingValue::ByteLimit(value) => Some(value),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_limit_is_not_a_limit() {
        assert_eq!(TimeLimit::seconds(0), None);
        assert_eq!(ByteLimit::bytes(0), None);
        assert_eq!(
            TimeLimit::seconds(600).and_then(TimeLimit::as_duration),
            Some(Duration::from_secs(600))
        );
        assert_eq!(TimeLimit::NoLimit.as_duration(), None);
    }

    #[test]
    fn every_type_round_trips_and_refuses_other_kinds() {
        fn check<T: SettingType + PartialEq + std::fmt::Debug>(value: T, other: SettingValue) {
            let wrapped = value.into_value();
            assert_eq!(wrapped.kind(), T::KIND);
            assert_eq!(T::from_value(wrapped), Some(value));
            assert_eq!(T::from_value(other), None);
        }
        check(true, SettingValue::Count(1));
        check(7_u32, SettingValue::Bool(true));
        check(TimeLimit::NoLimit, SettingValue::Count(1));
        check(ByteLimit::Unlimited, SettingValue::Bool(false));
    }

    #[test]
    fn a_byte_limit_becomes_the_driver_contract_buffer() {
        let bytes = NonZeroU32::new(20_000).expect("non-zero");
        assert_eq!(
            ServerOutputBuffer::from(ByteLimit::Bytes(bytes)),
            ServerOutputBuffer::Bytes(bytes)
        );
        assert_eq!(
            ServerOutputBuffer::from(ByteLimit::Unlimited),
            ServerOutputBuffer::Unlimited
        );
    }
}
