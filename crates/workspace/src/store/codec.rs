//! Row encodings: model values to SQLite columns and back.
//!
//! Every enumeration is written as a fixed lowercase word, never as a Rust
//! discriminant or a `Debug` rendering, so reordering an enum cannot change
//! what an existing file means. Decoding is strict: a word this build does
//! not know is an error the caller reports, never a guess.

use std::num::NonZeroU32;
use std::path::PathBuf;

use reldex_db_driver_api::SessionRole;

use crate::history::HistoryOutcome;
use crate::ids::ProfileId;
use crate::profile::{
    Authentication, DatabaseType, Environment, PasswordStorage, Profile, ProfileDetails,
    ProfileEndpoint, ServiceTarget, TlsOptions, Transport,
};
use crate::settings::{ByteLimit, EntryLimit, Level, SettingValue, TimeLimit, ValueKind};
use crate::time::UnixTimeMs;

pub(crate) const fn level_word(level: Level) -> &'static str {
    match level {
        Level::BuiltIn => "built_in",
        Level::Application => "application",
        Level::Profile => "profile",
        Level::Worksheet => "worksheet",
    }
}

pub(crate) const fn kind_word(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Bool => "bool",
        ValueKind::TimeLimit => "time_limit",
        ValueKind::Count => "count",
        ValueKind::ByteLimit => "byte_limit",
        ValueKind::EntryLimit => "entry_limit",
    }
}

fn kind_from_word(word: &str) -> Option<ValueKind> {
    Some(match word {
        "bool" => ValueKind::Bool,
        "time_limit" => ValueKind::TimeLimit,
        "count" => ValueKind::Count,
        "byte_limit" => ValueKind::ByteLimit,
        "entry_limit" => ValueKind::EntryLimit,
        _ => return None,
    })
}

/// A setting value as stored: its kind word and its integer column.
/// `NULL` is "no limit" / "unlimited" for the two limit kinds.
pub(crate) fn encode_value(value: SettingValue) -> (&'static str, Option<i64>) {
    let int = match value {
        SettingValue::Bool(on) => Some(i64::from(on)),
        SettingValue::Count(count) => Some(i64::from(count)),
        SettingValue::TimeLimit(TimeLimit::Seconds(seconds)) => Some(i64::from(seconds.get())),
        SettingValue::ByteLimit(ByteLimit::Bytes(bytes)) => Some(i64::from(bytes.get())),
        SettingValue::EntryLimit(EntryLimit::Count(count)) => Some(i64::from(count.get())),
        SettingValue::TimeLimit(TimeLimit::NoLimit)
        | SettingValue::ByteLimit(ByteLimit::Unlimited)
        | SettingValue::EntryLimit(EntryLimit::Unlimited) => None,
    };
    (kind_word(value.kind()), int)
}

/// Why a stored setting value could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodeValueError {
    UnknownKind(String),
    Malformed,
}

pub(crate) fn decode_value(
    kind: &str,
    int: Option<i64>,
    text: Option<&str>,
) -> Result<SettingValue, DecodeValueError> {
    let kind =
        kind_from_word(kind).ok_or_else(|| DecodeValueError::UnknownKind(kind.to_owned()))?;
    // No kind this build knows uses the text column.
    if text.is_some() {
        return Err(DecodeValueError::Malformed);
    }
    let non_zero = |value: i64| u32::try_from(value).ok().and_then(NonZeroU32::new);
    Ok(match (kind, int) {
        (ValueKind::Bool, Some(0)) => SettingValue::Bool(false),
        (ValueKind::Bool, Some(1)) => SettingValue::Bool(true),
        (ValueKind::Count, Some(count)) => {
            SettingValue::Count(u32::try_from(count).map_err(|_| DecodeValueError::Malformed)?)
        }
        (ValueKind::TimeLimit, None) => SettingValue::TimeLimit(TimeLimit::NoLimit),
        (ValueKind::TimeLimit, Some(seconds)) => SettingValue::TimeLimit(TimeLimit::Seconds(
            non_zero(seconds).ok_or(DecodeValueError::Malformed)?,
        )),
        (ValueKind::ByteLimit, None) => SettingValue::ByteLimit(ByteLimit::Unlimited),
        (ValueKind::ByteLimit, Some(bytes)) => SettingValue::ByteLimit(ByteLimit::Bytes(
            non_zero(bytes).ok_or(DecodeValueError::Malformed)?,
        )),
        (ValueKind::EntryLimit, None) => SettingValue::EntryLimit(EntryLimit::Unlimited),
        (ValueKind::EntryLimit, Some(count)) => SettingValue::EntryLimit(EntryLimit::Count(
            non_zero(count).ok_or(DecodeValueError::Malformed)?,
        )),
        _ => return Err(DecodeValueError::Malformed),
    })
}

/// A history entry's outcome as stored: its word and, for
/// [`HistoryOutcome::Failed`], the native error code column.
pub(crate) fn encode_history_outcome(outcome: &HistoryOutcome) -> (&'static str, Option<i64>) {
    match outcome {
        HistoryOutcome::Succeeded => ("succeeded", None),
        HistoryOutcome::Failed { native_code } => ("failed", native_code.map(i64::from)),
        HistoryOutcome::Cancelled => ("cancelled", None),
        HistoryOutcome::TimedOut => ("timed_out", None),
    }
}

/// Decodes a stored history outcome. The schema's own `CHECK` keeps
/// `native_code` `NULL` for every outcome but `'failed'`, so this only
/// rejects a word this build does not know — a file written by a newer
/// Reldex.
pub(crate) fn decode_history_outcome(
    word: &str,
    native_code: Option<i64>,
) -> Result<HistoryOutcome, String> {
    Ok(match word {
        "succeeded" => HistoryOutcome::Succeeded,
        "failed" => HistoryOutcome::Failed {
            native_code: native_code.map(|code| i32::try_from(code).unwrap_or(i32::MAX)),
        },
        "cancelled" => HistoryOutcome::Cancelled,
        "timed_out" => HistoryOutcome::TimedOut,
        _ => return Err(format!("outcome '{word}' is not one this build knows")),
    })
}

/// One `profile` row, column for column.
///
/// `Debug` prints the connect string's length only, like
/// [`ProfileEndpoint`]'s.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct ProfileRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) database_type: String,
    pub(crate) environment: String,
    pub(crate) environment_label: Option<String>,
    pub(crate) treat_as_production: i64,
    pub(crate) endpoint_kind: String,
    pub(crate) host: Option<String>,
    pub(crate) port: Option<i64>,
    pub(crate) service_name: Option<String>,
    pub(crate) sid: Option<String>,
    pub(crate) connect_string: Option<String>,
    pub(crate) auth_kind: String,
    pub(crate) username: Option<String>,
    pub(crate) password_in_credential_store: Option<i64>,
    pub(crate) role: String,
    pub(crate) transport: String,
    pub(crate) ca_directory: Option<String>,
    pub(crate) allow_unenforced_certificate_pin: i64,
    pub(crate) created_at: i64,
    pub(crate) modified_at: i64,
}

impl std::fmt::Debug for ProfileRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let connect_string = self
            .connect_string
            .as_ref()
            .map(|text| format!("<redacted, {} bytes>", text.len()));
        f.debug_struct("ProfileRow")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("database_type", &self.database_type)
            .field("environment", &self.environment)
            .field("environment_label", &self.environment_label)
            .field("treat_as_production", &self.treat_as_production)
            .field("endpoint_kind", &self.endpoint_kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("service_name", &self.service_name)
            .field("sid", &self.sid)
            .field("connect_string", &connect_string)
            .field("auth_kind", &self.auth_kind)
            .field("username", &self.username)
            .field(
                "password_in_credential_store",
                &self.password_in_credential_store,
            )
            .field("role", &self.role)
            .field("transport", &self.transport)
            .field("ca_directory", &self.ca_directory)
            .field(
                "allow_unenforced_certificate_pin",
                &self.allow_unenforced_certificate_pin,
            )
            .field("created_at", &self.created_at)
            .field("modified_at", &self.modified_at)
            .finish()
    }
}

/// The column list, in [`ProfileRow`] order, for `SELECT` and `INSERT`.
pub(crate) const PROFILE_COLUMNS: &str = "id, name, database_type, environment, \
     environment_label, treat_as_production, endpoint_kind, host, port, service_name, sid, \
     connect_string, auth_kind, username, password_in_credential_store, role, transport, \
     ca_directory, allow_unenforced_certificate_pin, created_at, modified_at";

impl ProfileRow {
    pub(crate) fn from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            database_type: row.get(2)?,
            environment: row.get(3)?,
            environment_label: row.get(4)?,
            treat_as_production: row.get(5)?,
            endpoint_kind: row.get(6)?,
            host: row.get(7)?,
            port: row.get(8)?,
            service_name: row.get(9)?,
            sid: row.get(10)?,
            connect_string: row.get(11)?,
            auth_kind: row.get(12)?,
            username: row.get(13)?,
            password_in_credential_store: row.get(14)?,
            role: row.get(15)?,
            transport: row.get(16)?,
            ca_directory: row.get(17)?,
            allow_unenforced_certificate_pin: row.get(18)?,
            created_at: row.get(19)?,
            modified_at: row.get(20)?,
        })
    }
}

/// Encodes a validated profile.
///
/// Returns `None` only for a CA directory that is not valid Unicode, which
/// validation has already refused.
pub(crate) fn encode_profile(profile: &Profile) -> Option<ProfileRow> {
    let details = profile.details();
    let mut row = ProfileRow {
        id: profile.id().to_string(),
        name: details.name.clone(),
        database_type: match details.database {
            DatabaseType::Oracle => "oracle",
        }
        .to_owned(),
        created_at: profile.created_at().as_millis(),
        modified_at: profile.modified_at().as_millis(),
        ..ProfileRow::default()
    };
    let (environment, label) = match &details.environment {
        Environment::Development => ("development", None),
        Environment::Test => ("test", None),
        Environment::Uat => ("uat", None),
        Environment::Staging => ("staging", None),
        Environment::Production => ("production", None),
        Environment::Custom(label) => ("custom", Some(label.clone())),
    };
    environment.clone_into(&mut row.environment);
    row.environment_label = label;
    row.treat_as_production = i64::from(details.treat_as_production);
    match &details.endpoint {
        ProfileEndpoint::HostPort { host, port, target } => {
            row.host = Some(host.clone());
            row.port = Some(i64::from(*port));
            match target {
                ServiceTarget::ServiceName(name) => {
                    "host_port_service".clone_into(&mut row.endpoint_kind);
                    row.service_name = Some(name.clone());
                }
                ServiceTarget::Sid(sid) => {
                    "host_port_sid".clone_into(&mut row.endpoint_kind);
                    row.sid = Some(sid.clone());
                }
            }
        }
        ProfileEndpoint::ConnectString(text) => {
            "connect_string".clone_into(&mut row.endpoint_kind);
            row.connect_string = Some(text.clone());
        }
    }
    match &details.authentication {
        Authentication::Password { username, storage } => {
            "password".clone_into(&mut row.auth_kind);
            row.username = Some(username.clone());
            row.password_in_credential_store = Some(match storage {
                PasswordStorage::CredentialStore => 1,
                PasswordStorage::PromptEachTime => 0,
            });
        }
        Authentication::External => "external".clone_into(&mut row.auth_kind),
    }
    match details.role {
        SessionRole::Normal => "normal",
        SessionRole::SysDba => "sysdba",
        SessionRole::SysOper => "sysoper",
    }
    .clone_into(&mut row.role);
    match details.tls.transport {
        Transport::Plain => "plain",
        Transport::Tls => "tls",
    }
    .clone_into(&mut row.transport);
    row.ca_directory = match &details.tls.ca_directory {
        Some(path) => Some(path.to_str()?.to_owned()),
        None => None,
    };
    row.allow_unenforced_certificate_pin = i64::from(details.tls.allow_unenforced_certificate_pin);
    Some(row)
}

fn required(value: Option<String>, column: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("{column} is missing"))
}

fn flag(value: i64, column: &str) -> Result<bool, String> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(format!("{column} is not 0 or 1")),
    }
}

/// Decodes a row, validating the result exactly as `Profile::create` would.
/// The error says which column was wrong, never what it held.
pub(crate) fn decode_profile(row: ProfileRow) -> Result<Profile, String> {
    let id = ProfileId::parse(&row.id).map_err(|error| format!("id: {error}"))?;
    let database = match row.database_type.as_str() {
        "oracle" => DatabaseType::Oracle,
        _ => return Err("database_type is not one this build knows".to_owned()),
    };
    let environment = match row.environment.as_str() {
        "development" => Environment::Development,
        "test" => Environment::Test,
        "uat" => Environment::Uat,
        "staging" => Environment::Staging,
        "production" => Environment::Production,
        "custom" => Environment::Custom(required(row.environment_label, "environment_label")?),
        _ => return Err("environment is not one this build knows".to_owned()),
    };
    let host_port = |target| -> Result<ProfileEndpoint, String> {
        let port = row
            .port
            .and_then(|port| u16::try_from(port).ok())
            .ok_or_else(|| "port is missing or out of range".to_owned())?;
        Ok(ProfileEndpoint::HostPort {
            host: required(row.host.clone(), "host")?,
            port,
            target,
        })
    };
    let endpoint = match row.endpoint_kind.as_str() {
        "host_port_service" => host_port(ServiceTarget::ServiceName(required(
            row.service_name.clone(),
            "service_name",
        )?))?,
        "host_port_sid" => host_port(ServiceTarget::Sid(required(row.sid.clone(), "sid")?))?,
        "connect_string" => {
            ProfileEndpoint::ConnectString(required(row.connect_string, "connect_string")?)
        }
        _ => return Err("endpoint_kind is not one this build knows".to_owned()),
    };
    let authentication = match row.auth_kind.as_str() {
        "password" => Authentication::Password {
            username: required(row.username, "username")?,
            storage: match row.password_in_credential_store {
                Some(1) => PasswordStorage::CredentialStore,
                Some(0) => PasswordStorage::PromptEachTime,
                _ => return Err("password_in_credential_store is not 0 or 1".to_owned()),
            },
        },
        "external" => Authentication::External,
        _ => return Err("auth_kind is not one this build knows".to_owned()),
    };
    let role = match row.role.as_str() {
        "normal" => SessionRole::Normal,
        "sysdba" => SessionRole::SysDba,
        "sysoper" => SessionRole::SysOper,
        _ => return Err("role is not one this build knows".to_owned()),
    };
    let transport = match row.transport.as_str() {
        "plain" => Transport::Plain,
        "tls" => Transport::Tls,
        _ => return Err("transport is not one this build knows".to_owned()),
    };
    let details = ProfileDetails {
        name: row.name,
        database,
        environment,
        treat_as_production: flag(row.treat_as_production, "treat_as_production")?,
        endpoint,
        authentication,
        role,
        tls: TlsOptions {
            transport,
            ca_directory: row.ca_directory.map(PathBuf::from),
            allow_unenforced_certificate_pin: flag(
                row.allow_unenforced_certificate_pin,
                "allow_unenforced_certificate_pin",
            )?,
        },
    };
    Profile::from_stored(
        id,
        details,
        UnixTimeMs::from_millis(row.created_at),
        UnixTimeMs::from_millis(row.modified_at),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SettingId;

    #[test]
    fn every_value_kind_round_trips_including_the_unlimited_forms() {
        let values = [
            SettingValue::Bool(true),
            SettingValue::Bool(false),
            SettingValue::Count(0),
            SettingValue::Count(u32::MAX),
            SettingValue::TimeLimit(TimeLimit::NoLimit),
            SettingValue::TimeLimit(TimeLimit::Seconds(NonZeroU32::MIN)),
            SettingValue::TimeLimit(TimeLimit::Seconds(NonZeroU32::MAX)),
            SettingValue::ByteLimit(ByteLimit::Unlimited),
            SettingValue::ByteLimit(ByteLimit::Bytes(NonZeroU32::MAX)),
            SettingValue::EntryLimit(EntryLimit::Unlimited),
            SettingValue::EntryLimit(EntryLimit::Count(NonZeroU32::MAX)),
        ];
        for value in values {
            let (kind, int) = encode_value(value);
            assert_eq!(decode_value(kind, int, None), Ok(value), "{value:?}");
        }
        // Every registered default, too.
        for id in SettingId::ALL {
            let value = id.descriptor().default_value();
            let (kind, int) = encode_value(value);
            assert_eq!(decode_value(kind, int, None), Ok(value), "{id}");
        }
    }

    #[test]
    fn malformed_stored_values_are_errors_not_guesses() {
        assert_eq!(
            decode_value("colour", Some(1), None),
            Err(DecodeValueError::UnknownKind("colour".to_owned()))
        );
        for (kind, int) in [
            ("bool", Some(2)),
            ("bool", None),
            ("count", None),
            ("count", Some(-1)),
            ("count", Some(i64::from(u32::MAX) + 1)),
            ("time_limit", Some(0)),
            ("time_limit", Some(-5)),
            ("byte_limit", Some(0)),
            ("entry_limit", Some(0)),
        ] {
            assert_eq!(
                decode_value(kind, int, None),
                Err(DecodeValueError::Malformed),
                "{kind} {int:?}"
            );
        }
        assert_eq!(
            decode_value("bool", Some(1), Some("x")),
            Err(DecodeValueError::Malformed)
        );
    }

    #[test]
    fn every_history_outcome_round_trips() {
        for outcome in [
            HistoryOutcome::Succeeded,
            HistoryOutcome::Failed { native_code: None },
            HistoryOutcome::Failed {
                native_code: Some(1017),
            },
            HistoryOutcome::Cancelled,
            HistoryOutcome::TimedOut,
        ] {
            let (word, native_code) = encode_history_outcome(&outcome);
            assert_eq!(decode_history_outcome(word, native_code), Ok(outcome));
        }
    }

    #[test]
    fn an_unknown_outcome_word_is_an_error() {
        assert_eq!(
            decode_history_outcome("retried", None),
            Err("outcome 'retried' is not one this build knows".to_owned())
        );
    }
}
